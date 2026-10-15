//! Ação como administrador, sem elevar o app inteiro. Quando uma ação esbarra em
//! "acesso negado", o aviso oferece repeti-la: uma cópia do exe abre elevada (UAC) com
//! `--elevated=<tarefa>`, faz só aquilo e sai com o resultado no código de saída.
//!
//! O app continua comum: o que ele abre (terminal, servidor reiniciado) não herda a
//! elevação, arrastar do Explorer continua funcionando e a entrada de inicialização vale.

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::control::{self, Priority};
use crate::platform::{self, KillError};
use crate::ports::{self, Connection, TcpState};
use crate::sampler::ProcKey;
use crate::services::{self, StartType};
use crate::startup::{self, Source};

pub const ARG: &str = "--elevated=";

/// Um processo como a cópia elevada o confere: PID, hora de criação e executável
/// (a proteção de processos críticos vai pelo nome).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub key: ProcKey,
    pub exe: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Task {
    /// Na ordem dada (numa árvore, filhos antes dos pais).
    Kill(Vec<Target>),
    Suspend(Target),
    Resume(Target),
    Priority(Target, Priority),
    Affinity(Target, u64),
    Efficiency(Target, bool),
    CloseConnection { local: SocketAddr, remote: SocketAddr },
    Service { name: String, action: services::Action },
    Startup { source: Source, name: String, on: bool },
}

impl Task {
    /// Executa na cópia elevada. O código de saída é quantas partes falharam (0 = tudo
    /// certo); processo que já tinha encerrado conta como certo.
    pub fn run(&self) -> u32 {
        let ok = |r: Result<(), KillError>| u32::from(!matches!(r, Ok(()) | Err(KillError::NotFound)));
        match self {
            Task::Kill(targets) => targets.iter().map(|t| ok(platform::kill(t.key.pid, t.key.start, &t.exe))).sum(),
            Task::Suspend(t) => ok(control::suspend(t.key, &t.exe)),
            Task::Resume(t) => ok(control::resume(t.key, &t.exe)),
            Task::Priority(t, p) => ok(control::set_priority(t.key, &t.exe, *p)),
            Task::Affinity(t, mask) => ok(control::set_affinity(t.key, &t.exe, *mask)),
            Task::Efficiency(t, on) => ok(control::set_efficiency(t.key, &t.exe, *on)),
            Task::CloseConnection { local, remote } => {
                let c = Connection { pid: 0, local: *local, remote: *remote, state: TcpState::Established };
                u32::from(ports::close_connection(&c).is_err())
            }
            Task::Service { name, action } => u32::from(services::perform(name, *action).is_err()),
            Task::Startup { source, name, on } => u32::from(startup::set_enabled_raw(*source, name, *on).is_err()),
        }
    }

    /// A tarefa num argumento só. Os campos vão em hexadecimal: nome de serviço e de
    /// entrada de inicialização podem ter espaço, aspas e qualquer outra coisa.
    pub fn encode(&self) -> String {
        let target = |t: &Target| vec![t.key.pid.to_string(), t.key.start.to_string(), t.exe.clone()];
        let fields: Vec<String> = match self {
            Task::Kill(targets) => {
                let mut f = vec!["kill".to_owned()];
                f.extend(targets.iter().flat_map(target));
                f
            }
            Task::Suspend(t) => [vec!["suspend".to_owned()], target(t)].concat(),
            Task::Resume(t) => [vec!["resume".to_owned()], target(t)].concat(),
            Task::Priority(t, p) => [vec!["priority".to_owned()], target(t), vec![priority_code(*p).to_string()]].concat(),
            Task::Affinity(t, mask) => [vec!["affinity".to_owned()], target(t), vec![mask.to_string()]].concat(),
            Task::Efficiency(t, on) => [vec!["efficiency".to_owned()], target(t), vec![flag(*on)]].concat(),
            Task::CloseConnection { local, remote } => vec!["close".to_owned(), local.to_string(), remote.to_string()],
            Task::Service { name, action } => {
                let (kind, start) = match action {
                    services::Action::Start => ("start", None),
                    services::Action::Stop => ("stop", None),
                    services::Action::Restart => ("restart", None),
                    services::Action::SetStart(s) => ("set-start", Some(start_code(*s))),
                };
                let mut f = vec!["service".to_owned(), name.clone(), kind.to_owned()];
                f.extend(start.map(|s| s.to_string()));
                f
            }
            Task::Startup { source, name, on } => {
                vec!["startup".to_owned(), source_code(*source).to_string(), name.clone(), flag(*on)]
            }
        };
        fields.iter().map(|f| hex(f.as_bytes())).collect::<Vec<_>>().join(".")
    }

    pub fn decode(text: &str) -> Option<Task> {
        let fields: Vec<String> = text.split('.').map(unhex).collect::<Option<_>>()?;
        let (kind, rest) = fields.split_first()?;
        let target = |f: &[String]| -> Option<Target> {
            Some(Target { key: ProcKey { pid: f.first()?.parse().ok()?, start: f.get(1)?.parse().ok()? }, exe: f.get(2)?.clone() })
        };
        Some(match kind.as_str() {
            "kill" if !rest.is_empty() && rest.len() % 3 == 0 => {
                Task::Kill(rest.chunks(3).map(target).collect::<Option<_>>()?)
            }
            "suspend" => Task::Suspend(target(rest)?),
            "resume" => Task::Resume(target(rest)?),
            "priority" => Task::Priority(target(rest)?, *Priority::ALL.get(rest.get(3)?.parse::<usize>().ok()?)?),
            "affinity" => Task::Affinity(target(rest)?, rest.get(3)?.parse().ok()?),
            "efficiency" => Task::Efficiency(target(rest)?, rest.get(3)? == "1"),
            "close" => Task::CloseConnection { local: rest.first()?.parse().ok()?, remote: rest.get(1)?.parse().ok()? },
            "service" => {
                let action = match rest.get(1)?.as_str() {
                    "start" => services::Action::Start,
                    "stop" => services::Action::Stop,
                    "restart" => services::Action::Restart,
                    "set-start" => services::Action::SetStart(*START_TYPES.get(rest.get(2)?.parse::<usize>().ok()?)?),
                    _ => return None,
                };
                Task::Service { name: rest.first()?.clone(), action }
            }
            "startup" => Task::Startup {
                source: *SOURCES.get(rest.first()?.parse::<usize>().ok()?)?,
                name: rest.get(1)?.clone(),
                on: rest.get(2)? == "1",
            },
            _ => return None,
        })
    }
}

const START_TYPES: [StartType; 5] =
    [StartType::Auto, StartType::AutoDelayed, StartType::Manual, StartType::Disabled, StartType::System];
const SOURCES: [Source; 5] =
    [Source::UserRun, Source::MachineRun, Source::MachineRun32, Source::UserFolder, Source::CommonFolder];

fn priority_code(p: Priority) -> usize {
    Priority::ALL.iter().position(|x| *x == p).unwrap_or(2)
}

fn start_code(s: StartType) -> usize {
    START_TYPES.iter().position(|x| *x == s).unwrap_or(2)
}

fn source_code(s: Source) -> usize {
    SOURCES.iter().position(|x| *x == s).unwrap_or(0)
}

fn flag(on: bool) -> String {
    if on { "1" } else { "0" }.to_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<String> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> =
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

/// Na cópia elevada: executa a tarefa da linha de comando e devolve o código de saída.
/// `None` quando o app não foi aberto para isso.
pub fn run_from_args(args: &[String]) -> Option<i32> {
    let encoded = args.iter().find_map(|a| a.strip_prefix(ARG))?;
    // Tarefa ilegível: código alto, para o app não confundir com "uma parte falhou".
    Some(Task::decode(encoded).map_or(255, |task| task.run() as i32))
}

/// O que fazer com o resultado: o aviso de sucesso e, num reinício de servidor, o
/// comando a abrir depois.
#[derive(Clone, Debug)]
pub struct Request {
    pub task: Task,
    pub success: String,
    pub then: Option<crate::app::Relaunch>,
}

/// `Ok(falhas)` com a quantidade de partes que falharam mesmo como administrador.
pub type Outcome = (Request, Result<u32, String>);

/// Pede o UAC numa thread: a janela continua desenhando enquanto ele está aberto.
pub struct Elevator {
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
}

impl Default for Elevator {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }
}

impl Elevator {
    pub fn run(&self, ctx: &egui::Context, request: Request) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("elevate".into())
            .spawn(move || {
                let args = format!("{ARG}{}", request.task.encode());
                // Sem janela dona: a do app é de outra thread. O clique acabou de ser
                // nela, então o UAC abre na frente mesmo assim.
                let result = platform::run_elevated(&args, 0);
                let _ = tx.send((request, result));
                ctx.request_repaint();
            })
            .expect("thread de elevação");
    }

    pub fn take(&self) -> Vec<Outcome> {
        self.rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pid: u32) -> Target {
        Target { key: ProcKey { pid, start: 1_760_000_000 + u64::from(pid) }, exe: "Meu App (x86).exe".to_owned() }
    }

    #[test]
    fn ida_e_volta_pela_linha_de_comando() {
        let tasks = [
            Task::Kill(vec![t(10), t(20), t(30)]),
            Task::Suspend(t(1)),
            Task::Resume(t(2)),
            Task::Priority(t(3), Priority::High),
            Task::Affinity(t(4), 0b1010),
            Task::Efficiency(t(5), true),
            Task::CloseConnection { local: "127.0.0.1:52319".parse().unwrap(), remote: "10.0.0.2:443".parse().unwrap() },
            Task::Service { name: "Spooler".to_owned(), action: services::Action::Restart },
            Task::Service { name: "My Svc".to_owned(), action: services::Action::SetStart(StartType::AutoDelayed) },
            Task::Startup { source: Source::CommonFolder, name: "Atalho \"esquisito\": ç.lnk".to_owned(), on: false },
        ];
        for task in tasks {
            let encoded = task.encode();
            assert!(encoded.chars().all(|c| c.is_ascii_hexdigit() || c == '.'), "{encoded}");
            assert_eq!(Task::decode(&encoded), Some(task));
        }
    }

    #[test]
    fn argumento_estragado_nao_vira_tarefa() {
        assert_eq!(Task::decode(""), None);
        assert_eq!(Task::decode("zz"), None);
        assert_eq!(Task::decode(&hex(b"kill")), None, "kill sem alvo");
        assert_eq!(Task::decode(&[hex(b"priority"), hex(b"1"), hex(b"2"), hex(b"x.exe"), hex(b"9")].join(".")), None);
        assert_eq!(run_from_args(&["--minimized".to_owned()]), None);
        assert_eq!(run_from_args(&[format!("{ARG}zz")]), Some(255));
    }

    #[test]
    fn processo_que_ja_encerrou_conta_como_certo() {
        // Sem elevação mesmo: o alvo não existe, e "já tinha encerrado" não é falha.
        let gone = Target { key: ProcKey { pid: 0xFFFF_FFF0, start: 1 }, exe: "nada.exe".to_owned() };
        assert_eq!(Task::Kill(vec![gone]).run(), 0);
    }
}
