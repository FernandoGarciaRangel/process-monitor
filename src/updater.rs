//! Atualização automática: confere a última release no GitHub, baixa o instalador numa
//! thread própria e o executa em modo silencioso quando o usuário manda.
//!
//! A versão mais recente sai do redirecionamento de `/releases/latest` (que aponta para
//! `/releases/tag/vX.Y.Z`), sem API nem JSON. O instalador segue o nome que o
//! `package.ps1` gera: `ProcessMonitor-Setup-X.Y.Z.exe`.

use std::ffi::c_void;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR, w};

pub const REPO: &str = "FernandoGarciaSimpipe/process-monitor";
/// Espera antes da primeira verificação, para não disputar a abertura com a coleta.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(15);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// Depois de uma falha (sem rede, GitHub fora), tenta de novo mais cedo.
const RETRY_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("versão do Cargo.toml")
    }

    /// Aceita `1.2.3` e `v1.2.3`.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u32>().ok());
        let version = Self(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Ainda não verificou nesta execução.
    Idle,
    Checking,
    UpToDate { at: chrono::DateTime<chrono::Local> },
    Downloading { version: Version, received: u64, total: Option<u64> },
    Ready { version: Version, installer: PathBuf },
    Failed(String),
}

enum Msg {
    Auto(bool),
    CheckNow,
}

/// Thread que verifica a cada [`CHECK_INTERVAL`] com o download automático ligado, ou
/// quando o usuário pede.
pub struct Updater {
    latest: Arc<Mutex<Arc<State>>>,
    tx: Sender<Msg>,
}

impl Updater {
    pub fn spawn(ctx: egui::Context, auto: bool) -> Self {
        let latest = Arc::new(Mutex::new(Arc::new(State::Idle)));
        let (tx, rx) = mpsc::channel();
        let shared = latest.clone();
        std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || run(ctx, shared, rx, auto))
            .expect("thread de atualização");
        Self { latest, tx }
    }

    pub fn latest(&self) -> Arc<State> {
        self.latest.lock().map(|s| s.clone()).unwrap_or_else(|_| Arc::new(State::Idle))
    }

    pub fn set_auto(&self, on: bool) {
        let _ = self.tx.send(Msg::Auto(on));
    }

    pub fn check_now(&self) {
        let _ = self.tx.send(Msg::CheckNow);
    }
}

fn run(ctx: egui::Context, latest: Arc<Mutex<Arc<State>>>, rx: Receiver<Msg>, mut auto: bool) {
    let publish = |state: State| {
        if let Ok(mut slot) = latest.lock() {
            *slot = Arc::new(state);
        }
        ctx.request_repaint();
    };
    let dir = updates_dir();
    remove_old_installers(&dir);

    let mut next = Instant::now() + FIRST_CHECK_DELAY;
    let mut ready = false;
    loop {
        let wait = if auto && !ready { next.saturating_duration_since(Instant::now()) } else { Duration::from_secs(3600) };
        match rx.recv_timeout(wait) {
            Ok(Msg::Auto(on)) => {
                // Ligar o automático verifica logo, sem esperar as 6 horas.
                if on && !auto {
                    next = Instant::now();
                }
                auto = on;
                continue;
            }
            Ok(Msg::CheckNow) => {}
            Err(RecvTimeoutError::Timeout) if auto && !ready => {}
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
        publish(State::Checking);
        let result = check_and_download(&dir, &|state| publish(state));
        next = Instant::now() + if result.is_ok() { CHECK_INTERVAL } else { RETRY_INTERVAL };
        match result {
            Ok(Some((version, installer))) => {
                ready = true;
                publish(State::Ready { version, installer });
            }
            Ok(None) => publish(State::UpToDate { at: chrono::Local::now() }),
            Err(e) => publish(State::Failed(e)),
        }
    }
}

/// `Some` com o instalador baixado se houver versão mais nova que a deste executável.
fn check_and_download(dir: &Path, publish: &dyn Fn(State)) -> Result<Option<(Version, PathBuf)>, String> {
    let client = Client::new()?;
    let version = latest_version(&client)?;
    if version <= Version::current() {
        return Ok(None);
    }
    let installer = download(&client, version, dir, publish)?;
    Ok(Some((version, installer)))
}

fn latest_version(client: &Client) -> Result<Version, String> {
    let response = client.get(&format!("https://github.com/{REPO}/releases/latest"), false)?;
    match response.status {
        301 | 302 | 303 | 307 | 308 => {}
        404 => return Err("nenhuma versão publicada no GitHub".to_owned()),
        status => return Err(format!("o GitHub respondeu {status}")),
    }
    let location = response.header(WINHTTP_QUERY_LOCATION).ok_or("o GitHub não informou a versão")?;
    tag_version(&location).ok_or_else(|| format!("versão irreconhecível em {location}"))
}

/// `…/releases/tag/v1.2.3` → 1.2.3.
fn tag_version(location: &str) -> Option<Version> {
    let (_, tag) = location.rsplit_once("/tag/")?;
    Version::parse(tag.split(['?', '#']).next()?)
}

fn installer_name(version: Version) -> String {
    format!("ProcessMonitor-Setup-{version}.exe")
}

fn download(client: &Client, version: Version, dir: &Path, publish: &dyn Fn(State)) -> Result<PathBuf, String> {
    let name = installer_name(version);
    let response = client.get(&format!("https://github.com/{REPO}/releases/download/v{version}/{name}"), true)?;
    if response.status != 200 {
        return Err(format!("o download do instalador respondeu {}", response.status));
    }
    let total = response.number(WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);

    // Baixado numa execução anterior e não instalado: aproveita.
    let path = dir.join(&name);
    if let (Ok(meta), Some(total)) = (fs::metadata(&path), total)
        && meta.len() == total
    {
        return Ok(path);
    }

    fs::create_dir_all(dir).map_err(|e| format!("não criou {}: {e}", dir.display()))?;
    let partial = dir.join(format!("{name}.part"));
    let mut file = File::create(&partial).map_err(|e| format!("não criou {}: {e}", partial.display()))?;
    let mut received = 0u64;
    let mut head = [0u8; 2];
    let mut last_publish = Instant::now();
    publish(State::Downloading { version, received, total });
    let result = response.read_each(|chunk| {
        if received < 2 {
            for (i, b) in chunk.iter().take(2 - received as usize).enumerate() {
                head[received as usize + i] = *b;
            }
        }
        file.write_all(chunk).map_err(|e| format!("erro ao gravar o instalador: {e}"))?;
        received += chunk.len() as u64;
        if last_publish.elapsed() >= Duration::from_millis(150) {
            last_publish = Instant::now();
            publish(State::Downloading { version, received, total });
        }
        Ok(())
    });
    drop(file);
    let check = result.and_then(|()| {
        if total.is_some_and(|t| t != received) {
            Err("o download veio incompleto".to_owned())
        } else if &head != b"MZ" {
            Err("o arquivo baixado não é um executável".to_owned())
        } else {
            Ok(())
        }
    });
    if let Err(e) = check {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, &path).map_err(|e| format!("não renomeou o instalador: {e}"))?;
    Ok(path)
}

/// Executa o instalador em silêncio. Ele fecha esta instância, se ainda estiver aberta,
/// e abre a versão nova no fim (`/RELAUNCH=1`, tratado no `process-monitor.iss`).
pub fn install(installer: &Path) -> Result<(), String> {
    let log = installer.with_extension("log");
    std::process::Command::new(installer)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS", "/RELAUNCH=1"])
        .arg(format!("/LOG={}", log.display()))
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Ao lado dos dados do app, por usuário: não pede administrador para gravar.
fn updates_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Process Monitor")
        .join("updates")
}

/// Instaladores (e logs) de versões já instaladas e downloads interrompidos só ocupam
/// espaço. O log de uma atualização que falhou fica: a versão dele ainda é mais nova.
fn remove_old_installers(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let current = Version::current();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = name
            .strip_prefix("ProcessMonitor-Setup-")
            .and_then(|rest| rest.strip_suffix(".exe").or_else(|| rest.strip_suffix(".log")))
            .and_then(Version::parse)
            .is_some_and(|v| v <= current);
        if old || name.ends_with(".part") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Handle do WinHTTP, fechado ao sair de escopo.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn handle(raw: *mut c_void) -> Result<Handle, String> {
    if raw.is_null() { Err(describe(windows::core::Error::from_thread())) } else { Ok(Handle(raw)) }
}

/// WinHTTP: HTTPS do próprio Windows, com o proxy do sistema, sem dependência nova.
struct Client {
    session: Handle,
}

struct Response<'a> {
    // Ordem de declaração = ordem de fechamento: a requisição antes da conexão.
    request: Handle,
    _connect: Handle,
    _client: std::marker::PhantomData<&'a Client>,
    status: u32,
}

impl Client {
    fn new() -> Result<Self, String> {
        let agent = HSTRING::from(format!("ProcessMonitor/{}", Version::current()));
        let session =
            handle(unsafe { WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0) })?;
        unsafe { WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000) }.map_err(describe)?;
        Ok(Self { session })
    }

    /// GET em `https://host/caminho`. Sem `follow`, um redirecionamento volta como
    /// resposta (para ler o `Location`).
    fn get(&self, url: &str, follow: bool) -> Result<Response<'_>, String> {
        let rest = url.strip_prefix("https://").ok_or("só HTTPS")?;
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        unsafe {
            let connect = handle(WinHttpConnect(self.session.0, &HSTRING::from(host), INTERNET_DEFAULT_HTTPS_PORT, 0))?;
            let request = handle(WinHttpOpenRequest(
                connect.0,
                w!("GET"),
                &HSTRING::from(path),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            if !follow {
                let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
                WinHttpSetOption(Some(request.0), WINHTTP_OPTION_REDIRECT_POLICY, Some(&policy)).map_err(describe)?;
            }
            WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(describe)?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(describe)?;
            let mut response = Response { request, _connect: connect, _client: std::marker::PhantomData, status: 0 };
            response.status = response.number(WINHTTP_QUERY_STATUS_CODE).ok_or("resposta sem código HTTP")?;
            Ok(response)
        }
    }
}

impl Response<'_> {
    fn number(&self, query: u32) -> Option<u32> {
        let mut value = 0u32;
        let mut len = size_of::<u32>() as u32;
        unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                query | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut value).cast()),
                &mut len,
                std::ptr::null_mut(),
            )
        }
        .ok()?;
        Some(value)
    }

    fn header(&self, query: u32) -> Option<String> {
        let mut buf = vec![0u16; 2048];
        let mut len = (buf.len() * 2) as u32;
        unsafe {
            WinHttpQueryHeaders(self.request.0, query, PCWSTR::null(), Some(buf.as_mut_ptr().cast()), &mut len, std::ptr::null_mut())
        }
        .ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize / 2]))
    }

    fn read_each(&self, mut f: impl FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            unsafe { WinHttpReadData(self.request.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) }.map_err(describe)?;
            if read == 0 {
                return Ok(());
            }
            f(&buf[..read as usize])?;
        }
    }
}

/// Os erros do WinHTTP não têm texto no FormatMessage do sistema: os comuns em português.
fn describe(e: windows::core::Error) -> String {
    match e.code().0 as u32 & 0xFFFF {
        12002 => "o GitHub não respondeu a tempo".to_owned(),
        12007 | 12029 => "sem conexão com o GitHub".to_owned(),
        12030 | 12031 => "a conexão caiu".to_owned(),
        12175 | 12038 | 12044 | 12045 => "falha no certificado HTTPS".to_owned(),
        code => format!("erro de rede {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_order() {
        assert_eq!(Version::parse("v0.10.2"), Some(Version(0, 10, 2)));
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("1.2.x"), None);
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
    }

    #[test]
    fn tag_from_redirect() {
        let url = "https://github.com/FernandoGarciaSimpipe/process-monitor/releases/tag/v0.3.0";
        assert_eq!(tag_version(url), Some(Version(0, 3, 0)));
        assert_eq!(tag_version("https://github.com/x/y/releases"), None);
    }

    /// Acessa a rede: `cargo test -- --ignored`. Usa um repo público qualquer para
    /// exercitar o WinHTTP, o redirecionamento sem seguir e o download com redirect.
    #[test]
    #[ignore]
    fn winhttp_against_github() {
        let client = Client::new().unwrap();
        let latest = client.get("https://github.com/BurntSushi/ripgrep/releases/latest", false).unwrap();
        assert_eq!(latest.status, 302);
        let location = latest.header(WINHTTP_QUERY_LOCATION).unwrap();
        let version = tag_version(&location.replace("/tag/", "/tag/v")).expect(&location);
        assert!(version >= Version(14, 0, 0));

        let file = client
            .get(&format!("https://github.com/BurntSushi/ripgrep/releases/download/{version}/ripgrep-{version}-x86_64-pc-windows-msvc.zip.sha256"), true)
            .unwrap();
        assert_eq!(file.status, 200);
        let mut body = Vec::new();
        file.read_each(|c| {
            body.extend_from_slice(c);
            Ok(())
        })
        .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("ripgrep"));
    }
}
