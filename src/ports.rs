//! Portas TCP em escuta e a sondagem HTTP de cada uma.
//!
//! A tabela de portas vem do IP Helper (`GetExtendedTcpTable`) e é lida pela thread de
//! coleta junto com os processos. A sondagem HTTP roda numa thread própria e só com a
//! tela de portas aberta: uma requisição pode esperar até o timeout, e a coleta não
//! pode atrasar por causa disso.

use std::collections::{BTreeMap, HashMap};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ops::RangeInclusive;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
    TCP_TABLE_CLASS, TCP_TABLE_OWNER_PID_CONNECTIONS, TCP_TABLE_OWNER_PID_LISTENER,
};

use crate::sampler::{ProcInfo, Series};

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;

/// Faixas padrão: as portas que o Vite e o uvicorn escolhem quando a anterior está ocupada.
pub const DEFAULT_RANGES: &str = "5173-5183, 8000-8010";
/// Uma faixa vira uma linha de chips na tela; mais que isso não cabe nem ajuda.
const MAX_RANGE_LEN: u16 = 64;

/// A cada rodada só se abre e fecha a conexão TCP: diz se o servidor está de pé sem
/// aparecer no log dele. O GET completo, que o uvicorn, o Flask e o Next registram
/// linha a linha, fica para a primeira vez e a cada [`FULL_INTERVAL`].
const ROUND_INTERVAL: Duration = Duration::from_secs(2);
const FULL_INTERVAL: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Folgado de propósito: um servidor parado há dias tem a memória paginada para o disco
/// e leva alguns segundos na primeira resposta. Com 1,5 s ele aparecia como calado.
const READ_TIMEOUT: Duration = Duration::from_secs(4);
/// O `<title>` fica no começo da página; não vale ler o resto.
const PAGE_LIMIT: usize = 32 * 1024;
const API_LIMIT: usize = 128 * 1024;

/// Uma porta TCP em escuta e o processo dono dela.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub pid: u32,
    /// Endereços locais em que a porta escuta (`0.0.0.0`, `::1`…), sem repetição.
    pub addrs: Vec<IpAddr>,
}

impl Listener {
    /// Escuta em alguma interface além da loopback: outras máquinas da rede alcançam.
    pub fn exposed(&self) -> bool {
        self.addrs.iter().any(|a| !a.is_loopback())
    }

    /// Endereço para conectar. A porta pode escutar só em IPv6 (o Vite no Windows
    /// escuta em `::1`), e aí uma conexão em `127.0.0.1` é recusada.
    pub fn connect_addr(&self) -> SocketAddr {
        let local = |a: &&IpAddr| a.is_unspecified() || a.is_loopback();
        let ip = if self.addrs.iter().filter(local).any(|a| a.is_ipv4()) {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else if self.addrs.iter().filter(local).any(|a| a.is_ipv6()) {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        } else {
            self.addrs.first().copied().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
        };
        SocketAddr::new(ip, self.port)
    }

    /// URL para o navegador. `localhost` resolve tanto para `127.0.0.1` quanto para `::1`.
    pub fn url(&self) -> String {
        let addr = self.connect_addr();
        if addr.ip().is_loopback() { format!("http://localhost:{}", self.port) } else { format!("http://{addr}") }
    }

    /// Endereços para exibir: `::1`, `127.0.0.1`, `0.0.0.0, ::`.
    pub fn addrs_text(&self) -> String {
        self.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
    }
}

/// Portas TCP em escuta, uma entrada por porta e processo, ordenadas pela porta.
pub fn listeners() -> Vec<Listener> {
    let mut by_key: BTreeMap<(u16, u32), Vec<IpAddr>> = BTreeMap::new();
    let mut add = |port: u16, pid: u32, ip: IpAddr| {
        let addrs = by_key.entry((port, pid)).or_default();
        if !addrs.contains(&ip) {
            addrs.push(ip);
        }
    };
    if let Some(buf) = tcp_table(AF_INET, TCP_TABLE_OWNER_PID_LISTENER) {
        let offset = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        for row in unsafe { table_rows::<MIB_TCPROW_OWNER_PID>(&buf, offset) } {
            add(port_of(row.dwLocalPort), row.dwOwningPid, ipv4_of(row.dwLocalAddr));
        }
    }
    if let Some(buf) = tcp_table(AF_INET6, TCP_TABLE_OWNER_PID_LISTENER) {
        let offset = std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table);
        for row in unsafe { table_rows::<MIB_TCP6ROW_OWNER_PID>(&buf, offset) } {
            add(port_of(row.dwLocalPort), row.dwOwningPid, IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)));
        }
    }
    by_key.into_iter().map(|((port, pid), addrs)| Listener { port, pid, addrs }).collect()
}

/// Estado de uma conexão TCP, como o IP Helper devolve (`MIB_TCP_STATE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    SynSent,
    SynReceived,
    Established,
    /// Este lado já fechou e espera o outro (FIN_WAIT_1/2, CLOSING, LAST_ACK).
    Closing,
    /// O outro lado fechou e este processo ainda não: muitas indicam vazamento de socket.
    CloseWait,
    TimeWait,
    Other,
}

impl TcpState {
    fn from_raw(raw: u32) -> Self {
        match raw {
            3 => TcpState::SynSent,
            4 => TcpState::SynReceived,
            5 => TcpState::Established,
            6 | 7 | 9 | 10 => TcpState::Closing,
            8 => TcpState::CloseWait,
            11 => TcpState::TimeWait,
            _ => TcpState::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TcpState::SynSent => "Conectando",
            TcpState::SynReceived => "Aceitando",
            TcpState::Established => "Estabelecida",
            TcpState::Closing => "Fechando",
            TcpState::CloseWait => "Aguardando fechamento",
            TcpState::TimeWait => "Encerrada há pouco",
            TcpState::Other => "—",
        }
    }
}

/// Uma conexão TCP que não é de escuta, com o processo dono.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub pid: u32,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub state: TcpState,
}

/// Linha 2 (LISTEN) já aparece em [`listeners`]; 1 (CLOSED) e 12 (DELETE_TCB) não interessam.
fn is_connection(state: u32) -> bool {
    !matches!(state, 1 | 2 | 12)
}

/// Conexões TCP de todos os processos (IPv4 e IPv6), sem as portas em escuta.
pub fn connections() -> Vec<Connection> {
    let mut out = Vec::new();
    if let Some(buf) = tcp_table(AF_INET, TCP_TABLE_OWNER_PID_CONNECTIONS) {
        let offset = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        for row in unsafe { table_rows::<MIB_TCPROW_OWNER_PID>(&buf, offset) } {
            if is_connection(row.dwState) {
                out.push(Connection {
                    pid: row.dwOwningPid,
                    local: SocketAddr::new(ipv4_of(row.dwLocalAddr), port_of(row.dwLocalPort)),
                    remote: SocketAddr::new(ipv4_of(row.dwRemoteAddr), port_of(row.dwRemotePort)),
                    state: TcpState::from_raw(row.dwState),
                });
            }
        }
    }
    if let Some(buf) = tcp_table(AF_INET6, TCP_TABLE_OWNER_PID_CONNECTIONS) {
        let offset = std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table);
        for row in unsafe { table_rows::<MIB_TCP6ROW_OWNER_PID>(&buf, offset) } {
            if is_connection(row.dwState) {
                out.push(Connection {
                    pid: row.dwOwningPid,
                    local: SocketAddr::new(IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)), port_of(row.dwLocalPort)),
                    remote: SocketAddr::new(IpAddr::V6(Ipv6Addr::from(row.ucRemoteAddr)), port_of(row.dwRemotePort)),
                    state: TcpState::from_raw(row.dwState),
                });
            }
        }
    }
    out
}

/// O endereço está na ordem da rede: os bytes na memória já são os octetos.
fn ipv4_of(raw: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(raw.to_ne_bytes()))
}

/// A porta vem na ordem da rede, nos 16 bits de baixo.
fn port_of(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// Tabela TCP de uma família de endereços. Buffer de `u32` para ficar alinhado.
fn tcp_table(family: u32, class: TCP_TABLE_CLASS) -> Option<Vec<u32>> {
    let mut buf = vec![0u32; 2048];
    // A tabela pode crescer entre a consulta do tamanho e a leitura: tenta de novo.
    for _ in 0..4 {
        let mut size = (buf.len() * 4) as u32;
        let ret = unsafe {
            GetExtendedTcpTable(Some(buf.as_mut_ptr().cast()), &mut size, false, family, class, 0)
        };
        if ret == NO_ERROR.0 {
            return Some(buf);
        }
        if ret != ERROR_INSUFFICIENT_BUFFER.0 {
            return None;
        }
        buf = vec![0u32; (size as usize).div_ceil(4) + 64];
    }
    None
}

/// Linhas de uma tabela do IP Helper: `dwNumEntries` seguido das linhas a partir de `offset`.
unsafe fn table_rows<R: Copy>(buf: &[u32], offset: usize) -> Vec<R> {
    let declared = buf.first().copied().unwrap_or(0) as usize;
    // A contagem vem do sistema, mas o buffer é nosso: nunca ler além dele.
    let fits = (buf.len() * 4).saturating_sub(offset) / size_of::<R>();
    let first = unsafe { buf.as_ptr().cast::<u8>().add(offset).cast::<R>() };
    (0..declared.min(fits)).map(|i| unsafe { first.add(i).read_unaligned() }).collect()
}

// ── Classificação ──────────────────────────────────────────────────────────

/// Plataforma de desenvolvimento que escuta a porta, pelo executável e pela linha de comando.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Vite,
    Next,
    Node,
    Python,
    Dotnet,
    Java,
    Deno,
    Bun,
    Rust,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Vite => "Vite",
            Kind::Next => "Next.js",
            Kind::Node => "Node",
            Kind::Python => "Python",
            Kind::Dotnet => ".NET",
            Kind::Java => "Java",
            Kind::Deno => "Deno",
            Kind::Bun => "Bun",
            Kind::Rust => "Rust",
        }
    }
}

/// Executáveis de servidores Python que o venv instala como `.exe` próprio.
const PYTHON_SERVERS: &[&str] = &["uvicorn", "gunicorn", "hypercorn", "granian", "flask", "fastapi", "streamlit"];

/// Indícios, na linha de comando, de que um processo Python serve HTTP.
const PYTHON_HTTP_HINTS: &[&str] = &[
    "uvicorn", "gunicorn", "hypercorn", "granian", "daphne", "waitress", "flask", "django", "manage.py", "fastapi",
    "streamlit", "gradio", "http.server", "mkdocs",
];

/// `None` quando o processo não é de uma plataforma de desenvolvimento conhecida.
pub fn classify(p: &ProcInfo) -> Option<Kind> {
    let name = p.name.to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    let kind = match stem {
        "node" => {
            let cmd = normalized(&p.cmd);
            if cmd.contains("\\node_modules\\next\\") {
                Kind::Next
            } else if cmd.contains("vite") {
                Kind::Vite
            } else {
                Kind::Node
            }
        }
        "deno" => Kind::Deno,
        "bun" => Kind::Bun,
        "dotnet" => Kind::Dotnet,
        "java" | "javaw" => Kind::Java,
        s if s.starts_with("python") || s == "py" || PYTHON_SERVERS.contains(&s) => Kind::Python,
        // Executável compilado dentro da pasta de build de um projeto.
        _ => {
            let exe = p.exe.as_deref().map(|e| normalized(&e.to_string_lossy())).unwrap_or_default();
            if exe.contains("\\bin\\debug\\") || exe.contains("\\bin\\release\\") {
                Kind::Dotnet
            } else if exe.contains("\\target\\debug\\") || exe.contains("\\target\\release\\") {
                Kind::Rust
            } else {
                return None;
            }
        }
    };
    Some(kind)
}

/// Se vale mandar um GET para as portas do processo. Python só com indício de servidor
/// web: um kernel do Jupyter ou um depurador escutam portas que não falam HTTP.
pub fn likely_http(kind: Kind, cmd: &str) -> bool {
    if kind != Kind::Python {
        return true;
    }
    let cmd = cmd.to_ascii_lowercase();
    !cmd.contains("ipykernel") && PYTHON_HTTP_HINTS.iter().any(|h| cmd.contains(h))
}

/// Processo que reiniciar para trazer de volta o servidor de `p`: sobe pelos pais
/// enquanto forem de desenvolvimento, atravessando o `cmd.exe` que o npm usa entre ele
/// e o Vite (`npm run dev` → `cmd /c vite` → `node vite`). Para no shell ou terminal
/// do usuário, que não pode ser finalizado junto.
pub fn server_root<'a>(p: &'a ProcInfo, parent_of: impl Fn(&ProcInfo) -> Option<&'a ProcInfo>) -> &'a ProcInfo {
    let mut root = p;
    // O limite só protege de um laço de PIDs; uma cadeia real tem 3 ou 4 níveis.
    for _ in 0..16 {
        let Some(parent) = parent_of(root) else { break };
        if classify(parent).is_some() {
            root = parent;
        } else if parent.name.eq_ignore_ascii_case("cmd.exe")
            && let Some(grand) = parent_of(parent)
            && classify(grand).is_some()
        {
            root = grand;
        } else {
            break;
        }
    }
    root
}

/// Pastas que marcam o fim do caminho do projeto.
const PROJECT_MARKERS: &[&str] =
    &["\\node_modules\\", "\\.venv\\", "\\venv\\", "\\bin\\debug\\", "\\bin\\release\\", "\\target\\debug\\", "\\target\\release\\"];

/// Nomes de pasta que sozinhos não identificam o projeto (`frontend` de quê?).
const GENERIC_DIRS: &[&str] =
    &["frontend", "backend", "front", "back", "src", "app", "web", "client", "server", "ui", "api", "renderer", "packages", "apps", "site"];

/// Pasta do projeto: o que vem antes de `node_modules`, `.venv` ou da pasta de build, no
/// executável ou na linha de comando. Sem nenhum desses, o diretório de trabalho.
pub fn project_dir(p: &ProcInfo) -> Option<PathBuf> {
    let exe = p.exe.as_ref().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    dir_before_marker(&exe).or_else(|| dir_before_marker(&p.cmd)).map(PathBuf::from).or_else(|| p.cwd.clone())
}

fn dir_before_marker(text: &str) -> Option<&str> {
    // Minúsculas ASCII e barras trocadas não mudam o tamanho: os índices valem no original.
    let lower = normalized(text);
    let at = PROJECT_MARKERS.iter().filter_map(|m| lower.find(m)).min()?;
    // O caminho começa na última letra de unidade antes do marcador, sem atravessar aspas.
    let start = lower[..at].rfind('"').map_or(0, |i| i + 1);
    let colon = start + lower[start..at].rfind(":\\")?;
    let drive = colon.checked_sub(1)?;
    lower.as_bytes()[drive].is_ascii_alphabetic().then(|| &text[drive..at])
}

/// Nome curto do projeto: a última pasta, ou as duas últimas se a última for genérica.
pub fn project_label(dir: &Path) -> Option<String> {
    let mut names = dir.components().rev().filter_map(|c| match c {
        Component::Normal(s) => Some(s.to_string_lossy()),
        _ => None,
    });
    let last = names.next()?;
    if GENERIC_DIRS.iter().any(|g| last.eq_ignore_ascii_case(g))
        && let Some(parent) = names.next()
    {
        return Some(format!("{parent}\\{last}"));
    }
    Some(last.into_owned())
}

fn normalized(s: &str) -> String {
    s.to_ascii_lowercase().replace('/', "\\")
}

// ── Faixas ─────────────────────────────────────────────────────────────────

pub type PortRange = RangeInclusive<u16>;

/// `"5173-5183, 8000-8010, 3000"` → faixas. O erro diz qual trecho não foi entendido.
pub fn parse_ranges(text: &str) -> Result<Vec<PortRange>, String> {
    let mut ranges = Vec::new();
    for part in text.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once(['-', '–']).unwrap_or((part, part));
        let parse = |s: &str| s.trim().parse::<u16>().ok().filter(|&p| p > 0);
        let (Some(a), Some(b)) = (parse(a), parse(b)) else {
            return Err(format!("“{part}” não é uma porta nem uma faixa"));
        };
        let (lo, hi) = (a.min(b), a.max(b));
        if hi - lo >= MAX_RANGE_LEN {
            return Err(format!("“{part}” tem mais de {MAX_RANGE_LEN} portas"));
        }
        ranges.push(lo..=hi);
    }
    Ok(ranges)
}

pub fn in_ranges(ranges: &[PortRange], port: u16) -> bool {
    ranges.iter().any(|r| r.contains(&port))
}

// ── Sondagem HTTP ──────────────────────────────────────────────────────────

/// Uma porta a sondar. `api`: tentar `/openapi.json` quando a página não tiver título.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub port: u16,
    pub pid: u32,
    pub addr: SocketAddr,
    pub api: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reply {
    /// Respondeu HTTP: código e tempo até o primeiro byte, em ms.
    Http { status: u16, ms: f32 },
    /// Aceitou a conexão, mas não respondeu HTTP (banco de dados, websocket puro…).
    NotHttp,
    /// Aceitou e ficou calado até o timeout.
    Timeout,
    Refused,
}

impl Reply {
    /// O servidor está de pé. Um 404 na raiz é normal numa API.
    pub fn healthy(self) -> bool {
        matches!(self, Reply::Http { status, .. } if status < 500)
    }
}

/// O que se sabe de uma porta sondada.
#[derive(Clone, Debug, Default)]
pub struct PortStatus {
    pub reply: Option<Reply>,
    /// Tempo de conexão TCP nas últimas rodadas (0 quando não conectou).
    pub latency: Series,
    page_title: Option<String>,
    api_title: Option<String>,
    api_checked: bool,
    /// Quando foi o último GET completo.
    last_full: Option<Instant>,
}

impl PortStatus {
    /// `<title>` da página inicial ou, numa API, o título e a versão do OpenAPI.
    pub fn title(&self) -> Option<&str> {
        self.page_title.as_deref().or(self.api_title.as_deref())
    }

    fn apply(&mut self, probe: Probe) {
        self.latency.push(probe.connect_ms.unwrap_or(0.0));
        if probe.full {
            self.last_full = Some(Instant::now());
        }
        // `None`: só conectou e o servidor segue de pé; vale a última resposta HTTP.
        let Some(reply) = probe.reply else { return };
        // Fora do ar, o título anterior continua: ainda é o que a porta servia.
        if matches!(reply, Reply::Http { .. }) {
            self.page_title = probe.page_title;
        }
        if let Some(api) = probe.api_title {
            self.api_checked = true;
            self.api_title = api;
        }
        self.reply = Some(reply);
    }

    /// Vale um GET completo: porta nova, que voltou depois de cair, ou já se passou
    /// [`FULL_INTERVAL`] desde o último.
    fn wants_full(&self) -> bool {
        let up = matches!(self.reply, Some(Reply::Http { .. } | Reply::NotHttp));
        !up || self.last_full.is_none_or(|t| t.elapsed() >= FULL_INTERVAL)
    }
}

pub type Statuses = HashMap<(u16, u32), PortStatus>;

enum Msg {
    Targets(Vec<Target>),
    Active(bool),
}

/// Thread que sonda as portas a cada [`ROUND_INTERVAL`] enquanto estiver ativa.
pub struct Prober {
    latest: Arc<Mutex<Arc<Statuses>>>,
    tx: Sender<Msg>,
}

impl Prober {
    pub fn spawn(ctx: egui::Context) -> Self {
        let latest = Arc::new(Mutex::new(Arc::new(Statuses::new())));
        let (tx, rx) = mpsc::channel();
        let shared = latest.clone();
        std::thread::Builder::new()
            .name("ports".into())
            .spawn(move || run(ctx, shared, rx))
            .expect("thread de portas");
        Self { latest, tx }
    }

    pub fn latest(&self) -> Arc<Statuses> {
        self.latest.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_targets(&self, targets: Vec<Target>) {
        let _ = self.tx.send(Msg::Targets(targets));
    }

    /// Sem a tela de portas aberta, ninguém olha o resultado: a thread dorme.
    pub fn set_active(&self, active: bool) {
        let _ = self.tx.send(Msg::Active(active));
    }
}

fn run(ctx: egui::Context, latest: Arc<Mutex<Arc<Statuses>>>, rx: Receiver<Msg>) {
    let mut targets: Vec<Target> = Vec::new();
    let mut active = false;
    let mut statuses = Statuses::new();
    let mut next = Instant::now();
    let publish = |statuses: &Statuses| {
        if let Ok(mut slot) = latest.lock() {
            *slot = Arc::new(statuses.clone());
        }
        ctx.request_repaint();
    };
    loop {
        let wait = if active && !targets.is_empty() {
            next.saturating_duration_since(Instant::now())
        } else {
            Duration::from_secs(3600)
        };
        match rx.recv_timeout(wait) {
            Ok(Msg::Targets(list)) => {
                // Porta nova é sondada já; as que fecharam saem do mapa.
                if list.iter().any(|t| !statuses.contains_key(&(t.port, t.pid))) {
                    next = Instant::now();
                }
                let before = statuses.len();
                statuses.retain(|key, _| list.iter().any(|t| (t.port, t.pid) == *key));
                if statuses.len() != before {
                    publish(&statuses);
                }
                targets = list;
                continue;
            }
            Ok(Msg::Active(on)) => {
                if on && !active {
                    next = Instant::now();
                }
                active = on;
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !active || targets.is_empty() {
            continue;
        }
        let probes = probe_all(&targets, &statuses);
        for (target, probe) in targets.iter().zip(probes) {
            statuses.entry((target.port, target.pid)).or_default().apply(probe);
        }
        publish(&statuses);
        next = Instant::now() + ROUND_INTERVAL;
    }
}

struct Probe {
    /// `None` numa rodada só de conexão que deu certo: nada novo sobre o HTTP.
    reply: Option<Reply>,
    connect_ms: Option<f32>,
    /// Foi um GET completo (conta para o intervalo de [`FULL_INTERVAL`]).
    full: bool,
    page_title: Option<String>,
    /// `Some` quando o OpenAPI foi consultado nesta rodada (achando título ou não).
    api_title: Option<Option<String>>,
}

impl Probe {
    fn failed(reply: Reply) -> Self {
        Self { reply: Some(reply), connect_ms: None, full: false, page_title: None, api_title: None }
    }
}

/// Todas em paralelo: uma porta calada não pode segurar as outras até o timeout.
fn probe_all(targets: &[Target], statuses: &Statuses) -> Vec<Probe> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|t| {
                let status = statuses.get(&(t.port, t.pid));
                let full = status.is_none_or(PortStatus::wants_full);
                let want_api = t.api && status.is_none_or(|s| !s.api_checked);
                scope.spawn(move || if full { probe(t, want_api) } else { check(t) })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(Probe::failed(Reply::NotHttp))).collect()
    })
}

/// Só abre e fecha a conexão: o servidor nem chega a ver uma requisição.
fn check(t: &Target) -> Probe {
    match connect(t.addr) {
        Ok((_, ms)) => Probe { reply: None, connect_ms: Some(ms), full: false, page_title: None, api_title: None },
        Err(reply) => Probe::failed(reply),
    }
}

fn probe(t: &Target, want_api: bool) -> Probe {
    match get(t.addr, "/", PAGE_LIMIT) {
        Ok(page) => {
            let page_title = html_title(&page.body);
            let api_title = (want_api && page_title.is_none()).then(|| {
                get(t.addr, "/openapi.json", API_LIMIT)
                    .ok()
                    .filter(|r| r.status == 200)
                    .and_then(|r| openapi_title(&String::from_utf8_lossy(&r.body)))
            });
            Probe {
                reply: Some(Reply::Http { status: page.status, ms: page.ms }),
                connect_ms: Some(page.connect_ms),
                full: true,
                page_title,
                api_title,
            }
        }
        Err(reply) => Probe { full: true, ..Probe::failed(reply) },
    }
}

struct Response {
    status: u16,
    ms: f32,
    connect_ms: f32,
    body: Vec<u8>,
}

/// Conexão TCP com timeout, e quanto ela levou em ms.
fn connect(addr: SocketAddr) -> Result<(TcpStream, f32), Reply> {
    let started = Instant::now();
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| match e.kind() {
        ErrorKind::TimedOut => Reply::Timeout,
        _ => Reply::Refused,
    })?;
    Ok((stream, started.elapsed().as_secs_f32() * 1000.0))
}

/// GET mínimo em HTTP/1.1, sem dependências. O tempo medido é até o primeiro byte.
fn get(addr: SocketAddr, path: &str, limit: usize) -> Result<Response, Reply> {
    let started = Instant::now();
    let (mut stream, connect_ms) = connect(addr)?;
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    // `localhost` no Host: o Vite recusa nomes que não estão em `allowedHosts`.
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost:{}\r\nUser-Agent: ProcessMonitor\r\nAccept: text/html, application/json, */*\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Err(Reply::NotHttp);
    }
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut first_byte = None;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                first_byte.get_or_insert_with(|| started.elapsed());
                data.extend_from_slice(&chunk[..n]);
                if data.len() >= limit || complete(&data) {
                    break;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if data.is_empty() {
                    return Err(Reply::Timeout);
                }
                break;
            }
            Err(_) => break,
        }
    }
    let status = parse_status(&data).ok_or(Reply::NotHttp)?;
    let body = find(&data, b"\r\n\r\n").map(|i| data[i + 4..].to_vec()).unwrap_or_default();
    Ok(Response { status, ms: first_byte.unwrap_or_default().as_secs_f32() * 1000.0, connect_ms, body })
}

/// Já chegou o que interessa: o corpo inteiro (pelo `Content-Length`) ou o fim do `<title>`.
fn complete(data: &[u8]) -> bool {
    let Some(end) = find(data, b"\r\n\r\n") else { return false };
    let headers = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
    let body = &data[end + 4..];
    let length = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    length.is_some_and(|n| body.len() >= n) || find(&body.to_ascii_lowercase(), b"</title").is_some()
}

/// `HTTP/1.1 200 OK` → 200.
fn parse_status(data: &[u8]) -> Option<u16> {
    let line = data.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    parts.next()?.starts_with("HTTP/").then_some(())?;
    parts.next()?.parse().ok().filter(|s| (100..600).contains(s))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Texto do `<title>`, com as entidades comuns decodificadas e os espaços normalizados.
fn html_title(body: &[u8]) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let open = find(&lower, b"<title")?;
    let start = open + lower[open..].iter().position(|&b| b == b'>')? + 1;
    let end = start + find(&lower[start..], b"</title")?;
    let raw = String::from_utf8_lossy(&body[start..end]);
    let decoded = raw
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    let text = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| text.chars().take(120).collect())
}

/// Título e versão de `info` num documento OpenAPI (`"Minha API 1.2.0"`), sem um parser
/// JSON completo: percorre só o objeto `info`, contando chaves e respeitando strings.
fn openapi_title(doc: &str) -> Option<String> {
    let at = doc.find("\"info\"")?;
    let rest = doc[at + 6..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('{')?;
    let (mut title, mut version) = (None, None);
    // `key`: chave do valor que vem a seguir; `last`: string que pode virar chave.
    let (mut depth, mut key, mut last) = (1usize, None::<String>, None::<String>);
    let mut i = 0;
    while let Some(c) = rest[i..].chars().next() {
        if c == '"' {
            let (s, len) = json_string(&rest[i..])?;
            i += len;
            if depth == 1 {
                match key.take().as_deref() {
                    Some("title") => title = Some(s),
                    Some("version") => version = Some(s),
                    Some(_) => {}
                    None => last = Some(s),
                }
            }
            continue;
        }
        match c {
            ':' if depth == 1 => key = last.take(),
            ',' if depth == 1 => (key, last) = (None, None),
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += c.len_utf8();
    }
    let title = title.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())?;
    Some(match version.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
        Some(v) => format!("{title} {v}"),
        None => title,
    })
}

/// String JSON a partir das aspas de abertura: o texto e quantos bytes ela ocupa.
fn json_string(s: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut chars = s.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, i + 1)),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' | 'f' => {}
                'u' => {
                    let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                    out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}'));
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::tests::proc;
    use std::net::TcpListener;

    fn node(cmd: &str) -> ProcInfo {
        let mut p = proc(1, None, 1, "node.exe");
        p.cmd = cmd.to_owned();
        p
    }

    #[test]
    fn classifica_pelo_executavel_e_comando() {
        let vite = node(r#""node"   "C:\Users\ana\proj\frontend\node_modules\.bin\\..\vite\bin\vite.js""#);
        assert_eq!(classify(&vite), Some(Kind::Vite));
        let next = node(r"node C:\app\node_modules\next\dist\server\lib\start-server.js");
        assert_eq!(classify(&next), Some(Kind::Next));
        assert_eq!(classify(&node("node server.js")), Some(Kind::Node));
        assert_eq!(classify(&proc(1, None, 1, "Python.exe")), Some(Kind::Python));
        assert_eq!(classify(&proc(1, None, 1, "uvicorn.exe")), Some(Kind::Python));
        assert_eq!(classify(&proc(1, None, 1, "svchost.exe")), None);
        let mut api = proc(1, None, 1, "SimPipeCloud.exe");
        api.exe = Some(PathBuf::from(r"C:\src\Cloud\SimPipeCloud\bin\Debug\net8.0\SimPipeCloud.exe"));
        assert_eq!(classify(&api), Some(Kind::Dotnet));
    }

    #[test]
    fn python_so_e_sondado_com_indicio_de_servidor_web() {
        assert!(likely_http(Kind::Python, r"python.exe -m uvicorn main:app --reload --port 8001"));
        assert!(!likely_http(Kind::Python, r"python.exe -m ipykernel_launcher --f=C:\x\jupyter\runtime\kernel.json"));
        assert!(!likely_http(Kind::Python, r"python.exe -m debugpy --listen 5678 script.py"));
        assert!(likely_http(Kind::Node, "node qualquer.js"));
    }

    #[test]
    fn pasta_e_nome_do_projeto() {
        let vite = node(r#""node"   "C:\Users\ana\orca\AlertaEmail\frontend\node_modules\.bin\\..\vite\bin\vite.js""#);
        let dir = project_dir(&vite).unwrap();
        assert_eq!(dir, PathBuf::from(r"C:\Users\ana\orca\AlertaEmail\frontend"));
        assert_eq!(project_label(&dir).as_deref(), Some(r"AlertaEmail\frontend"));

        // Sem aspas e com o executável do node antes: vale a última unidade antes do marcador.
        let plain = node(r"C:\nodejs\node.exe C:\proj\loja\node_modules\vite\bin\vite.js");
        assert_eq!(project_dir(&plain), Some(PathBuf::from(r"C:\proj\loja")));

        let mut venv = proc(1, None, 1, "python.exe");
        venv.exe = Some(PathBuf::from(r"C:\apis\cotacoes\.venv\Scripts\python.exe"));
        assert_eq!(project_dir(&venv).and_then(|d| project_label(&d)).as_deref(), Some("cotacoes"));

        // Sem marcador, o diretório de trabalho.
        let mut uvicorn = proc(1, None, 1, "python.exe");
        uvicorn.cmd = "python.exe -m uvicorn api.main:app --reload".to_owned();
        uvicorn.cwd = Some(PathBuf::from(r"C:\apis\estoque"));
        assert_eq!(project_dir(&uvicorn), Some(PathBuf::from(r"C:\apis\estoque")));
    }

    #[test]
    fn faixas_de_portas() {
        assert_eq!(parse_ranges("5173-5183, 8000–8010; 3000"), Ok(vec![5173..=5183, 8000..=8010, 3000..=3000]));
        // Invertida, a faixa é desvirada.
        assert_eq!(parse_ranges(" 9010-9000 ").unwrap().first(), Some(&(9000..=9010)));
        assert_eq!(parse_ranges(""), Ok(vec![]));
        assert!(parse_ranges("1-65535").is_err());
        assert!(parse_ranges("abc").is_err());
        assert!(parse_ranges("0").is_err());
        assert!(in_ranges(&parse_ranges(DEFAULT_RANGES).unwrap(), 5173));
    }

    #[test]
    fn endereco_para_conectar_respeita_a_familia() {
        let v6 = Listener { port: 5173, pid: 1, addrs: vec!["::1".parse().unwrap()] };
        assert_eq!(v6.connect_addr(), "[::1]:5173".parse().unwrap());
        assert_eq!(v6.url(), "http://localhost:5173");
        assert!(!v6.exposed());
        let all = Listener { port: 80, pid: 1, addrs: vec!["0.0.0.0".parse().unwrap(), "::".parse().unwrap()] };
        assert_eq!(all.connect_addr(), "127.0.0.1:80".parse().unwrap());
        assert!(all.exposed());
        let lan = Listener { port: 139, pid: 4, addrs: vec!["192.168.1.17".parse().unwrap()] };
        assert_eq!(lan.url(), "http://192.168.1.17:139");
    }

    #[test]
    fn titulo_da_pagina() {
        let page = b"HTTP/1.1 200 OK\r\n\r\n<html><head><TITLE>\n  Loja &amp; Estoque  </TITLE></head>";
        assert_eq!(parse_status(page), Some(200));
        let body = &page[find(page, b"\r\n\r\n").unwrap() + 4..];
        assert_eq!(html_title(body).as_deref(), Some("Loja & Estoque"));
        assert_eq!(html_title(b"<html></html>"), None);
        assert_eq!(parse_status(b"SSH-2.0-OpenSSH"), None);
    }

    #[test]
    fn titulo_do_openapi() {
        let doc = r#"{"openapi":"3.1.0","info":{"title":"Cota\u00e7\u00f5es \"BR\"","contact":{"title":"não é este"},"version":"1.8.64"},"paths":{"/x":{"get":{"title":"nem este"}}}}"#;
        assert_eq!(openapi_title(doc).as_deref(), Some("Cotações \"BR\" 1.8.64"));
        assert_eq!(openapi_title(r#"{"info":{"version":"1"}}"#), None);
        assert_eq!(openapi_title(r#"{"info": {"title": "Só título"}}"#).as_deref(), Some("Só título"));
    }

    #[test]
    fn le_a_propria_porta_em_escuta() {
        let server = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = server.local_addr().unwrap().port();
        let found = listeners().into_iter().find(|l| l.port == port).expect("porta aberta pelo teste");
        assert_eq!(found.pid, std::process::id());
        assert_eq!(found.addrs, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    }

    /// O mesmo critério de `Snapshot::parent_of`.
    fn parent_in<'a>(procs: &'a [ProcInfo], child: &ProcInfo) -> Option<&'a ProcInfo> {
        procs.iter().find(|p| Some(p.pid) == child.parent && p.start_time <= child.start_time)
    }

    fn with_cmd(mut p: ProcInfo, cmd: &str) -> ProcInfo {
        p.cmd = cmd.to_owned();
        p
    }

    #[test]
    fn raiz_do_servidor() {
        // pwsh → node (npm run dev) → cmd /c vite → node (vite, dono da porta).
        let npm = vec![
            proc(1, None, 10, "pwsh.exe"),
            with_cmd(proc(2, Some(1), 11, "node.exe"), r#""C:
odejs
ode.exe" npm-cli.js run dev"#),
            proc(3, Some(2), 12, "cmd.exe"),
            with_cmd(proc(4, Some(3), 13, "node.exe"), r"node D:pp
ode_modulesiteinite.js"),
        ];
        assert_eq!(server_root(&npm[3], |c| parent_in(&npm, c)).pid, 2);

        // pwsh → python -m uvicorn (sem reload): ele mesmo.
        let uvicorn = vec![proc(1, None, 10, "pwsh.exe"), with_cmd(proc(2, Some(1), 11, "python.exe"), "python -m uvicorn main:app")];
        assert_eq!(server_root(&uvicorn[1], |c| parent_in(&uvicorn, c)).pid, 2);

        // uvicorn --reload: o worker escuta, o reloader é o pai.
        let reload = vec![
            proc(1, None, 10, "WindowsTerminal.exe"),
            proc(2, Some(1), 11, "cmd.exe"),
            with_cmd(proc(3, Some(2), 12, "python.exe"), "python -m uvicorn main:app --reload"),
            with_cmd(proc(4, Some(3), 13, "python.exe"), "python -c from multiprocessing.spawn import spawn_main"),
        ];
        // O cmd.exe do usuário (filho do terminal) não entra.
        assert_eq!(server_root(&reload[3], |c| parent_in(&reload, c)).pid, 3);
    }

    #[test]
    fn le_as_proprias_conexoes() {
        let server = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().unwrap();
        let client = TcpStream::connect(addr).expect("connect");
        let (_accepted, _) = server.accept().expect("accept");
        let local = client.local_addr().unwrap();
        let all = connections();
        let me = std::process::id();
        // As duas pontas são deste processo: a do cliente e a aceita pelo servidor.
        let out = all.iter().find(|c| c.local == local).expect("ponta do cliente");
        assert_eq!((out.pid, out.remote, out.state), (me, addr, TcpState::Established));
        let back = all.iter().find(|c| c.local == addr && c.remote == local).expect("ponta do servidor");
        assert_eq!(back.pid, me);
        assert!(all.iter().all(|c| c.local != addr || c.remote.port() != 0), "porta em escuta não é conexão");
    }

    #[test]
    fn sonda_servidor_http_local() {
        let server = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = server.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = conn.read(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET / HTTP/1.1"));
            let body = "<title>Painel</title>";
            let _ = write!(conn, "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        });
        let target = Target { port: addr.port(), pid: 0, addr, api: false };
        let result = probe(&target, false);
        handle.join().unwrap();
        assert!(matches!(result.reply, Some(Reply::Http { status: 404, .. })));
        assert!(result.reply.is_some_and(Reply::healthy));
        assert!(result.full && result.connect_ms.is_some());
        assert_eq!(result.page_title.as_deref(), Some("Painel"));
    }

    #[test]
    fn rodada_seguinte_so_conecta_sem_requisicao() {
        let server = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().unwrap();
        // O servidor conta os bytes recebidos em cada conexão.
        let handle = std::thread::spawn(move || {
            let mut received = Vec::new();
            for _ in 0..2 {
                let (mut conn, _) = server.accept().unwrap();
                let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buf = [0u8; 1024];
                let n = conn.read(&mut buf).unwrap_or(0);
                if n > 0 {
                    let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                }
                received.push(n);
            }
            received
        });
        let target = Target { port: addr.port(), pid: 0, addr, api: false };
        let mut status = PortStatus::default();
        assert!(status.wants_full(), "porta nova pede o GET completo");
        status.apply(probe(&target, false));
        assert!(!status.wants_full(), "logo depois do GET, só conexão");
        let quick = check(&target);
        assert!(quick.reply.is_none() && quick.connect_ms.is_some());
        status.apply(quick);
        assert!(matches!(status.reply, Some(Reply::Http { status: 200, .. })), "a resposta HTTP anterior continua valendo");
        let received = handle.join().unwrap();
        assert!(received[0] > 0, "o primeiro acesso é um GET");
        assert_eq!(received[1], 0, "a rodada rápida não manda nada ao servidor");
    }

    #[test]
    fn porta_que_nao_fala_http() {
        let server = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = server.accept().unwrap();
            let _ = conn.write_all(b"\x00\x01binario");
        });
        let result = probe(&Target { port: addr.port(), pid: 0, addr, api: false }, false);
        handle.join().unwrap();
        assert_eq!(result.reply, Some(Reply::NotHttp));
    }
}
