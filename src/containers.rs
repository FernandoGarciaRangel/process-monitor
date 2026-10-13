//! Quem está por trás das portas do Docker e do WSL. No Windows, a porta publicada por
//! um container é escutada pelo `com.docker.backend.exe`, e a de um servidor dentro do
//! WSL pelo `wslrelay.exe`: na lista apareceria só isso. Aqui se descobre o container
//! (`docker ps`) e as distros em execução (`wsl -l --running`).
//!
//! Só roda com a tela de portas aberta e alguma porta desses processos na lista, numa
//! thread por leitura, no máximo a cada `REFRESH`. Sem Docker instalado ou parado, a
//! leitura falha calada e espera mais (`BACKOFF`) para tentar de novo.

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(10);
const BACKOFF: Duration = Duration::from_secs(60);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Processos do Docker Desktop que escutam as portas publicadas.
const DOCKER_PROCESSES: &[&str] = &["com.docker.backend.exe", "docker-proxy.exe", "vpnkit.exe", "com.docker.proxy.exe"];

pub fn is_docker(exe: &str) -> bool {
    DOCKER_PROCESSES.iter().any(|d| d.eq_ignore_ascii_case(exe))
}

pub fn is_wsl(exe: &str) -> bool {
    exe.eq_ignore_ascii_case("wslrelay.exe")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    pub name: String,
    pub image: String,
}

#[derive(Clone, Debug, Default)]
pub struct Info {
    /// Porta do Windows → container que a publica.
    pub docker: HashMap<u16, Container>,
    /// Distros do WSL em execução, sem as internas do Docker Desktop.
    pub wsl: Vec<String>,
}

impl Info {
    /// Rótulo para uma porta do `wslrelay`: a distro, se só uma estiver rodando.
    pub fn wsl_label(&self) -> String {
        match self.wsl.as_slice() {
            [one] => format!("{one} (WSL)"),
            _ => "WSL".to_owned(),
        }
    }
}

pub struct Watcher {
    latest: Arc<Mutex<Arc<Info>>>,
    pending: bool,
    next: Instant,
    done_tx: Sender<bool>,
    done_rx: Receiver<bool>,
    stop_tx: Sender<(String, Result<(), String>)>,
    stop_rx: Receiver<(String, Result<(), String>)>,
    ctx: egui::Context,
}

impl Watcher {
    pub fn new(ctx: egui::Context) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        Self { latest: Arc::default(), pending: false, next: Instant::now(), done_tx, done_rx, stop_tx, stop_rx, ctx }
    }

    pub fn latest(&self) -> Arc<Info> {
        self.latest.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Chamada a cada quadro. `wanted`: a tela de portas está aberta e há porta do
    /// Docker ou do WSL na lista.
    pub fn poll(&mut self, wanted: bool) {
        while let Ok(ok) = self.done_rx.try_recv() {
            self.pending = false;
            self.next = Instant::now() + if ok { REFRESH } else { BACKOFF };
        }
        if !wanted || self.pending || Instant::now() < self.next {
            return;
        }
        self.pending = true;
        let (latest, done, ctx) = (self.latest.clone(), self.done_tx.clone(), self.ctx.clone());
        std::thread::Builder::new()
            .name("containers".into())
            .spawn(move || {
                let docker = run("docker", &["ps", "--format", "{{.Names}}\t{{.Image}}\t{{.Ports}}"]).map(|t| parse_docker_ps(&t));
                let wsl = run_utf16("wsl.exe", &["-l", "--running", "-q"]).map(|t| parse_distros(&t)).unwrap_or_default();
                let ok = docker.is_some() || !wsl.is_empty();
                if let Ok(mut slot) = latest.lock() {
                    *slot = Arc::new(Info { docker: docker.unwrap_or_default(), wsl });
                }
                let _ = done.send(ok);
                ctx.request_repaint();
            })
            .expect("thread de containers");
    }

    /// `docker stop`, numa thread: parar leva até 10 s. O resultado sai em `take_stopped`.
    pub fn stop(&mut self, name: String) {
        let (tx, ctx) = (self.stop_tx.clone(), self.ctx.clone());
        std::thread::Builder::new()
            .name("docker-stop".into())
            .spawn(move || {
                let result = Command::new("docker")
                    .args(["stop", &name])
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| e.to_string())
                    .and_then(|o| {
                        if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_owned()) }
                    });
                let _ = tx.send((name, result));
                ctx.request_repaint();
            })
            .expect("thread de docker stop");
        // Relê logo depois de parar, para a porta sumir.
        self.next = Instant::now();
    }

    pub fn take_stopped(&self) -> Vec<(String, Result<(), String>)> {
        self.stop_rx.try_iter().collect()
    }
}

fn command(exe: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(exe).args(args).creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null()).output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn run(exe: &str, args: &[&str]) -> Option<String> {
    command(exe, args).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// O `wsl.exe` escreve em UTF-16.
fn run_utf16(exe: &str, args: &[&str]) -> Option<String> {
    let bytes = command(exe, args)?;
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    Some(String::from_utf16_lossy(&units))
}

/// Linhas `nome\timagem\tportas`. Portas como o Docker escreve:
/// `0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp, 8000-8001->8000-8001/tcp, 6379/tcp`.
/// Só as publicadas (com `->`) contam; a exposta sem publicar não abre porta no Windows.
fn parse_docker_ps(text: &str) -> HashMap<u16, Container> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(name), Some(image), Some(ports)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let container = Container { name: name.trim().to_owned(), image: image.trim().to_owned() };
        for mapping in ports.split(',') {
            let Some((host, _)) = mapping.trim().split_once("->") else { continue };
            let Some((_, range)) = host.rsplit_once(':') else { continue };
            let (start, end) = match range.split_once('-') {
                Some((a, b)) => (a.parse::<u16>(), b.parse::<u16>()),
                None => (range.parse(), range.parse()),
            };
            if let (Ok(start), Ok(end)) = (start, end) {
                for port in start..=end.min(start.saturating_add(256)) {
                    out.insert(port, container.clone());
                }
            }
        }
    }
    out
}

/// Uma distro por linha; as internas do Docker Desktop ficam de fora.
fn parse_distros(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\0' || c == '\u{feff}').to_owned())
        .filter(|l| !l.is_empty() && !l.to_ascii_lowercase().starts_with("docker-desktop"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portas_do_docker_ps() {
        let text = "db\tpostgres:16\t0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp\n\
                    cache\tredis:7\t6379/tcp\n\
                    web\tnginx\t0.0.0.0:8080-8081->80-81/tcp\n\
                    sem-portas\talpine\t\n";
        let ports = parse_docker_ps(text);
        assert_eq!(ports[&5432], Container { name: "db".into(), image: "postgres:16".into() });
        assert_eq!(ports[&8080].name, "web");
        assert_eq!(ports[&8081].name, "web");
        assert!(!ports.contains_key(&6379), "exposta sem publicar não abre porta no Windows");
        assert_eq!(ports.len(), 3);
    }

    #[test]
    fn distros_do_wsl() {
        assert_eq!(parse_distros("Ubuntu\r\ndocker-desktop\r\n\r\n"), vec!["Ubuntu"]);
        let one = Info { docker: HashMap::new(), wsl: vec!["Ubuntu".into()] };
        assert_eq!(one.wsl_label(), "Ubuntu (WSL)");
        assert_eq!(Info::default().wsl_label(), "WSL");
    }

    #[test]
    fn processos_do_docker_e_do_wsl() {
        assert!(is_docker("com.docker.backend.exe"));
        assert!(is_wsl("wslrelay.exe"));
        assert!(!is_docker("node.exe"));
    }
}
