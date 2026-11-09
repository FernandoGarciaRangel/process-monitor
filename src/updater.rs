//! Atualização automática: confere a última release no GitHub, baixa o instalador numa
//! thread própria e o executa em modo silencioso quando o usuário manda.
//!
//! A versão mais recente sai do redirecionamento de `/releases/latest` (que aponta para
//! `/releases/tag/vX.Y.Z`), sem API nem JSON. O instalador segue o nome que o
//! `package.ps1` gera: `ProcessMonitor-Setup-X.Y.Z.exe`.

use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Networking::WinHttp::{WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_LOCATION};

use crate::http::Client;

pub const REPO: &str = "FernandoGarciaRangel/process-monitor";
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
    // Com uma versão pronta, continua verificando: se sair outra enquanto o app fica na
    // bandeja, o aviso passa a ser dela.
    let mut ready = false;
    loop {
        let wait = if auto { next.saturating_duration_since(Instant::now()) } else { Duration::from_secs(3600) };
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
            Err(RecvTimeoutError::Timeout) if auto => {}
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Pronta, a verificação é silenciosa: o botão de instalar não some enquanto isso.
        if !ready {
            publish(State::Checking);
        }
        let result = check_and_download(&dir, &|state| publish(state));
        next = Instant::now() + if result.is_ok() { CHECK_INTERVAL } else { RETRY_INTERVAL };
        match result {
            Ok(Some((version, installer))) => {
                ready = true;
                publish(State::Ready { version, installer });
            }
            Ok(None) => {
                ready = false;
                publish(State::UpToDate { at: chrono::Local::now() });
            }
            // A versão já baixada continua valendo.
            Err(_) if ready => {}
            Err(e) => publish(State::Failed(e)),
        }
    }
}

/// `Some` com o instalador baixado se houver versão mais nova que a deste executável.
fn check_and_download(dir: &Path, publish: &dyn Fn(State)) -> Result<Option<(Version, PathBuf)>, String> {
    let client = Client::new([10_000, 10_000, 30_000, 30_000]).map_err(github)?;
    let version = latest_version(&client)?;
    if version <= Version::current() {
        return Ok(None);
    }
    let installer = download(&client, version, dir, publish)?;
    Ok(Some((version, installer)))
}

fn latest_version(client: &Client) -> Result<Version, String> {
    let response = client.get(&format!("https://github.com/{REPO}/releases/latest"), false).map_err(github)?;
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
    let response = client.get(&format!("https://github.com/{REPO}/releases/download/v{version}/{name}"), true).map_err(github)?;
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
    }).map_err(github);
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
    // O build de debug tem a pasta dele: ao abrir, ele apagaria o instalador que a
    // versão instalada baixou (a do Cargo.toml já é a nova).
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(crate::platform::APP_NAME)
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

/// As mensagens do `http` falam de "servidor"; aqui o servidor é sempre o GitHub.
fn github(e: String) -> String {
    e.replace("o servidor", "o GitHub")
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
        let url = "https://github.com/FernandoGarciaRangel/process-monitor/releases/tag/v0.3.0";
        assert_eq!(tag_version(url), Some(Version(0, 3, 0)));
        assert_eq!(tag_version("https://github.com/x/y/releases"), None);
    }

    /// Acessa a rede: `cargo test -- --ignored`. Usa um repo público qualquer para
    /// exercitar o WinHTTP, o redirecionamento sem seguir e o download com redirect.
    #[test]
    #[ignore]
    fn winhttp_against_github() {
        let client = Client::new([10_000, 10_000, 30_000, 30_000]).unwrap();
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
