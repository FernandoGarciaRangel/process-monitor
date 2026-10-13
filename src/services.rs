//! Serviços do Windows: lista, iniciar, parar, reiniciar e tipo de inicialização.
//!
//! Uma thread própria, criada na primeira vez que a tela abre, relê a lista a cada
//! `REFRESH` só enquanto a tela está à vista. A configuração (tipo de inicialização,
//! descrição) quase nunca muda e custa uma chamada por serviço: fica em cache e só é
//! lida de novo depois de alterada aqui.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_DEPENDENT_SERVICES_RUNNING, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, ENUM_SERVICE_STATUS_PROCESSW,
    ENUM_SERVICE_TYPE, EnumServicesStatusExW, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW,
    QueryServiceConfig2W, QueryServiceConfigW, QueryServiceStatus, SC_ENUM_PROCESS_INFO, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACCEPT_STOP, SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONTROL_STOP,
    SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_DISABLED, SERVICE_ERROR,
    SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_START_PENDING, SERVICE_START_TYPE, SERVICE_STATE_ALL, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOP_PENDING,
    SERVICE_STOPPED, SERVICE_WIN32, StartServiceW,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};

/// De quanto em quanto tempo a lista é relida com a tela aberta.
const REFRESH: Duration = Duration::from_secs(3);
/// Quanto esperar um serviço parar antes de iniciá-lo de novo, no reiniciar.
const STOP_WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    Starting,
    Stopping,
    Running,
    /// Pausado ou mudando de pausa: raro, não vale distinguir.
    Paused,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Stopped => "Parado",
            State::Starting => "Iniciando…",
            State::Stopping => "Parando…",
            State::Running => "Em execução",
            State::Paused => "Pausado",
        }
    }

    fn from_raw(raw: u32) -> Self {
        match raw {
            x if x == SERVICE_STOPPED.0 => State::Stopped,
            x if x == SERVICE_START_PENDING.0 => State::Starting,
            x if x == SERVICE_STOP_PENDING.0 => State::Stopping,
            x if x == SERVICE_RUNNING.0 => State::Running,
            _ => State::Paused,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartType {
    Auto,
    AutoDelayed,
    Manual,
    Disabled,
    /// Inicialização do kernel (drivers): não se mexe por aqui.
    System,
}

impl StartType {
    /// As que dá para escolher.
    pub const CHOICES: [StartType; 4] = [StartType::Auto, StartType::AutoDelayed, StartType::Manual, StartType::Disabled];

    pub fn label(self) -> &'static str {
        match self {
            StartType::Auto => "Automático",
            StartType::AutoDelayed => "Automático (atrasado)",
            StartType::Manual => "Manual",
            StartType::Disabled => "Desativado",
            StartType::System => "Sistema",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Service {
    /// Nome interno (`Spooler`): é o que as chamadas usam.
    pub name: String,
    /// Nome de exibição, localizado (`Spooler de Impressão`).
    pub display: String,
    pub state: State,
    /// 0 quando parado.
    pub pid: u32,
    /// Aceita o pedido de parar agora.
    pub can_stop: bool,
    pub start: Option<StartType>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct Config {
    start: Option<StartType>,
    description: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    SetStart(StartType),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    AccessDenied,
    Other(String),
}

/// Resultado de uma ação, para o aviso na tela: nome de exibição e o que houve.
pub type Outcome = (String, Action, Result<(), Error>);

pub type Listing = Result<Vec<Service>, String>;

enum Msg {
    Active(bool),
    Refresh,
}

/// Dono da thread de leitura. As ações rodam numa thread cada, porque parar um
/// serviço pode levar segundos.
pub struct Manager {
    latest: Arc<Mutex<Option<Arc<Listing>>>>,
    tx: Sender<Msg>,
    outcome_tx: Sender<Outcome>,
    outcomes: Receiver<Outcome>,
    configs: Arc<Mutex<HashMap<String, Config>>>,
    active: bool,
}

impl Manager {
    pub fn spawn(ctx: egui::Context) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let configs: Arc<Mutex<HashMap<String, Config>>> = Arc::default();
        let (tx, rx) = mpsc::channel();
        let (outcome_tx, outcomes) = mpsc::channel();
        let (shared, cache) = (latest.clone(), configs.clone());
        std::thread::Builder::new()
            .name("services".into())
            .spawn(move || run(ctx, shared, cache, rx))
            .expect("thread de serviços");
        Self { latest, tx, outcome_tx, outcomes, configs, active: false }
    }

    pub fn latest(&self) -> Option<Arc<Listing>> {
        self.latest.lock().ok()?.clone()
    }

    /// Relê só com a tela à vista.
    pub fn set_active(&mut self, active: bool) {
        if active != self.active {
            self.active = active;
            let _ = self.tx.send(Msg::Active(active));
        }
    }

    pub fn take_outcomes(&self) -> Vec<Outcome> {
        self.outcomes.try_iter().collect()
    }

    pub fn act(&self, ctx: &egui::Context, name: String, display: String, action: Action) {
        let (out, tx, configs, ctx) = (self.outcome_tx.clone(), self.tx.clone(), self.configs.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("service-action".into())
            .spawn(move || {
                let result = perform(&name, action);
                if matches!(action, Action::SetStart(_))
                    && let Ok(mut c) = configs.lock()
                {
                    c.remove(&name);
                }
                let _ = out.send((display, action, result));
                let _ = tx.send(Msg::Refresh);
                ctx.request_repaint();
            })
            .expect("thread de ação de serviço");
        // Mostra "Iniciando…" e "Parando…" já, sem esperar a ação terminar.
        let _ = self.tx.send(Msg::Refresh);
    }
}

fn run(ctx: egui::Context, latest: Arc<Mutex<Option<Arc<Listing>>>>, configs: Arc<Mutex<HashMap<String, Config>>>, rx: Receiver<Msg>) {
    let mut active = false;
    let mut next = Instant::now();
    loop {
        let msg = if active {
            rx.recv_timeout(next.saturating_duration_since(Instant::now()))
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match msg {
            Ok(Msg::Active(a)) => {
                active = a;
                if !active {
                    continue;
                }
            }
            Ok(Msg::Refresh) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let listing = match configs.lock() {
            Ok(mut cache) => list(&mut cache),
            Err(_) => Err("cache de serviços inválido".to_owned()),
        };
        if let Ok(mut slot) = latest.lock() {
            *slot = Some(Arc::new(listing));
        }
        ctx.request_repaint();
        next = Instant::now() + REFRESH;
    }
}

struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

/// Todos os serviços Win32, com a configuração do cache (lida para os que faltam).
fn list(cache: &mut HashMap<String, Config>) -> Listing {
    unsafe {
        let scm = Handle(
            OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)
                .map_err(|e| e.message())?,
        );
        let mut buf: Vec<u8> = Vec::new();
        let mut needed = 0u32;
        let mut count = 0u32;
        // Primeiro só o tamanho; o buffer cresce se aparecer serviço novo entre as chamadas.
        for _ in 0..4 {
            let mut resume = 0u32;
            let result = EnumServicesStatusExW(
                scm.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                (!buf.is_empty()).then_some(buf.as_mut_slice()),
                &mut needed,
                &mut count,
                Some(&mut resume),
                PCWSTR::null(),
            );
            match result {
                Ok(()) => break,
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => buf = vec![0u8; buf.len() + needed as usize + 4096],
                Err(e) => return Err(e.message()),
            }
        }
        let entries = std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let name = wide(e.lpServiceName);
            let status = &e.ServiceStatusProcess;
            let config = cache.entry(name.clone()).or_insert_with(|| read_config(&scm, &name)).clone();
            out.push(Service {
                display: wide(e.lpDisplayName),
                state: State::from_raw(status.dwCurrentState.0),
                pid: status.dwProcessId,
                can_stop: status.dwControlsAccepted & SERVICE_ACCEPT_STOP != 0,
                start: config.start,
                description: config.description,
                name,
            });
        }
        // Serviços removidos não ficam no cache.
        if cache.len() > out.len() {
            cache.retain(|n, _| out.iter().any(|s| &s.name == n));
        }
        Ok(out)
    }
}

unsafe fn read_config(scm: &Handle, name: &str) -> Config {
    unsafe {
        let Ok(service) = OpenServiceW(scm.0, &HSTRING::from(name), SERVICE_QUERY_CONFIG) else { return Config::default() };
        let service = Handle(service);
        let mut config = Config::default();
        let mut needed = 0u32;
        let _ = QueryServiceConfigW(service.0, None, 0, &mut needed);
        if needed > 0 {
            // u64 para o alinhamento da estrutura no começo do buffer.
            let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
            let ptr = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
            if QueryServiceConfigW(service.0, Some(ptr), needed, &mut needed).is_ok() {
                let raw = &*ptr;
                config.start = Some(match raw.dwStartType {
                    t if t == SERVICE_AUTO_START => {
                        if delayed(&service) { StartType::AutoDelayed } else { StartType::Auto }
                    }
                    t if t == SERVICE_DEMAND_START => StartType::Manual,
                    t if t == SERVICE_DISABLED => StartType::Disabled,
                    _ => StartType::System,
                });
            }
        }
        config.description = config2(&service, SERVICE_CONFIG_DESCRIPTION).and_then(|buf| {
            let d = &*(buf.as_ptr() as *const SERVICE_DESCRIPTIONW);
            if d.lpDescription.is_null() { None } else { Some(wide(d.lpDescription)) }
        });
        // Descrição de serviço às vezes é uma referência de recurso não resolvida.
        config.description = config.description.filter(|d| !d.is_empty() && !d.starts_with('@'));
        config
    }
}

unsafe fn delayed(service: &Handle) -> bool {
    unsafe {
        config2(service, SERVICE_CONFIG_DELAYED_AUTO_START_INFO)
            .is_some_and(|buf| (*(buf.as_ptr() as *const SERVICE_DELAYED_AUTO_START_INFO)).fDelayedAutostart.as_bool())
    }
}

/// `QueryServiceConfig2W` com o buffer do tamanho pedido. O buffer é de u64 para o
/// alinhamento da estrutura que vem no começo.
unsafe fn config2(service: &Handle, level: windows::Win32::System::Services::SERVICE_CONFIG) -> Option<Vec<u64>> {
    unsafe {
        let mut needed = 0u32;
        match QueryServiceConfig2W(service.0, level, None, &mut needed) {
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() && needed > 0 => {}
            _ => return None,
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let bytes = std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 8);
        QueryServiceConfig2W(service.0, level, Some(bytes), &mut needed).ok()?;
        Some(buf)
    }
}

fn perform(name: &str, action: Action) -> Result<(), Error> {
    let access = match action {
        Action::Start => SERVICE_START | SERVICE_QUERY_STATUS,
        Action::Stop => SERVICE_STOP | SERVICE_QUERY_STATUS,
        Action::Restart => SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS,
        Action::SetStart(_) => SERVICE_CHANGE_CONFIG,
    };
    unsafe {
        let scm = Handle(OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT).map_err(error)?);
        let service = Handle(OpenServiceW(scm.0, &HSTRING::from(name), access).map_err(error)?);
        match action {
            Action::Start => start(&service),
            Action::Stop => stop(&service),
            Action::Restart => {
                stop(&service)?;
                let deadline = Instant::now() + STOP_WAIT;
                loop {
                    let mut status = SERVICE_STATUS::default();
                    QueryServiceStatus(service.0, &mut status).map_err(error)?;
                    if status.dwCurrentState == SERVICE_STOPPED {
                        break;
                    }
                    if Instant::now() > deadline {
                        return Err(Error::Other("não parou a tempo".to_owned()));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                start(&service)
            }
            Action::SetStart(kind) => set_start(&service, kind),
        }
    }
}

unsafe fn start(service: &Handle) -> Result<(), Error> {
    match unsafe { StartServiceW(service.0, None) } {
        Err(e) if e.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => Ok(()),
        other => other.map_err(error),
    }
}

unsafe fn stop(service: &Handle) -> Result<(), Error> {
    let mut status = SERVICE_STATUS::default();
    match unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } {
        Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => Ok(()),
        Err(e) if e.code() == ERROR_DEPENDENT_SERVICES_RUNNING.to_hresult() => {
            Err(Error::Other("outros serviços em execução dependem dele".to_owned()))
        }
        other => other.map_err(error),
    }
}

unsafe fn set_start(service: &Handle, kind: StartType) -> Result<(), Error> {
    let raw: SERVICE_START_TYPE = match kind {
        StartType::Auto | StartType::AutoDelayed => SERVICE_AUTO_START,
        StartType::Manual => SERVICE_DEMAND_START,
        StartType::Disabled => SERVICE_DISABLED,
        StartType::System => return Err(Error::Other("tipo não suportado".to_owned())),
    };
    unsafe {
        ChangeServiceConfigW(
            service.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            raw,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
        .map_err(error)?;
        if matches!(kind, StartType::Auto | StartType::AutoDelayed) {
            let info = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: BOOL::from(kind == StartType::AutoDelayed) };
            ChangeServiceConfig2W(service.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(&info as *const _ as *const _))
                .map_err(error)?;
        }
        Ok(())
    }
}

fn error(e: windows::core::Error) -> Error {
    if e.code() == ERROR_ACCESS_DENIED.to_hresult() { Error::AccessDenied } else { Error::Other(e.message()) }
}

impl Error {
    pub fn describe(&self) -> &str {
        match self {
            Error::AccessDenied => "acesso negado",
            Error::Other(msg) => msg,
        }
    }
}

impl Action {
    /// "iniciado", "parado"… para o aviso de sucesso.
    pub fn done(self) -> String {
        match self {
            Action::Start => "iniciado".to_owned(),
            Action::Stop => "parado".to_owned(),
            Action::Restart => "reiniciado".to_owned(),
            Action::SetStart(kind) => format!("agora inicia como {}", kind.label().to_lowercase()),
        }
    }

    /// "iniciar", "parar"… para o aviso de erro.
    pub fn verb(self) -> &'static str {
        match self {
            Action::Start => "iniciar",
            Action::Stop => "parar",
            Action::Restart => "reiniciar",
            Action::SetStart(_) => "mudar a inicialização de",
        }
    }
}

fn wide(p: PWSTR) -> String {
    if p.is_null() { String::new() } else { unsafe { p.to_string().unwrap_or_default() } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lista_os_servicos() {
        let mut cache = HashMap::new();
        let services = list(&mut cache).unwrap();
        assert!(services.len() > 20, "{} serviços", services.len());
        // O agendador de tarefas existe e roda em toda instalação do Windows.
        let schedule = services.iter().find(|s| s.name.eq_ignore_ascii_case("Schedule")).unwrap();
        assert_eq!(schedule.state, State::Running);
        assert!(schedule.pid > 0);
        assert!(schedule.start.is_some());
        // Na segunda leitura a configuração vem do cache.
        let again = list(&mut cache).unwrap();
        assert_eq!(again.len(), services.len());
    }
}
