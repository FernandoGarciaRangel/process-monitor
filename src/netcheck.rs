//! Saúde da rede: o roteador, o DNS e a internet testados em camadas, o sinal e as
//! quedas do Wi-Fi, os erros do adaptador e os processos que não conseguem conectar.
//!
//! A thread `netcheck` mede a cada [`ROUND_EVERY`]; quem conclui é a thread da coleta,
//! com [`diagnose`] e [`Sockets`], que não fazem chamada de sistema (os testes usam).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::Dns::{
    DNS_QUERY_BYPASS_CACHE, DNS_QUERY_NO_HOSTS_FILE, DNS_RECORDA, DNS_TYPE_A, DnsFree, DnsFreeRecordList, DnsQuery_W,
};
use windows::Win32::NetworkManagement::IpHelper::{
    GetBestRoute, GetIfEntry2, ICMP_ECHO_REPLY, IF_TYPE_IEEE80211, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
    MIB_IF_ROW2, MIB_IPFORWARDROW,
};
use windows::Win32::NetworkManagement::Ndis::MediaConnectStateConnected;
use windows::Win32::NetworkManagement::WiFi::{
    WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory,
    WlanOpenHandle, WlanQueryInterface, wlan_interface_state_connected, wlan_intf_opcode_current_connection,
    wlan_intf_opcode_rssi,
};
use windows::core::PCWSTR;

use crate::format;
use crate::ports::{Connection, TcpState};
use crate::sampler::{ProcInfo, ProcKey};

/// Uma rodada de testes a cada 5 s: três pacotes pequenos, nada que pese na rede.
pub const ROUND_EVERY: Duration = Duration::from_secs(5);
/// Rodadas guardadas: 2 minutos, a janela da perda.
const KEEP_ROUNDS: usize = 24;
/// Dois servidores de empresas diferentes, pelo IP: o teste da internet não depende do
/// DNS, e um deles fora do ar não vira "sem internet".
const INTERNET: [SocketAddrV4; 2] =
    [SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443), SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 443)];
const INTERNET_TIMEOUT: Duration = Duration::from_secs(3);
/// O nome que o próprio Windows consulta para saber se tem internet.
const DNS_NAME: &str = "www.msftconnecttest.com";
const PING_TIMEOUT_MS: u32 = 1000;
/// Rodadas seguidas com falha para concluir que caiu: uma só pode ser um pacote perdido.
const DOWN_ROUNDS: usize = 2;
/// Perda só conta com pelo menos 1 minuto de rodadas, e a partir de 3 falhas.
const LOSS_MIN_ROUNDS: usize = 12;
const LOSS_MIN_FAILURES: usize = 3;
/// Latência: mediana das últimas respostas. Um handshake TCP com a Cloudflare ou o
/// Google leva menos de 50 ms numa conexão boa.
const LATENCY_SAMPLES: usize = 12;
const LATENCY_MIN_SAMPLES: usize = 6;
const LATENCY_HIGH_MS: u32 = 250;
/// Qualidade do Windows (0–100). 40% fica perto de -80 dBm, onde a conexão já sofre.
const WEAK_SIGNAL: u32 = 40;
/// Leituras seguidas abaixo do limite: passar no corredor não avisa.
const WEAK_ROUNDS: u32 = 3;
const DROPS_WINDOW: Duration = Duration::from_secs(10 * 60);
const DROPS_MIN: usize = 2;
/// Erros do adaptador no último minuto: pelo menos 20 e 1% dos pacotes.
const ERRORS_WINDOW: Duration = Duration::from_secs(60);
const ERRORS_MIN_SPAN: Duration = Duration::from_secs(30);
const ERRORS_MIN: u64 = 20;
const ERRORS_MIN_PERCENT: f32 = 1.0;
/// Conexão tentando abrir (SYN enviado, sem resposta) há esse tempo.
const STUCK_AFTER: Duration = Duration::from_secs(15);
/// Intervalo entre tentativas que ainda conta como a mesma insistência.
const RETRY_GRACE: Duration = Duration::from_secs(10);
/// Conexões que o outro lado fechou e o programa não: a partir de quantas, por quanto tempo.
const LEAK_MIN: usize = 20;
const LEAK_AFTER: Duration = Duration::from_secs(60);

/// Resultado de um teste numa rodada.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Step {
    /// Respondeu, em milissegundos.
    Ok(u32),
    Failed,
    /// Não deu para testar (sem roteador na rota, sem adaptador).
    #[default]
    Skipped,
}

impl Step {
    fn failed(self) -> bool {
        self == Step::Failed
    }

    fn ms(self) -> Option<u32> {
        match self {
            Step::Ok(ms) => Some(ms),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Round {
    pub gateway: Step,
    pub dns: Step,
    pub internet: Step,
}

/// O adaptador por onde sai o tráfego para a internet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Adapter {
    pub name: String,
    pub wifi: bool,
    pub connected: bool,
    /// Velocidade do link, em bits por segundo.
    pub speed: u64,
    /// Contadores desde que o adaptador subiu.
    pub errors: u64,
    pub packets: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wifi {
    pub connected: bool,
    pub ssid: String,
    /// Qualidade do sinal pelo Windows, 0–100.
    pub signal: u32,
    pub rssi: Option<i32>,
    /// Taxa de recepção negociada, em kbit/s.
    pub rate_kbps: u32,
}

/// Uma leitura da thread: o que ela viu na rodada.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    /// `None`: nenhuma rota para a internet (sem rede).
    pub route: Option<Option<Ipv4Addr>>,
    pub adapter: Option<Adapter>,
    pub wifi: Option<Wifi>,
    pub round: Round,
}

/// O que a thread mediu até agora. A thread da coleta lê e tira as conclusões.
#[derive(Clone, Debug, Default)]
pub struct Measured {
    /// Sem nenhuma rodada ainda: a tela mostra "testando".
    pub started: bool,
    pub online: bool,
    pub gateway: Option<Ipv4Addr>,
    /// O roteador já respondeu ao ping. Há roteadores que nunca respondem, e aí a
    /// falha do ping não quer dizer nada.
    pub gateway_answers: bool,
    pub adapter: Option<Adapter>,
    pub wifi: Option<Wifi>,
    pub rounds: VecDeque<Round>,
    /// (instante, erros, pacotes) do adaptador, no último minuto.
    counters: VecDeque<(Instant, u64, u64)>,
    wifi_drops: VecDeque<Instant>,
    weak_rounds: u32,
}

impl Measured {
    /// Junta a leitura nova ao histórico.
    pub fn record(&mut self, now: Instant, reading: Reading) {
        self.started = true;
        self.online = reading.route.is_some();
        let gateway = reading.route.flatten();
        if gateway != self.gateway {
            self.gateway_answers = false;
        }
        self.gateway = gateway;
        if matches!(reading.round.gateway, Step::Ok(_)) {
            self.gateway_answers = true;
        }

        // Queda do Wi-Fi: estava conectado e era por ele que a internet saía.
        let was_wifi = self.adapter.as_ref().is_some_and(|a| a.wifi)
            && self.wifi.as_ref().is_some_and(|w| w.connected);
        if was_wifi && !reading.wifi.as_ref().is_some_and(|w| w.connected) {
            self.wifi_drops.push_back(now);
        }
        while self.wifi_drops.front().is_some_and(|t| now.duration_since(*t) > DROPS_WINDOW) {
            self.wifi_drops.pop_front();
        }

        let on_wifi = reading.adapter.as_ref().is_some_and(|a| a.wifi);
        match &reading.wifi {
            Some(w) if on_wifi && w.connected && w.signal < WEAK_SIGNAL => self.weak_rounds += 1,
            _ => self.weak_rounds = 0,
        }

        // Os contadores recomeçam com outro adaptador.
        if reading.adapter.as_ref().map(|a| &a.name) != self.adapter.as_ref().map(|a| &a.name) {
            self.counters.clear();
        }
        if let Some(a) = &reading.adapter {
            self.counters.push_back((now, a.errors, a.packets));
        }
        while self.counters.front().is_some_and(|(t, ..)| now.duration_since(*t) > ERRORS_WINDOW) {
            self.counters.pop_front();
        }

        self.adapter = reading.adapter;
        self.wifi = reading.wifi;
        if self.rounds.len() == KEEP_ROUNDS {
            self.rounds.pop_front();
        }
        self.rounds.push_back(reading.round);
    }

    pub fn last(&self) -> Option<Round> {
        self.rounds.back().copied()
    }

    /// Rodadas em que a internet foi testada e quantas falharam.
    fn internet_failures(&self) -> (usize, usize) {
        let tested = self.rounds.iter().filter(|r| r.internet != Step::Skipped);
        let (mut failed, mut total) = (0, 0);
        for r in tested {
            total += 1;
            failed += usize::from(r.internet.failed());
        }
        (failed, total)
    }

    /// Porcentagem das rodadas em que a internet não respondeu, com rodadas suficientes.
    pub fn loss(&self) -> Option<u32> {
        let (failed, total) = self.internet_failures();
        (total >= LOSS_MIN_ROUNDS).then(|| (failed * 100 / total) as u32)
    }

    /// Mediana do tempo de resposta da internet nas últimas rodadas.
    pub fn latency(&self) -> Option<u32> {
        let mut ms: Vec<u32> = self.rounds.iter().rev().filter_map(|r| r.internet.ms()).take(LATENCY_SAMPLES).collect();
        if ms.len() < LATENCY_MIN_SAMPLES {
            return None;
        }
        ms.sort_unstable();
        Some(ms[ms.len() / 2])
    }

    pub fn wifi_drops(&self) -> usize {
        self.wifi_drops.len()
    }

    /// Erros e pacotes no último minuto, se a janela já cobre meio minuto.
    fn recent_errors(&self) -> Option<(u64, u64)> {
        let (t0, e0, p0) = *self.counters.front()?;
        let (t1, e1, p1) = *self.counters.back()?;
        (t1.duration_since(t0) >= ERRORS_MIN_SPAN).then(|| (e1.saturating_sub(e0), p1.saturating_sub(p0)))
    }

    /// As últimas `n` rodadas, se já houver tantas.
    fn tail(&self, n: usize) -> Option<impl Iterator<Item = &Round>> {
        (self.rounds.len() >= n).then(|| self.rounds.iter().skip(self.rounds.len() - n))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Issue {
    /// Nenhuma rota para a internet: sem Wi-Fi e sem cabo.
    Offline,
    /// O roteador, que costuma responder, parou, e a internet junto.
    RouterDown { gateway: Ipv4Addr },
    /// A internet não responde, mas o roteador sim (ou ele nunca responde a ping).
    NoInternet { router_ok: bool },
    DnsDown,
    Unstable { loss: u32 },
    Slow { ms: u32 },
    WeakWifi { signal: u32, rssi: Option<i32> },
    WifiDrops { count: usize },
    AdapterErrors { adapter: String, errors: u64, percent: f32 },
    /// Um processo tenta abrir uma conexão e o outro lado não responde.
    CantConnect { key: ProcKey, process: String, remote: SocketAddr, secs: u64 },
    /// Um processo acumula conexões que o outro lado fechou e ele não.
    SocketLeak { key: ProcKey, process: String, count: usize },
}

impl Issue {
    /// Sem internet de jeito nenhum: os outros avisos de conexão não ajudam.
    pub fn is_down(&self) -> bool {
        matches!(self, Issue::Offline | Issue::RouterDown { .. } | Issue::NoInternet { .. })
    }

    pub fn severe(&self) -> bool {
        self.is_down() || matches!(self, Issue::DnsDown)
    }

    /// O processo do aviso, quando é de um processo.
    pub fn process(&self) -> Option<ProcKey> {
        match self {
            Issue::CantConnect { key, .. } | Issue::SocketLeak { key, .. } => Some(*key),
            _ => None,
        }
    }

    /// Para os alertas: o mesmo problema mantém o nome enquanto dura.
    pub fn kind(&self) -> &'static str {
        match self {
            Issue::Offline => "offline",
            Issue::RouterDown { .. } => "router",
            Issue::NoInternet { .. } => "internet",
            Issue::DnsDown => "dns",
            Issue::Unstable { .. } => "loss",
            Issue::Slow { .. } => "latency",
            Issue::WeakWifi { .. } => "weak",
            Issue::WifiDrops { .. } => "drops",
            Issue::AdapterErrors { .. } => "errors",
            Issue::CantConnect { .. } => "connect",
            Issue::SocketLeak { .. } => "leak",
        }
    }

    pub fn title(&self) -> String {
        match self {
            Issue::Offline => "Sem rede".to_owned(),
            Issue::RouterDown { .. } => "Roteador não responde".to_owned(),
            Issue::NoInternet { .. } => "Sem internet".to_owned(),
            Issue::DnsDown => "DNS não responde".to_owned(),
            Issue::Unstable { .. } => "Conexão instável".to_owned(),
            Issue::Slow { .. } => "Internet lenta para responder".to_owned(),
            Issue::WeakWifi { .. } => "Sinal de Wi-Fi fraco".to_owned(),
            Issue::WifiDrops { .. } => "Wi-Fi caindo".to_owned(),
            Issue::AdapterErrors { .. } => "Erros no adaptador de rede".to_owned(),
            Issue::CantConnect { process, .. } => format!("{process} não consegue conectar"),
            Issue::SocketLeak { process, .. } => format!("{process} não fecha conexões"),
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Issue::Offline => "Nenhum adaptador conectado: o Wi-Fi ou o cabo caiu.".to_owned(),
            Issue::RouterDown { gateway } => format!(
                "O roteador ({gateway}) e a internet não respondem. Confira o Wi-Fi ou o cabo, ou reinicie o roteador."
            ),
            Issue::NoInternet { router_ok: true } => {
                "O roteador responde, mas a internet não: o problema está no roteador ou no provedor.".to_owned()
            }
            Issue::NoInternet { router_ok: false } => "A internet não responde.".to_owned(),
            Issue::DnsDown => "A internet funciona, mas os nomes não resolvem: sites e programas não acham os \
                               servidores. Reinicie o roteador ou troque o DNS (1.1.1.1, 8.8.8.8)."
                .to_owned(),
            Issue::Unstable { loss } => {
                format!("{loss}% das tentativas de falar com a internet falharam nos últimos 2 minutos.")
            }
            Issue::Slow { ms } => format!("A internet leva {ms} ms para responder; o normal é abaixo de 100 ms."),
            Issue::WeakWifi { signal, rssi } => match rssi {
                Some(dbm) => format!("Sinal em {signal}% ({dbm} dBm). Chegue mais perto do roteador."),
                None => format!("Sinal em {signal}%. Chegue mais perto do roteador."),
            },
            Issue::WifiDrops { count } => format!("O Wi-Fi desconectou {count} vezes nos últimos 10 minutos."),
            Issue::AdapterErrors { adapter, errors, percent } => format!(
                "{adapter}: {errors} pacotes com erro no último minuto ({percent:.1}%). Pode ser o cabo, a porta ou o driver."
            ),
            Issue::CantConnect { remote, secs, .. } => format!(
                "Tenta {remote} há {} sem resposta: o servidor está fora do ar ou um firewall bloqueia.",
                format::duration(*secs)
            ),
            Issue::SocketLeak { count, .. } => format!(
                "{count} conexões que o outro lado já fechou continuam abertas no programa. Costuma ser vazamento; \
                 reiniciar o programa libera."
            ),
        }
    }
}

/// Os problemas de rede da máquina, pelas medições. Os dos processos vêm de [`Sockets`].
pub fn diagnose(m: &Measured) -> Vec<Issue> {
    let mut out = Vec::new();
    if !m.started {
        return out;
    }
    if !m.online {
        out.push(Issue::Offline);
    } else if let Some(mut tail) = m.tail(DOWN_ROUNDS)
        && tail.all(|r| r.internet.failed())
    {
        let router_failed =
            m.gateway_answers && m.tail(DOWN_ROUNDS).is_some_and(|mut t| t.all(|r| r.gateway.failed()));
        match m.gateway {
            Some(gateway) if router_failed => out.push(Issue::RouterDown { gateway }),
            _ => out.push(Issue::NoInternet { router_ok: m.gateway_answers && !router_failed }),
        }
    } else {
        let internet_ok = m.last().is_some_and(|r| matches!(r.internet, Step::Ok(_)));
        if internet_ok && m.tail(DOWN_ROUNDS).is_some_and(|mut t| t.all(|r| r.dns.failed())) {
            out.push(Issue::DnsDown);
        }
        if let Some(loss) = m.loss()
            && m.internet_failures().0 >= LOSS_MIN_FAILURES
        {
            out.push(Issue::Unstable { loss });
        }
        if let Some(ms) = m.latency().filter(|ms| *ms >= LATENCY_HIGH_MS) {
            out.push(Issue::Slow { ms });
        }
    }
    if m.weak_rounds >= WEAK_ROUNDS
        && let Some(w) = &m.wifi
    {
        out.push(Issue::WeakWifi { signal: w.signal, rssi: w.rssi });
    }
    if m.wifi_drops() >= DROPS_MIN {
        out.push(Issue::WifiDrops { count: m.wifi_drops() });
    }
    if let Some((errors, packets)) = m.recent_errors()
        && errors >= ERRORS_MIN
    {
        let percent = errors as f32 * 100.0 / packets.max(1) as f32;
        if percent >= ERRORS_MIN_PERCENT {
            let adapter = m.adapter.as_ref().map(|a| a.name.clone()).unwrap_or_default();
            out.push(Issue::AdapterErrors { adapter, errors, percent });
        }
    }
    out
}

/// Acompanha as conexões TCP de cada processo entre as coletas.
#[derive(Default)]
pub struct Sockets {
    /// Desde quando cada processo tenta o mesmo endereço sem resposta, e a última vez
    /// que a tentativa foi vista.
    stuck: HashMap<(u32, SocketAddr), (Instant, Instant)>,
    /// Desde quando cada processo está acima do limite de conexões não fechadas.
    leaking: HashMap<u32, Instant>,
}

impl Sockets {
    pub fn update(&mut self, now: Instant, connections: &[Connection], processes: &[ProcInfo], own_pid: u32) -> Vec<Issue> {
        let mut trying: HashSet<(u32, SocketAddr)> = HashSet::new();
        let mut half_closed: HashMap<u32, usize> = HashMap::new();
        for c in connections.iter().filter(|c| c.pid != own_pid && c.pid != 0) {
            match c.state {
                TcpState::SynSent if !local(c.remote.ip()) => {
                    trying.insert((c.pid, c.remote));
                }
                TcpState::CloseWait => *half_closed.entry(c.pid).or_default() += 1,
                _ => {}
            }
        }
        // Quem tenta de novo logo depois de desistir (o Windows desiste em 21 s) continua
        // contando; conectou, recomeça.
        let connected: HashSet<(u32, SocketAddr)> =
            connections.iter().filter(|c| c.state == TcpState::Established).map(|c| (c.pid, c.remote)).collect();
        for k in &trying {
            self.stuck.entry(*k).and_modify(|(_, seen)| *seen = now).or_insert((now, now));
        }
        self.stuck.retain(|k, (_, seen)| !connected.contains(k) && now.duration_since(*seen) < RETRY_GRACE);
        self.leaking.retain(|pid, _| half_closed.get(pid).is_some_and(|n| *n >= LEAK_MIN));
        for (pid, n) in &half_closed {
            if *n >= LEAK_MIN {
                self.leaking.entry(*pid).or_insert(now);
            }
        }

        let find = |pid: u32| processes.iter().find(|p| p.pid == pid);
        let mut out = Vec::new();
        let mut stuck: Vec<_> = self
            .stuck
            .iter()
            .filter(|(k, (since, _))| trying.contains(k) && now.duration_since(*since) >= STUCK_AFTER)
            .map(|(k, (since, _))| (k, since))
            .collect();
        stuck.sort_by_key(|(_, since)| **since);
        for ((pid, remote), since) in stuck {
            // Um aviso por processo: o primeiro endereço que travou.
            if out.iter().any(|i: &Issue| i.process().is_some_and(|k| k.pid == *pid)) {
                continue;
            }
            if let Some(p) = find(*pid) {
                out.push(Issue::CantConnect {
                    key: p.key(),
                    process: p.display_name().to_owned(),
                    remote: *remote,
                    secs: now.duration_since(*since).as_secs(),
                });
            }
        }
        for (pid, since) in &self.leaking {
            if now.duration_since(*since) >= LEAK_AFTER
                && let Some(p) = find(*pid)
            {
                out.push(Issue::SocketLeak {
                    key: p.key(),
                    process: p.display_name().to_owned(),
                    count: half_closed[pid],
                });
            }
        }
        out
    }
}

/// Conexão para a própria máquina ou para a rede local: não é "internet".
fn local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// O que a tela e os alertas leem: as medições e as conclusões.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub measured: Arc<Measured>,
    pub issues: Vec<Issue>,
}

/// A thread que mede. Para quando o dono é descartado.
pub struct Prober {
    latest: Arc<Mutex<Arc<Measured>>>,
    stop: Arc<AtomicBool>,
}

impl Prober {
    pub fn spawn() -> Self {
        let latest = Arc::new(Mutex::new(Arc::new(Measured::default())));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, s) = (latest.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("netcheck".into()).spawn(move || run(l, s));
        Self { latest, stop }
    }

    pub fn latest(&self) -> Arc<Measured> {
        self.latest.lock().map(|m| m.clone()).unwrap_or_default()
    }
}

impl Drop for Prober {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(latest: Arc<Mutex<Arc<Measured>>>, stop: Arc<AtomicBool>) {
    let wlan = Wlan::open();
    let mut measured = Measured::default();
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let reading = read(wlan.as_ref());
        measured.record(Instant::now(), reading);
        if let Ok(mut slot) = latest.lock() {
            *slot = Arc::new(measured.clone());
        }
        let deadline = started + ROUND_EVERY;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn read(wlan: Option<&Wlan>) -> Reading {
    let wifi = wlan.and_then(Wlan::read);
    let Some((if_index, gateway)) = best_route() else {
        return Reading { route: None, adapter: None, wifi, round: Round::default() };
    };
    let round = Round {
        gateway: gateway.map_or(Step::Skipped, ping),
        dns: resolve(),
        internet: internet(),
    };
    Reading { route: Some(gateway), adapter: adapter(if_index), wifi, round }
}

/// A rota que o Windows usaria para a internet: o adaptador e o roteador.
fn best_route() -> Option<(u32, Option<Ipv4Addr>)> {
    let mut row = MIB_IPFORWARDROW::default();
    let dest = u32::from_ne_bytes(INTERNET[0].ip().octets());
    if unsafe { GetBestRoute(dest, None, &mut row) } != 0 {
        return None;
    }
    let hop = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
    // Sem roteador (VPN ponto a ponto, por exemplo), o próximo salto é o próprio destino.
    let gateway = (!hop.is_unspecified() && hop != *INTERNET[0].ip()).then_some(hop);
    Some((row.dwForwardIfIndex, gateway))
}

fn adapter(index: u32) -> Option<Adapter> {
    let mut row = MIB_IF_ROW2 { InterfaceIndex: index, ..Default::default() };
    if unsafe { GetIfEntry2(&mut row) }.0 != 0 {
        return None;
    }
    Some(Adapter {
        name: crate::system::wide_to_string(&row.Alias),
        wifi: row.Type == IF_TYPE_IEEE80211,
        connected: row.MediaConnectState == MediaConnectStateConnected,
        speed: row.ReceiveLinkSpeed,
        errors: row.InErrors + row.OutErrors,
        packets: row.InUcastPkts + row.InNUcastPkts + row.OutUcastPkts + row.OutNUcastPkts,
    })
}

fn ping(addr: Ipv4Addr) -> Step {
    unsafe {
        let Ok(handle) = IcmpCreateFile() else { return Step::Skipped };
        let data = [0x50u8; 32];
        let mut reply = vec![0u8; size_of::<ICMP_ECHO_REPLY>() + data.len() + 8];
        let answers = IcmpSendEcho(
            handle,
            u32::from_ne_bytes(addr.octets()),
            data.as_ptr().cast(),
            data.len() as u16,
            None,
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            PING_TIMEOUT_MS,
        );
        let _ = IcmpCloseHandle(handle);
        if answers == 0 {
            return Step::Failed;
        }
        // O buffer de bytes não tem o alinhamento da estrutura.
        let r = std::ptr::read_unaligned(reply.as_ptr().cast::<ICMP_ECHO_REPLY>());
        if r.Status == 0 { Step::Ok(r.RoundTripTime) } else { Step::Failed }
    }
}

/// Consulta o DNS de verdade: sem o cache do Windows, que responderia mesmo com o
/// servidor fora do ar.
fn resolve() -> Step {
    let name: Vec<u16> = DNS_NAME.encode_utf16().chain([0]).collect();
    let mut records: *mut DNS_RECORDA = std::ptr::null_mut();
    let t = Instant::now();
    let result = unsafe {
        DnsQuery_W(
            PCWSTR(name.as_ptr()),
            DNS_TYPE_A,
            DNS_QUERY_BYPASS_CACHE | DNS_QUERY_NO_HOSTS_FILE,
            None,
            &mut records,
            None,
        )
    };
    let ms = t.elapsed().as_millis() as u32;
    if !records.is_null() {
        unsafe { DnsFree(Some(records as *const _), DnsFreeRecordList) };
    }
    if result.0 == 0 { Step::Ok(ms) } else { Step::Failed }
}

/// Abre e fecha uma conexão TCP: passa por firewalls que bloqueiam ping.
fn internet() -> Step {
    for addr in INTERNET {
        let t = Instant::now();
        if TcpStream::connect_timeout(&SocketAddr::V4(addr), INTERNET_TIMEOUT).is_ok() {
            return Step::Ok(t.elapsed().as_millis() as u32);
        }
    }
    Step::Failed
}

/// Sessão do serviço de Wi-Fi do Windows. `None` em máquina sem Wi-Fi.
struct Wlan(HANDLE);

impl Wlan {
    fn open() -> Option<Self> {
        let mut version = 0u32;
        let mut handle = HANDLE::default();
        (unsafe { WlanOpenHandle(2, None, &mut version, &mut handle) } == 0).then_some(Wlan(handle))
    }

    fn read(&self) -> Option<Wifi> {
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(self.0, None, &mut list) != 0 || list.is_null() {
                return None;
            }
            let infos = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), (*list).dwNumberOfItems as usize);
            let chosen = infos
                .iter()
                .find(|i| i.isState == wlan_interface_state_connected)
                .or(infos.first())
                .map(|i| (i.InterfaceGuid, i.isState));
            WlanFreeMemory(list as *const _);
            let (guid, state) = chosen?;
            if state != wlan_interface_state_connected {
                return Some(Wifi::default());
            }

            let mut size = 0u32;
            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            if WlanQueryInterface(self.0, &guid, wlan_intf_opcode_current_connection, None, &mut size, &mut data, None) != 0
                || data.is_null()
            {
                // Conectado, mas o Windows não deu os detalhes (permissão de localização).
                return Some(Wifi { connected: true, ..Wifi::default() });
            }
            let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
            let assoc = &attrs.wlanAssociationAttributes;
            let ssid_len = (assoc.dot11Ssid.uSSIDLength as usize).min(assoc.dot11Ssid.ucSSID.len());
            let mut wifi = Wifi {
                connected: true,
                ssid: String::from_utf8_lossy(&assoc.dot11Ssid.ucSSID[..ssid_len]).into_owned(),
                signal: assoc.wlanSignalQuality,
                rssi: None,
                rate_kbps: assoc.ulRxRate,
            };
            WlanFreeMemory(data);

            let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
            if WlanQueryInterface(self.0, &guid, wlan_intf_opcode_rssi, None, &mut size, &mut data, None) == 0
                && !data.is_null()
            {
                wifi.rssi = Some(*(data as *const i32));
                WlanFreeMemory(data);
            }
            Some(wifi)
        }
    }
}

impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe { WlanCloseHandle(self.0, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::tests::proc;

    const GW: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 1);

    fn round(gateway: Step, dns: Step, internet: Step) -> Reading {
        Reading { route: Some(Some(GW)), adapter: None, wifi: None, round: Round { gateway, dns, internet } }
    }

    fn feed(m: &mut Measured, t: Instant, readings: impl IntoIterator<Item = Reading>) -> Instant {
        let mut t = t;
        for r in readings {
            m.record(t, r);
            t += ROUND_EVERY;
        }
        t
    }

    const OK: Step = Step::Ok(20);

    #[test]
    fn camadas_da_conexao() {
        let t = Instant::now();
        // Tudo bem: nada.
        let mut m = Measured::default();
        feed(&mut m, t, (0..5).map(|_| round(OK, OK, OK)));
        assert!(diagnose(&m).is_empty());

        // Uma falha só da internet não é queda.
        feed(&mut m, t, [round(OK, OK, Step::Failed)]);
        assert!(diagnose(&m).is_empty());
        // Duas seguidas, com o roteador respondendo: sem internet, culpa do provedor.
        feed(&mut m, t, [round(OK, OK, Step::Failed)]);
        assert_eq!(diagnose(&m), vec![Issue::NoInternet { router_ok: true }]);
        // O roteador também parou: o problema é perto.
        feed(&mut m, t, (0..2).map(|_| round(Step::Failed, Step::Failed, Step::Failed)));
        assert_eq!(diagnose(&m), vec![Issue::RouterDown { gateway: GW }]);

        // Roteador que nunca responde a ping: a falha dele não quer dizer nada.
        let mut m = Measured::default();
        feed(&mut m, t, (0..3).map(|_| round(Step::Failed, OK, Step::Failed)));
        assert_eq!(diagnose(&m), vec![Issue::NoInternet { router_ok: false }]);

        // Sem rota nenhuma.
        let mut m = Measured::default();
        m.record(t, Reading::default());
        assert_eq!(diagnose(&m), vec![Issue::Offline]);
    }

    #[test]
    fn dns_fora_com_internet_de_pe() {
        let mut m = Measured::default();
        feed(&mut m, Instant::now(), (0..3).map(|_| round(OK, Step::Failed, OK)));
        assert_eq!(diagnose(&m), vec![Issue::DnsDown]);
    }

    #[test]
    fn perda_e_latencia() {
        let t = Instant::now();
        let mut m = Measured::default();
        // 20 rodadas, 4 falhas espalhadas: 20% de perda, sem queda.
        feed(&mut m, t, (0..20).map(|i| round(OK, OK, if i % 5 == 0 { Step::Failed } else { OK })));
        assert_eq!(diagnose(&m), vec![Issue::Unstable { loss: 20 }]);

        let mut m = Measured::default();
        feed(&mut m, t, (0..8).map(|_| round(OK, OK, Step::Ok(400))));
        assert_eq!(diagnose(&m), vec![Issue::Slow { ms: 400 }]);
        // Poucas amostras ainda não concluem.
        let mut m = Measured::default();
        feed(&mut m, t, (0..3).map(|_| round(OK, OK, Step::Ok(400))));
        assert!(diagnose(&m).is_empty());
    }

    fn on_wifi(signal: u32, connected: bool) -> Reading {
        Reading {
            route: connected.then_some(Some(GW)),
            adapter: Some(Adapter { name: "Wi-Fi".into(), wifi: true, connected, ..Adapter::default() }),
            wifi: Some(Wifi { connected, signal, ..Wifi::default() }),
            round: if connected { Round { gateway: OK, dns: OK, internet: OK } } else { Round::default() },
        }
    }

    #[test]
    fn wifi_fraco_e_quedas() {
        let t = Instant::now();
        let mut m = Measured::default();
        // Duas leituras fracas não bastam; a terceira avisa.
        let t = feed(&mut m, t, [on_wifi(30, true), on_wifi(30, true)]);
        assert!(diagnose(&m).is_empty());
        let t = feed(&mut m, t, [on_wifi(30, true)]);
        assert_eq!(diagnose(&m), vec![Issue::WeakWifi { signal: 30, rssi: None }]);

        // Duas quedas em 10 minutos.
        let mut m = Measured::default();
        let t = feed(&mut m, t, [on_wifi(90, true), on_wifi(90, false), on_wifi(90, true), on_wifi(90, false)]);
        assert!(diagnose(&m).contains(&Issue::WifiDrops { count: 2 }));
        // Passados os 10 minutos, as quedas antigas saem da conta.
        m.record(t + DROPS_WINDOW + Duration::from_secs(60), on_wifi(90, true));
        assert_eq!(m.wifi_drops(), 0);
    }

    #[test]
    fn erros_do_adaptador() {
        let t = Instant::now();
        let reading = |errors: u64, packets: u64| Reading {
            route: Some(Some(GW)),
            adapter: Some(Adapter { name: "Ethernet".into(), errors, packets, ..Adapter::default() }),
            wifi: None,
            round: Round { gateway: OK, dns: OK, internet: OK },
        };
        let mut m = Measured::default();
        // 50 erros em 1000 pacotes ao longo de 40 s: 5%.
        feed(&mut m, t, (0..9).map(|i| reading(i * 50 / 8, i * 1000 / 8)));
        assert_eq!(
            diagnose(&m),
            vec![Issue::AdapterErrors { adapter: "Ethernet".into(), errors: 50, percent: 5.0 }]
        );
        // Poucos erros num tráfego grande: normal.
        let mut m = Measured::default();
        feed(&mut m, t, (0..9).map(|i| reading(i * 30 / 8, i * 100_000 / 8)));
        assert!(diagnose(&m).is_empty());
    }

    fn conn(pid: u32, remote: &str, state: TcpState) -> Connection {
        Connection { pid, local: "192.168.0.10:50000".parse().unwrap(), remote: remote.parse().unwrap(), state }
    }

    #[test]
    fn processo_que_nao_conecta() {
        let t = Instant::now();
        let app = proc(10, None, 100, "app.exe");
        let procs = std::slice::from_ref(&app);
        let mut s = Sockets::default();
        let trying = [conn(10, "203.0.113.5:443", TcpState::SynSent)];
        assert!(s.update(t, &trying, procs, 1).is_empty());
        let found = s.update(t + STUCK_AFTER, &trying, procs, 1);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].process(), Some(app.key()));
        // O Windows desistiu e o programa tentou de novo 3 s depois: continua contando.
        let later = t + Duration::from_secs(21);
        assert!(s.update(later, &[], procs, 1).is_empty());
        let again = s.update(later + Duration::from_secs(3), &trying, procs, 1);
        assert!(matches!(again[0], Issue::CantConnect { secs: 24, .. }));
        // Conectou: some e recomeça do zero.
        assert!(s.update(later, &[conn(10, "203.0.113.5:443", TcpState::Established)], procs, 1).is_empty());
        assert!(s.update(later + Duration::from_secs(1), &trying, procs, 1).is_empty());
        // Para a própria máquina não conta.
        let local = [conn(10, "127.0.0.1:5432", TcpState::SynSent)];
        s.update(t, &local, procs, 1);
        assert!(s.update(t + STUCK_AFTER, &local, procs, 1).is_empty());
    }

    #[test]
    fn processo_que_nao_fecha_conexoes() {
        let t = Instant::now();
        let app = proc(10, None, 100, "app.exe");
        let procs = std::slice::from_ref(&app);
        let mut s = Sockets::default();
        let many: Vec<Connection> = (0..25).map(|i| conn(10, &format!("203.0.113.5:{}", 1000 + i), TcpState::CloseWait)).collect();
        assert!(s.update(t, &many, procs, 1).is_empty());
        let found = s.update(t + LEAK_AFTER, &many, procs, 1);
        assert_eq!(found, vec![Issue::SocketLeak { key: app.key(), process: "app.exe".into(), count: 25 }]);
        // Abaixo do limite, recomeça a contar.
        assert!(s.update(t + LEAK_AFTER, &many[..5], procs, 1).is_empty());
        assert!(s.update(t + LEAK_AFTER * 2, &many, procs, 1).is_empty());
    }

    /// Mede de verdade nesta máquina: `cargo test leitura_da_rede -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn leitura_da_rede() {
        let reading = read(Wlan::open().as_ref());
        println!("{reading:#?}");
        assert!(reading.route.is_some());
    }
}
