//! Estado do aplicativo e montagem da janela: barra lateral, conteúdo e detalhes.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, Frame, Id, Margin, RichText, Stroke, vec2};
use egui_phosphor::regular as icon;

use crate::icons::IconCache;
use crate::{containers, control, export, services};
use crate::platform::{self, KillError};
use crate::ports::{self, Kind, PortRange, Prober, Statuses, Target};
use crate::sampler::{ProcInfo, ProcKey, Sampler, Series, Snapshot};
use crate::theme::{self, GRAPHITE_0, GRAPHITE_3, HAIRLINE, HAIRLINE_STRONG, INK, INK_2, INK_3, R_LG, R_MD};
use crate::ui;
use crate::alerts;
use crate::tray::Tray;
use crate::ui::widgets;
use crate::updater::{self, Updater};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Processes,
    Services,
    Startup,
    Ports,
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu(usize),
}

impl View {
    /// Nome estável, guardado entre execuções.
    fn key(self) -> String {
        match self {
            View::Processes => "processes".to_owned(),
            View::Services => "services".to_owned(),
            View::Startup => "startup".to_owned(),
            View::Ports => "ports".to_owned(),
            View::Cpu => "cpu".to_owned(),
            View::Memory => "memory".to_owned(),
            View::Disk => "disk".to_owned(),
            View::Network => "network".to_owned(),
            View::Gpu(i) => format!("gpu:{i}"),
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "processes" => View::Processes,
            "services" => View::Services,
            "startup" => View::Startup,
            "ports" => View::Ports,
            "cpu" => View::Cpu,
            "memory" => View::Memory,
            "disk" => View::Disk,
            "network" => View::Network,
            _ => View::Gpu(key.strip_prefix("gpu:")?.parse().ok()?),
        })
    }

    /// Telas que dividem o item "Processos" da barra lateral, em abas.
    pub fn is_system(self) -> bool {
        matches!(self, View::Processes | View::Services | View::Startup)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Pid,
    User,
    Cpu,
    Memory,
    Disk,
    Gpu,
}

impl SortKey {
    const ALL: [SortKey; 7] =
        [SortKey::Name, SortKey::Pid, SortKey::User, SortKey::Cpu, SortKey::Memory, SortKey::Disk, SortKey::Gpu];

    fn key(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Pid => "pid",
            SortKey::User => "user",
            SortKey::Cpu => "cpu",
            SortKey::Memory => "memory",
            SortKey::Disk => "disk",
            SortKey::Gpu => "gpu",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortSort {
    Port,
    Name,
    Pid,
    Response,
    Memory,
    Uptime,
}

impl PortSort {
    const ALL: [PortSort; 6] =
        [PortSort::Port, PortSort::Name, PortSort::Pid, PortSort::Response, PortSort::Memory, PortSort::Uptime];

    fn key(self) -> &'static str {
        match self {
            PortSort::Port => "port",
            PortSort::Name => "name",
            PortSort::Pid => "pid",
            PortSort::Response => "response",
            PortSort::Memory => "memory",
            PortSort::Uptime => "uptime",
        }
    }
}

/// Pedido de finalização aguardando confirmação no modal.
#[derive(Clone, Debug)]
pub struct KillRequest {
    pub title: String,
    pub detail: String,
    /// Alvos na ordem de execução (numa árvore, filhos antes dos pais).
    pub targets: Vec<KillTarget>,
    /// Reiniciar: o servidor a abrir de novo depois de finalizar.
    pub then: Option<Relaunch>,
}

/// Comando a rodar de novo, numa janela de terminal nova, depois que a porta liberar.
#[derive(Clone, Debug)]
pub struct Relaunch {
    pub cmd: String,
    pub cwd: std::path::PathBuf,
    pub title: String,
    pub port: u16,
}

/// Quanto esperar a porta liberar antes de abrir o servidor de novo.
const RELAUNCH_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct KillTarget {
    pub key: ProcKey,
    /// Nome do executável: é o que a proteção de processos críticos confere.
    pub exe: String,
    /// Nome amigável, para as mensagens.
    pub label: String,
}

impl KillTarget {
    fn of(p: &crate::sampler::ProcInfo) -> Self {
        Self { key: p.key(), exe: p.name.clone(), label: p.display_name().to_owned() }
    }
}

/// Ajuste pedido no painel de detalhes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tune {
    Priority(control::Priority),
    /// Máscara de núcleos lógicos.
    Affinity(u64),
    Efficiency(bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Ok,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub offer_elevation: bool,
    pub until: Instant,
}

/// Pontos de histórico por processo: 10 minutos mesmo no intervalo de 0,5 s.
pub const PROC_HISTORY_LEN: usize = 1200;

/// CPU e memória de um processo desde que o app o viu, para o painel de detalhes.
pub struct ProcHistory {
    pub cpu: Series,
    pub memory: Series,
}

impl ProcHistory {
    fn new() -> Self {
        Self { cpu: Series::with_capacity(PROC_HISTORY_LEN), memory: Series::with_capacity(PROC_HISTORY_LEN) }
    }
}

/// Período dos gráficos do painel de detalhes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistorySpan {
    Minute,
    TenMinutes,
}

impl HistorySpan {
    pub fn label(self) -> &'static str {
        match self {
            HistorySpan::Minute => "1 min",
            HistorySpan::TenMinutes => "10 min",
        }
    }

    /// Pontos que cabem no período com o intervalo de coleta atual (no máximo o histórico).
    pub fn points(self, interval_ms: u64) -> usize {
        let secs = match self {
            HistorySpan::Minute => 60,
            HistorySpan::TenMinutes => 600,
        };
        ((secs * 1000 / interval_ms.max(1)) as usize).clamp(2, PROC_HISTORY_LEN)
    }
}

/// Quanto tempo os gráficos de desempenho mostram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryWindow {
    One,
    Five,
    Fifteen,
}

impl HistoryWindow {
    pub const ALL: [HistoryWindow; 3] = [HistoryWindow::One, HistoryWindow::Five, HistoryWindow::Fifteen];

    pub fn minutes(self) -> u64 {
        match self {
            HistoryWindow::One => 1,
            HistoryWindow::Five => 5,
            HistoryWindow::Fifteen => 15,
        }
    }

    pub fn duration(self) -> Duration {
        Duration::from_secs(self.minutes() * 60)
    }

    pub fn label(self) -> String {
        format!("{} min", self.minutes())
    }

    fn from_minutes(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.minutes().to_string() == text)
    }
}

pub struct App {
    pub sampler: Sampler,
    pub snap: Option<Arc<Snapshot>>,
    pub view: View,
    pub search: String,
    pub group: bool,
    pub expanded: HashSet<String>,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub selected: Option<ProcKey>,
    pub scroll_to_selected: bool,
    /// Posição de cada linha no quadro anterior, para desempatar a ordenação.
    pub prev_rank: HashMap<ProcKey, usize>,
    pub prev_group_rank: HashMap<String, usize>,
    /// Histórico de todos os processos vivos, alimentado a cada coleta: ao selecionar
    /// um processo, o gráfico já vem com os últimos minutos.
    pub histories: HashMap<ProcKey, ProcHistory>,
    pub history_span: HistorySpan,
    pub confirm: Option<KillRequest>,
    pub toasts: Vec<Toast>,
    /// Resultado dos reinícios, que rodam numa thread: nome do servidor ou o erro.
    relaunch_tx: mpsc::Sender<Result<String, String>>,
    relaunch_rx: mpsc::Receiver<Result<String, String>>,
    pub icons: IconCache,
    /// Janela "Quem está usando".
    pub locks: ui::locks::State,
    pub elevated: bool,
    pub interval_ms: u64,
    protected_cache: HashMap<ProcKey, bool>,

    pub prober: Prober,
    /// Resultado mais recente da sondagem HTTP, por (porta, PID).
    pub port_status: Arc<Statuses>,
    pub port_ranges: Vec<PortRange>,
    /// Como o usuário digitou as faixas; é o que se guarda entre execuções.
    pub port_ranges_text: String,
    /// Todas as portas em escuta, não só as de desenvolvimento e das faixas.
    pub ports_all: bool,
    pub port_search: String,
    pub port_sort: PortSort,
    pub port_sort_desc: bool,
    /// Porta escolhida na tela de portas; o processo dela fica em `selected`.
    pub selected_port: Option<u16>,
    pub scroll_to_port: bool,
    /// Portas de desenvolvimento e das faixas, para o contador da barra lateral.
    pub dev_port_count: usize,
    port_targets: Vec<Target>,
    prober_active: bool,

    pub updater: Updater,
    pub update_state: Arc<updater::State>,
    pub auto_update: bool,
    /// Versão cujo aviso foi dispensado nesta execução (continua nas configurações). Se
    /// sair outra, o aviso volta.
    pub update_dismissed: Option<updater::Version>,
    /// Versão já anunciada num balão do Windows: cada versão avisa uma vez.
    update_announced: Option<updater::Version>,
    pub settings_open: bool,

    pub tray: Tray,
    /// O X esconde na bandeja; sair de verdade só pelo menu do ícone.
    pub minimize_to_tray: bool,
    /// O balão "continua na bandeja" já apareceu uma vez.
    tray_hint_shown: bool,
    pub always_on_top: bool,
    /// Com a opção desligada, a tela de portas não manda nenhuma requisição.
    pub probe_http: bool,
    /// Colunas escondidas, como `tabela.coluna` (`processes.pid`, `ports.address`).
    pub hidden_columns: BTreeSet<String>,
    pub history_window: HistoryWindow,
    /// Entrada de inicialização do Windows: (ligada, abre minimizado). Lida do registro.
    pub autostart: (bool, bool),
    /// Registrado no lugar do Gerenciador de Tarefas (chave IFEO). Lido do registro.
    pub replaces_taskmgr: bool,
    /// Aberto pela inicialização do Windows com `--minimized`: some depois do 1º quadro.
    start_minimized: bool,
    /// Fechar de verdade (Sair, instalador, atualização, reinício como administrador).
    quitting: bool,
    pub alert_config: alerts::Config,
    /// Alvo do último alerta mostrado, escrito pela thread da coleta.
    last_alert: Arc<std::sync::Mutex<Option<alerts::Target>>>,

    /// Prioridade, núcleos e modo eficiência do selecionado, lidos uma vez por seleção.
    tuning: Option<(ProcKey, control::Tuning)>,
    /// Criado na primeira vez que a tela de serviços abre.
    pub services: Option<services::Manager>,
    pub service_search: String,
    pub selected_service: Option<String>,
    pub startup: ui::startup::State,
    /// Containers do Docker e distros do WSL por trás das portas, lidos com a tela aberta.
    pub containers: containers::Watcher,
    pub container_info: Arc<containers::Info>,
}

const INTERVAL_KEY: &str = "interval_ms";
const GROUP_KEY: &str = "group";
const PORT_RANGES_KEY: &str = "port_ranges";
const PORTS_ALL_KEY: &str = "ports_all";
const AUTO_UPDATE_KEY: &str = "auto_update";
const VIEW_KEY: &str = "view";
const HISTORY_SPAN_KEY: &str = "history_span";
const SORT_KEY: &str = "sort";
const SORT_DESC_KEY: &str = "sort_desc";
const PORT_SORT_KEY: &str = "port_sort";
const PORT_SORT_DESC_KEY: &str = "port_sort_desc";
const TRAY_KEY: &str = "minimize_to_tray";
const TRAY_HINT_KEY: &str = "tray_hint_shown";
const ON_TOP_KEY: &str = "always_on_top";
const PROBE_HTTP_KEY: &str = "probe_http";
const HIDDEN_COLUMNS_KEY: &str = "hidden_columns";
const HISTORY_WINDOW_KEY: &str = "history_window";

/// Tabelas com largura de coluna salva (ver `ui::columns`).
pub const TABLES: [&str; 4] = ["processes", "ports", "services", "startup"];

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, start_minimized: bool) -> Self {
        theme::apply(&cc.egui_ctx);
        let stored = |key: &str| cc.storage.and_then(|s| s.get_string(key));
        let interval_ms = stored(INTERVAL_KEY)
            .and_then(|v| v.parse().ok())
            .filter(|v| [500, 1000, 2000].contains(v))
            .unwrap_or(1000);
        let group = stored(GROUP_KEY).is_some_and(|v| v == "1");
        let port_ranges_text = stored(PORT_RANGES_KEY)
            .filter(|t| ports::parse_ranges(t).is_ok())
            .unwrap_or_else(|| ports::DEFAULT_RANGES.to_owned());
        // Ligado por padrão: só baixa; instalar continua sendo decisão do usuário.
        let auto_update = stored(AUTO_UPDATE_KEY).is_none_or(|v| v == "1");
        // A GPU salva pode não existir mais (placa trocada): `logic` confere no primeiro
        // snapshot e volta para os processos.
        let view = stored(VIEW_KEY).and_then(|v| View::from_key(&v)).unwrap_or(View::Processes);
        let sort = stored(SORT_KEY).and_then(|v| SortKey::ALL.into_iter().find(|k| k.key() == v)).unwrap_or(SortKey::Cpu);
        let port_sort =
            stored(PORT_SORT_KEY).and_then(|v| PortSort::ALL.into_iter().find(|k| k.key() == v)).unwrap_or(PortSort::Port);
        let minimize_to_tray = stored(TRAY_KEY).is_none_or(|v| v == "1");
        let history_window =
            stored(HISTORY_WINDOW_KEY).and_then(|v| HistoryWindow::from_minutes(&v)).unwrap_or(HistoryWindow::One);
        let hidden_columns = stored(HIDDEN_COLUMNS_KEY)
            .map(|v| v.split(',').filter(|c| !c.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default();
        let (relaunch_tx, relaunch_rx) = mpsc::channel();
        let alert_config = load_alert_config(&stored);
        let app = Self {
            sampler: Sampler::spawn(
                cc.egui_ctx.clone(),
                Duration::from_millis(interval_ms),
                history_window.duration(),
            ),
            snap: None,
            view,
            search: String::new(),
            group,
            expanded: HashSet::new(),
            sort,
            sort_desc: stored(SORT_DESC_KEY).is_none_or(|v| v == "1"),
            selected: None,
            scroll_to_selected: false,
            prev_rank: HashMap::new(),
            prev_group_rank: HashMap::new(),
            histories: HashMap::new(),
            history_span: if stored(HISTORY_SPAN_KEY).as_deref() == Some("1") {
                HistorySpan::Minute
            } else {
                HistorySpan::TenMinutes
            },
            confirm: None,
            toasts: Vec::new(),
            relaunch_tx,
            relaunch_rx,
            icons: IconCache::new(cc.egui_ctx.clone()),
            locks: ui::locks::State::new(cc.egui_ctx.clone()),
            elevated: platform::is_elevated(),
            interval_ms,
            protected_cache: HashMap::new(),
            prober: Prober::spawn(cc.egui_ctx.clone()),
            port_status: Arc::default(),
            port_ranges: ports::parse_ranges(&port_ranges_text).unwrap_or_default(),
            port_ranges_text,
            ports_all: stored(PORTS_ALL_KEY).is_some_and(|v| v == "1"),
            port_search: String::new(),
            port_sort,
            port_sort_desc: stored(PORT_SORT_DESC_KEY).is_some_and(|v| v == "1"),
            selected_port: None,
            scroll_to_port: false,
            dev_port_count: 0,
            port_targets: Vec::new(),
            prober_active: false,
            updater: Updater::spawn(cc.egui_ctx.clone(), auto_update),
            update_state: Arc::new(updater::State::Idle),
            auto_update,
            update_dismissed: None,
            update_announced: None,
            settings_open: false,
            tray: Tray::spawn(cc.egui_ctx.clone(), minimize_to_tray),
            minimize_to_tray,
            tray_hint_shown: stored(TRAY_HINT_KEY).is_some_and(|v| v == "1"),
            always_on_top: stored(ON_TOP_KEY).is_some_and(|v| v == "1"),
            probe_http: stored(PROBE_HTTP_KEY).is_none_or(|v| v == "1"),
            hidden_columns,
            history_window,
            autostart: read_autostart(),
            replaces_taskmgr: platform::replaces_task_manager(),
            start_minimized,
            quitting: false,
            alert_config,
            last_alert: Arc::default(),
            tuning: None,
            services: None,
            service_search: String::new(),
            selected_service: None,
            startup: ui::startup::State::new(cc.egui_ctx.clone()),
            containers: containers::Watcher::new(cc.egui_ctx.clone()),
            container_info: Arc::default(),
        };
        app.install_alerts();
        app
    }

    /// Os alertas são avaliados na thread da coleta e viram balões da bandeja. O alvo do
    /// último fica guardado para o clique no balão abrir o processo ou a porta.
    fn install_alerts(&self) {
        let notifier = self.tray.notifier();
        let last = self.last_alert.clone();
        self.sampler.set_alerts(self.alert_config.clone());
        self.sampler.set_alert_sink(Box::new(move |alert| {
            if let Ok(mut slot) = last.lock() {
                *slot = Some(alert.target);
            }
            notifier.notify(&alert.title, &alert.body);
        }));
    }

    pub fn set_alert_config(&mut self, config: alerts::Config) {
        if config != self.alert_config {
            self.alert_config = config;
            self.sampler.set_alerts(self.alert_config.clone());
        }
    }

    pub fn is_watched(&self, port: u16) -> bool {
        self.alert_config.watched_ports.contains(&port)
    }

    /// Liga ou desliga a vigilância de uma porta.
    pub fn toggle_watch(&mut self, port: u16) {
        let mut config = self.alert_config.clone();
        if let Some(i) = config.watched_ports.iter().position(|p| *p == port) {
            config.watched_ports.remove(i);
        } else {
            config.watched_ports.push(port);
            config.watched_ports.sort_unstable();
        }
        self.set_alert_config(config);
    }

    /// Clique no balão de um alerta: mostra o processo ou a porta do aviso.
    fn open_last_alert(&mut self) {
        let target = self.last_alert.lock().ok().and_then(|mut t| t.take());
        match target {
            Some(alerts::Target::Process(key)) => {
                self.view = View::Processes;
                self.select(Some(key));
                self.scroll_to_selected = true;
            }
            Some(alerts::Target::Port(port)) => {
                self.view = View::Ports;
                let owner = self.snap.as_ref().and_then(|s| s.listeners.iter().find(|l| l.port == port).map(|l| l.pid));
                let key = owner.and_then(|pid| self.snap.as_ref()?.processes.iter().find(|p| p.pid == pid).map(|p| p.key()));
                if let Some(key) = key {
                    self.select_port(key, port);
                    self.scroll_to_port = true;
                }
            }
            Some(alerts::Target::Disk) => self.view = View::Disk,
            None => {}
        }
    }

    pub fn set_minimize_to_tray(&mut self, on: bool) {
        self.minimize_to_tray = on;
        self.tray.set_icon(on);
    }

    pub fn set_always_on_top(&mut self, ctx: &egui::Context, on: bool) {
        self.always_on_top = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(window_level(on)));
    }

    pub fn set_history_window(&mut self, window: HistoryWindow) {
        self.history_window = window;
        self.sampler.set_history_window(window.duration());
    }

    /// Grava no registro; o estado mostrado é sempre o que ficou lá.
    pub fn set_autostart(&mut self, enabled: bool, minimized: bool) {
        if let Err(e) = platform::set_autostart(enabled, minimized) {
            self.toast(format!("Não alterou a inicialização com o Windows: {e}"), ToastKind::Error, false);
        }
        self.autostart = read_autostart();
    }

    /// Pede administrador e grava a chave IFEO; o estado mostrado é sempre o que ficou
    /// no registro, mesmo se o usuário cancelar o UAC.
    pub fn set_replace_task_manager(&mut self, on: bool) {
        match platform::request_replace_task_manager(on) {
            Ok(()) => {
                let msg = if on {
                    "O Process Monitor abre no lugar do Gerenciador de Tarefas"
                } else {
                    "Gerenciador de Tarefas padrão restaurado"
                };
                self.toast(msg.to_owned(), ToastKind::Ok, false);
            }
            Err(e) => self.toast(format!("Não alterou o Gerenciador de Tarefas: {e}"), ToastKind::Error, false),
        }
        self.replaces_taskmgr = platform::replaces_task_manager();
    }

    pub fn column_visible(&self, table: &str, column: &str) -> bool {
        !self.hidden_columns.contains(&format!("{table}.{column}"))
    }

    pub fn set_column_visible(&mut self, table: &str, column: &str, visible: bool) {
        let key = format!("{table}.{column}");
        if visible {
            self.hidden_columns.remove(&key);
        } else {
            self.hidden_columns.insert(key);
        }
        self.sort_by_visible_column();
    }

    pub fn set_probe_http(&mut self, on: bool) {
        self.probe_http = on;
        self.sort_by_visible_column();
    }

    /// Ordenar por uma coluna escondida deixaria a tabela numa ordem sem explicação na
    /// tela: volta para nome (processos) ou porta, que nunca somem.
    fn sort_by_visible_column(&mut self) {
        if !self.column_visible("processes", self.sort.key()) {
            self.sort = SortKey::Name;
            self.sort_desc = false;
        }
        let response = matches!(self.port_sort, PortSort::Response);
        if !self.column_visible("ports", self.port_sort.key()) || (response && !self.probe_http) {
            self.port_sort = PortSort::Port;
            self.port_sort_desc = false;
        }
    }

    /// Volta todas as opções ao padrão, inclusive a inicialização com o Windows e as
    /// larguras de coluna. A atualização em andamento não é afetada.
    pub fn reset_settings(&mut self, ctx: &egui::Context) {
        self.set_interval(1000);
        self.group = false;
        let _ = self.set_port_ranges(ports::DEFAULT_RANGES.to_owned());
        self.ports_all = false;
        self.probe_http = true;
        self.set_auto_update(true);
        self.view = View::Processes;
        self.history_span = HistorySpan::TenMinutes;
        self.sort = SortKey::Cpu;
        self.sort_desc = true;
        self.port_sort = PortSort::Port;
        self.port_sort_desc = false;
        self.set_minimize_to_tray(true);
        self.set_always_on_top(ctx, false);
        self.hidden_columns.clear();
        self.set_history_window(HistoryWindow::One);
        self.set_autostart(false, false);
        self.set_alert_config(alerts::Config::default());
        for table in TABLES {
            ui::columns::reset_widths(ctx, table);
        }
        self.toast("Configurações redefinidas".to_owned(), ToastKind::Ok, false);
    }

    /// Fecha de verdade, sem cair na bandeja.
    pub fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Janela, bandeja e o pedido de fechar. Roda também com a janela escondida.
    fn window_logic(&mut self, ctx: &egui::Context) {
        if ctx.cumulative_frame_nr() == 0 && self.always_on_top {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(window_level(true)));
        }
        // O eframe mostra a janela depois do primeiro quadro; só então dá para escondê-la.
        if self.start_minimized {
            if ctx.cumulative_frame_nr() >= 1 {
                self.start_minimized = false;
                let command = if self.minimize_to_tray && self.tray.icon_shown() {
                    egui::ViewportCommand::Visible(false)
                } else {
                    egui::ViewportCommand::Minimized(true)
                };
                ctx.send_viewport_cmd(command);
            } else {
                ctx.request_repaint();
            }
        }
        if self.tray.take_balloon_click() {
            self.open_last_alert();
        }
        if self.tray.take_show() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.tray.quit_requested() && !self.quitting {
            self.quit(ctx);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.minimize_to_tray && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // Sem o ícone (o Explorer não aceitou), esconder deixaria a janela sem volta.
            if self.tray.icon_shown() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            if !self.tray_hint_shown && self.tray.icon_shown() {
                self.tray_hint_shown = true;
                // O clique no balão não pode abrir um alerta antigo.
                if let Ok(mut slot) = self.last_alert.lock() {
                    *slot = None;
                }
                self.tray.notifier().notify(
                    "O Process Monitor continua aberto",
                    "Ele fica no ícone da bandeja. Para fechar de vez, clique com o botão direito no ícone e escolha Sair.",
                );
            }
        }
    }

    pub fn set_interval(&mut self, ms: u64) {
        // Os gráficos supõem pontos igualmente espaçados: com outro intervalo, o histórico
        // antigo ficaria comprimido ou esticado. O do sampler também recomeça.
        if ms != self.interval_ms {
            self.histories.clear();
        }
        self.interval_ms = ms;
        self.sampler.set_interval(Duration::from_millis(ms));
    }

    pub fn set_auto_update(&mut self, on: bool) {
        self.auto_update = on;
        self.updater.set_auto(on);
    }

    /// Balão do Windows quando a atualização automática termina de baixar uma versão: com
    /// o app na bandeja, o aviso dentro da janela passaria despercebido. O clique no balão
    /// abre a janela, onde está o aviso com o botão de instalar.
    fn announce_update(&mut self) {
        let updater::State::Ready { version, .. } = &*self.update_state else { return };
        if !self.auto_update || self.update_announced == Some(*version) {
            return;
        }
        self.update_announced = Some(*version);
        // O clique no balão não pode abrir um alerta antigo.
        if let Ok(mut slot) = self.last_alert.lock() {
            *slot = None;
        }
        self.tray.notifier().notify(
            &format!("Versão {version} disponível"),
            "Já foi baixada. Clique para abrir o Process Monitor e instalar.",
        );
    }

    /// Roda o instalador baixado e fecha o app; o instalador abre a versão nova.
    pub fn install_update(&mut self, ctx: &egui::Context) {
        let updater::State::Ready { installer, .. } = &*self.update_state else { return };
        match updater::install(installer) {
            Ok(()) => self.quit(ctx),
            Err(e) => {
                self.toast(format!("Não abriu o instalador: {e}"), ToastKind::Error, false);
                // O arquivo pode ter sumido (limpeza, antivírus): baixa de novo.
                self.updater.check_now();
            }
        }
    }

    pub fn select(&mut self, key: Option<ProcKey>) {
        if self.selected != key {
            self.selected = key;
            self.selected_port = None;
        }
    }

    /// Escolhe uma linha da tela de portas: a porta e o processo que a escuta.
    pub fn select_port(&mut self, key: ProcKey, port: u16) {
        self.select(Some(key));
        self.selected_port = Some(port);
    }

    /// Troca as faixas de portas. O texto inválido fica no campo, mas não é aplicado.
    pub fn set_port_ranges(&mut self, text: String) -> Result<(), String> {
        let ranges = ports::parse_ranges(&text);
        self.port_ranges_text = text;
        self.port_ranges = ranges?;
        self.sync_ports();
        Ok(())
    }

    /// Portas que vale sondar: as de processos de desenvolvimento que falam HTTP e
    /// qualquer uma dentro das faixas. Só manda para a thread quando a lista muda.
    pub fn sync_ports(&mut self) {
        let Some(snap) = self.snap.clone() else { return };
        let procs: HashMap<u32, &ProcInfo> = snap.processes.iter().map(|p| (p.pid, p)).collect();
        let mut dev = 0;
        let mut targets = Vec::new();
        for l in &snap.listeners {
            let proc = procs.get(&l.pid).copied();
            let kind = proc.and_then(ports::classify);
            let in_range = ports::in_ranges(&self.port_ranges, l.port);
            if kind.is_some() || in_range {
                dev += 1;
            }
            let http = in_range || kind.zip(proc).is_some_and(|(k, p)| ports::likely_http(k, &p.cmd));
            if http {
                targets.push(Target { port: l.port, pid: l.pid, addr: l.connect_addr(), api: kind == Some(Kind::Python) });
            }
        }
        self.dev_port_count = dev;
        if targets != self.port_targets {
            self.port_targets = targets.clone();
            self.prober.set_targets(targets);
        }
    }

    /// Protegido = finalizar derrubaria o Windows (ou é este próprio app). Em cache por
    /// processo, porque a checagem abre um handle.
    pub fn is_protected(&mut self, key: ProcKey, name: &str) -> bool {
        *self.protected_cache.entry(key).or_insert_with(|| platform::is_protected(key.pid, name))
    }

    pub fn request_kill(&mut self, key: ProcKey, tree: bool) {
        let Some(snap) = self.snap.clone() else { return };
        let Some(proc) = snap.find(key) else { return };
        let label = proc.display_name().to_owned();
        let exe = &proc.name;
        if tree {
            let targets: Vec<KillTarget> = snap.tree_postorder(key).into_iter().map(KillTarget::of).collect();
            let others = targets.len().saturating_sub(1);
            self.confirm = Some(KillRequest {
                title: format!("Finalizar a árvore de {label}?"),
                detail: if others == 0 {
                    format!("{exe} (PID {}) não tem processos filhos. Dados não salvos serão perdidos.", key.pid)
                } else {
                    format!(
                        "{exe} (PID {}) e {others} processo{} filho{} serão encerrados. Dados não salvos serão perdidos.",
                        key.pid,
                        if others == 1 { "" } else { "s" },
                        if others == 1 { "" } else { "s" },
                    )
                },
                targets,
                then: None,
            });
        } else {
            self.confirm = Some(KillRequest {
                title: format!("Finalizar {label}?"),
                detail: format!("O processo {exe} (PID {}) será encerrado imediatamente. Dados não salvos serão perdidos.", key.pid),
                targets: vec![KillTarget::of(proc)],
                then: None,
            });
        }
    }

    /// Nome do projeto do servidor, ou o nome do processo quando não é de desenvolvimento.
    fn server_label(proc: &ProcInfo) -> String {
        ports::classify(proc)
            .and_then(|_| ports::project_dir(proc))
            .and_then(|d| ports::project_label(&d))
            .unwrap_or_else(|| proc.display_name().to_owned())
    }

    /// Processo a reiniciar para trazer de volta o servidor de `key`, ou por que não dá.
    pub fn restart_target(&mut self, key: ProcKey) -> Result<ProcKey, &'static str> {
        let snap = self.snap.clone().ok_or("Ainda lendo o sistema.")?;
        let proc = snap.find(key).ok_or("O processo já encerrou.")?;
        // Reiniciar o Chrome ou um serviço pela linha de comando não faz sentido.
        if ports::classify(proc).is_none() {
            return Err("Não é um servidor de desenvolvimento.");
        }
        let root = ports::server_root(proc, |c| snap.parent_of(c));
        if root.cmd.is_empty() || root.cwd.is_none() {
            return Err("Sem acesso à linha de comando ou à pasta do processo. Execute como administrador.");
        }
        let (root_key, root_name) = (root.key(), root.name.clone());
        if self.is_protected(root_key, &root_name) {
            return Err("Processo crítico do sistema.");
        }
        Ok(root_key)
    }

    /// Reiniciar o servidor que escuta `port`: finaliza a árvore e roda o mesmo comando
    /// numa janela de terminal nova, na mesma pasta.
    pub fn request_restart(&mut self, key: ProcKey, port: u16) {
        let root_key = match self.restart_target(key) {
            Ok(k) => k,
            Err(reason) => return self.toast(format!("Não dá para reiniciar: {reason}"), ToastKind::Error, false),
        };
        let Some(snap) = self.snap.clone() else { return };
        let Some(root) = snap.find(root_key) else { return };
        let Some(cwd) = root.cwd.clone() else { return };
        let label = Self::server_label(snap.find(key).unwrap_or(root));
        let targets: Vec<KillTarget> = snap.tree_postorder(root_key).into_iter().map(KillTarget::of).collect();
        let elevated = if self.elevated {
            " Como o Process Monitor está como administrador, o servidor também vai rodar como administrador."
        } else {
            ""
        };
        let tree = match targets.len().saturating_sub(1) {
            0 => " será encerrado".to_owned(),
            1 => " e 1 processo filho serão encerrados".to_owned(),
            n => format!(" e {n} processos filhos serão encerrados"),
        };
        self.confirm = Some(KillRequest {
            title: format!("Reiniciar {label}?"),
            detail: format!(
                "{} (PID {}){tree}, e o mesmo comando roda de novo numa janela de terminal nova, \
                 em {}. O processo novo herda o ambiente do Process Monitor, não o do terminal original.{elevated}\n\n{}",
                root.name,
                root.pid,
                cwd.display(),
                root.cmd
            ),
            targets,
            then: Some(Relaunch { cmd: root.cmd.clone(), cwd, title: format!("{label} ({port})"), port }),
        });
    }

    /// Espera a porta liberar e abre o servidor de novo, numa thread: a espera chega a segundos.
    fn relaunch(&self, r: Relaunch) {
        let tx = self.relaunch_tx.clone();
        std::thread::Builder::new()
            .name("relaunch".into())
            .spawn(move || {
                let deadline = Instant::now() + RELAUNCH_WAIT;
                while ports::listeners().iter().any(|l| l.port == r.port) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(150));
                }
                let _ = tx.send(platform::spawn_in_console(&r.cmd, &r.cwd, &r.title).map(|()| r.title));
            })
            .expect("thread de reinício");
    }

    /// Finalizar o processo que escuta `port`, com a porta no texto da confirmação.
    pub fn request_kill_port(&mut self, key: ProcKey, port: u16) {
        let Some(snap) = self.snap.clone() else { return };
        let Some(proc) = snap.find(key) else { return };
        let label = Self::server_label(proc);
        // Num servidor de desenvolvimento vai a árvore desde a raiz (`npm run dev` →
        // `cmd` → `node vite`): finalizar só quem escuta deixava o npm vivo, e há
        // ferramentas que sobem o servidor de novo.
        let root = match ports::classify(proc) {
            Some(_) => ports::server_root(proc, |c| snap.parent_of(c)),
            None => proc,
        };
        let (root_key, root_name) = (root.key(), root.name.clone());
        let targets: Vec<KillTarget> = if root_key != key && !self.is_protected(root_key, &root_name) {
            snap.tree_postorder(root_key).into_iter().map(KillTarget::of).collect()
        } else {
            vec![KillTarget::of(proc)]
        };
        let tree: HashSet<u32> = targets.iter().map(|t| t.key.pid).collect();
        let others = snap.listeners.iter().filter(|l| tree.contains(&l.pid) && l.port != port).count();
        let also = match others {
            0 => String::new(),
            1 => " Ele também escuta em outra porta, que fecha junto.".to_owned(),
            n => format!(" Ele também escuta em outras {n} portas, que fecham junto."),
        };
        let who = match targets.len() {
            1 => format!("{label} ({}, PID {}) será encerrado.", proc.name, key.pid),
            n => format!(
                "{label} será encerrado com a árvore inteira: {} (PID {}) e mais {} processo{}.",
                root_name,
                root_key.pid,
                n - 1,
                if n == 2 { "" } else { "s" }
            ),
        };
        self.confirm = Some(KillRequest {
            title: format!("Liberar a porta {port}?"),
            detail: format!("{who}{also} Dados não salvos serão perdidos."),
            targets,
            then: None,
        });
    }

    /// `exe` é o nome do executável, a chave dos grupos.
    pub fn request_kill_group(&mut self, exe: &str) {
        let Some(snap) = self.snap.clone() else { return };
        let targets: Vec<KillTarget> = snap.processes.iter().filter(|p| p.name == exe).map(KillTarget::of).collect();
        let Some(label) = targets.first().map(|t| t.label.clone()) else { return };
        self.confirm = Some(KillRequest {
            title: format!("Finalizar todos os processos de {label}?"),
            detail: format!("{} processos {exe} serão encerrados. Dados não salvos serão perdidos.", targets.len()),
            targets,
            then: None,
        });
    }

    /// Suspende ou retoma. Suspenso, o processo não roda nada até ser retomado.
    pub fn toggle_suspend(&mut self, key: ProcKey) {
        let Some(proc) = self.snap.as_ref().and_then(|s| s.find(key)).cloned() else { return };
        let label = proc.display_name().to_owned();
        // O estado vem da coleta (as threads do processo), então vale também para quem
        // foi suspenso por outro programa. Coleta já para a tela refletir a mudança.
        if proc.suspended {
            match control::resume(key, &proc.name) {
                Ok(()) | Err(KillError::NotFound) => self.toast(format!("{label} retomado"), ToastKind::Ok, false),
                Err(e) => self.control_error(&format!("Não retomou {label}"), e),
            }
        } else {
            match control::suspend(key, &proc.name) {
                Ok(()) => self.toast(format!("{label} suspenso até você retomar"), ToastKind::Ok, false),
                Err(e) => self.control_error(&format!("Não suspendeu {label}"), e),
            }
        }
        self.sampler.refresh_now();
    }

    fn control_error(&mut self, what: &str, e: KillError) {
        let denied = e == KillError::AccessDenied;
        self.toast(format!("{what}: {}", e.describe()), ToastKind::Error, denied && !self.elevated);
    }

    /// Prioridade, núcleos e modo eficiência. Lidos uma vez por seleção (abre um handle)
    /// e de novo depois de uma mudança.
    pub fn tuning(&mut self, key: ProcKey) -> control::Tuning {
        if self.tuning.as_ref().is_none_or(|(k, _)| *k != key) {
            self.tuning = Some((key, control::read(key)));
        }
        self.tuning.as_ref().map(|(_, t)| t.clone()).unwrap_or_default()
    }

    /// Aplica um ajuste e relê o estado do processo; erro vira aviso.
    pub fn tune(&mut self, key: ProcKey, change: Tune) {
        let Some(proc) = self.snap.as_ref().and_then(|s| s.find(key)).cloned() else { return };
        let result = match change {
            Tune::Priority(p) => control::set_priority(key, &proc.name, p),
            Tune::Affinity(mask) => control::set_affinity(key, &proc.name, mask),
            Tune::Efficiency(on) => control::set_efficiency(key, &proc.name, on),
        };
        if let Err(e) = result {
            self.control_error(&format!("Não alterou {}", proc.display_name()), e);
        }
        self.tuning = None;
    }

    /// Grava o histórico do processo em CSV na pasta Downloads e mostra o arquivo.
    pub fn export_history(&mut self, key: ProcKey) {
        let Some(proc) = self.snap.as_ref().and_then(|s| s.find(key)).cloned() else { return };
        let Some(history) = self.histories.get(&key) else { return };
        let csv = export::history_csv(&history.cpu, &history.memory, self.interval_ms, chrono::Local::now());
        let stem = proc.name.strip_suffix(".exe").unwrap_or(&proc.name);
        match export::save(stem, key.pid, &csv) {
            Ok(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.toast(format!("Histórico salvo em Downloads: {name}"), ToastKind::Ok, false);
                platform::open_in_explorer(&path);
            }
            Err(e) => self.toast(format!("Não exportou o histórico: {e}"), ToastKind::Error, false),
        }
    }

    /// Ação num serviço; o resultado chega depois, como aviso.
    pub fn service_action(&mut self, ctx: &egui::Context, name: &str, display: &str, action: services::Action) {
        if let Some(manager) = &self.services {
            manager.act(ctx, name.to_owned(), display.to_owned(), action);
        }
    }

    fn execute_kill(&mut self, request: KillRequest) {
        let total = request.targets.len();
        let mut done = 0;
        let mut errors: Vec<(String, KillError)> = Vec::new();
        for target in &request.targets {
            match platform::kill(target.key.pid, target.key.start, &target.exe) {
                Ok(()) => done += 1,
                // Numa árvore, o filho pode ter morrido junto com outro antes da vez dele.
                Err(KillError::NotFound) => done += 1,
                Err(e) => errors.push((target.label.clone(), e)),
            }
        }
        if errors.is_empty()
            && let Some(relaunch) = request.then.clone()
        {
            self.toast(format!("Reiniciando {}…", relaunch.title), ToastKind::Ok, false);
            self.relaunch(relaunch);
        } else if errors.is_empty() {
            let text = if total == 1 {
                format!("{} finalizado", request.targets[0].label)
            } else {
                format!("{done} processos finalizados")
            };
            self.toast(text, ToastKind::Ok, false);
        } else {
            let denied = errors.iter().any(|(_, e)| *e == KillError::AccessDenied);
            let (name, first) = &errors[0];
            let text = if total == 1 {
                format!("Não foi possível finalizar {name}: {}", first.describe())
            } else {
                format!("{done} de {total} finalizados. {name}: {}", first.describe())
            };
            self.toast(text, ToastKind::Error, denied && !self.elevated);
        }
        if done > 0 {
            if let Some(sel) = self.selected
                && request.targets.iter().any(|t| t.key == sel)
            {
                self.select(None);
            }
            self.sampler.refresh_now();
            // Quem estava usando o arquivo pode ter sido finalizado: a lista é refeita.
            if self.locks.open {
                self.locks.check_after(Duration::from_millis(400));
            }
        }
    }

    pub fn toast(&mut self, text: String, kind: ToastKind, offer_elevation: bool) {
        let secs = if offer_elevation { 8 } else { 4 };
        self.toasts.push(Toast { text, kind, offer_elevation, until: Instant::now() + Duration::from_secs(secs) });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    pub fn relaunch_elevated(&mut self, ctx: &egui::Context) {
        match platform::relaunch_as_admin() {
            Ok(()) => self.quit(ctx),
            Err(e) => self.toast(format!("Não reiniciou como administrador: {e}"), ToastKind::Error, false),
        }
    }

    /// Acrescenta a coleta nova ao histórico de cada processo; os que encerraram saem.
    fn record_histories(&mut self, snap: &Snapshot) {
        let alive: HashSet<ProcKey> = snap.processes.iter().map(|p| p.key()).collect();
        self.histories.retain(|k, _| alive.contains(k));
        for p in &snap.processes {
            let h = self.histories.entry(p.key()).or_insert_with(ProcHistory::new);
            h.cpu.push(p.cpu);
            h.memory.push(p.memory as f32);
        }
    }

    /// Docker e WSL só são consultados com a tela de portas aberta e alguma porta deles.
    fn containers_logic(&mut self) {
        let wanted = self.view == View::Ports
            && self.snap.as_ref().is_some_and(|s| {
                let owners: HashSet<u32> = s.listeners.iter().map(|l| l.pid).collect();
                s.processes.iter().any(|p| owners.contains(&p.pid) && (containers::is_docker(&p.name) || containers::is_wsl(&p.name)))
            });
        self.containers.poll(wanted);
        self.container_info = self.containers.latest();
        for (name, result) in self.containers.take_stopped() {
            match result {
                Ok(()) => self.toast(format!("Container {name} parado"), ToastKind::Ok, false),
                Err(e) => self.toast(format!("Não parou o container {name}: {e}"), ToastKind::Error, false),
            }
        }
    }

    /// A thread dos serviços nasce na primeira visita à tela e só relê com ela aberta.
    fn services_logic(&mut self, ctx: &egui::Context) {
        let visible = self.view == View::Services;
        if visible && self.services.is_none() {
            self.services = Some(services::Manager::spawn(ctx.clone()));
        }
        let Some(manager) = &mut self.services else { return };
        manager.set_active(visible);
        for (display, action, result) in manager.take_outcomes() {
            match result {
                Ok(()) => self.toast(format!("{display} {}", action.done()), ToastKind::Ok, false),
                Err(e) => {
                    let denied = e == services::Error::AccessDenied;
                    let text = format!("Não deu para {} {display}: {}", action.verb(), e.describe());
                    self.toast(text, ToastKind::Error, denied && !self.elevated);
                }
            }
        }
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(request) = self.confirm.clone() else { return };
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(Id::new("confirm_kill"))
            .frame(
                Frame::NONE
                    .fill(theme::GRAPHITE_2)
                    .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
                    .corner_radius(CornerRadius::same(R_LG))
                    .inner_margin(Margin::same(22)),
            )
            .backdrop_color(Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::WARNING_OCTAGON).font(theme::regular(22.0)).color(theme::DANGER));
                    ui.add_space(4.0);
                    ui.label(RichText::new(&request.title).font(theme::semibold(16.0)).color(INK));
                });
                ui.add_space(8.0);
                ui.label(RichText::new(&request.detail).font(theme::regular(13.0)).color(INK_2));
                ui.add_space(18.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if request.then.is_some() {
                        "Reiniciar"
                    } else if request.targets.len() > 1 {
                        "Finalizar todos"
                    } else {
                        "Finalizar"
                    };
                    if widgets::danger_button(ui, label, true).clicked() {
                        decision = Some(true);
                    }
                    if widgets::ghost_button(ui, "Cancelar").clicked() {
                        decision = Some(false);
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    decision = Some(true);
                }
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                self.confirm = None;
                self.execute_kill(request);
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        let ready = match &*self.update_state {
            updater::State::Ready { version, .. } if self.update_dismissed != Some(*version) => Some(*version),
            _ => None,
        };
        if self.toasts.is_empty() && ready.is_none() {
            return;
        }
        let mut elevate = false;
        let mut dismiss: Option<usize> = None;
        let mut update: Option<bool> = None;
        egui::Area::new(Id::new("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -18.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                if let Some(version) = ready {
                    update = update_notice(ui, version);
                    ui.add_space(8.0);
                }
                for (i, toast) in self.toasts.iter().enumerate() {
                    let (glyph, color) = match toast.kind {
                        ToastKind::Ok => (icon::CHECK_CIRCLE, theme::OK),
                        ToastKind::Error => (icon::WARNING_CIRCLE, theme::DANGER),
                    };
                    Frame::NONE
                        .fill(GRAPHITE_3)
                        .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
                        .corner_radius(CornerRadius::same(R_MD))
                        .inner_margin(Margin::symmetric(14, 10))
                        .shadow(ui.visuals().popup_shadow)
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(glyph).font(theme::regular(16.0)).color(color));
                                ui.add(egui::Label::new(RichText::new(&toast.text).color(INK).font(theme::regular(13.0))).wrap());
                                if toast.offer_elevation
                                    && ui.button(RichText::new(format!("{} Executar como admin", icon::SHIELD_CHECK))).clicked()
                                {
                                    elevate = true;
                                }
                                if widgets::icon_button(ui, icon::X, "Fechar").clicked() {
                                    dismiss = Some(i);
                                }
                            });
                        });
                    ui.add_space(8.0);
                }
            });
        if let Some(i) = dismiss {
            self.toasts.remove(i);
        }
        if elevate {
            self.relaunch_elevated(ctx);
        }
        match update {
            Some(true) => self.install_update(ctx),
            Some(false) => self.update_dismissed = ready,
            None => {}
        }
        if let Some(next) = self.toasts.iter().map(|t| t.until).min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.window_logic(ctx);
        if let Some(latest) = self.sampler.latest()
            && self.snap.as_ref().is_none_or(|s| s.seq != latest.seq)
        {
            // Processos que sumiram não precisam mais do cache de proteção.
            if latest.seq % 30 == 0 {
                let alive: HashSet<ProcKey> = latest.processes.iter().map(|p| p.key()).collect();
                self.protected_cache.retain(|k, _| alive.contains(k));
            }
            if let View::Gpu(i) = self.view
                && i >= latest.gpus.len()
            {
                self.view = View::Processes;
            }
            self.record_histories(&latest);
            self.snap = Some(latest);
            self.sync_ports();
        }
        // Sonda só quando alguém vê o resultado: na tela de portas ou na seção de portas
        // do painel de detalhes. Fora disso, a resposta mostrada ficaria velha.
        let selected_listens = self.selected.zip(self.snap.as_ref()).is_some_and(|(k, s)| {
            s.listeners.iter().any(|l| l.pid == k.pid)
        });
        let ports_visible = self.view == View::Ports || (self.view == View::Processes && selected_listens);
        let probe = ports_visible && self.probe_http;
        if probe != self.prober_active {
            self.prober_active = probe;
            self.prober.set_active(probe);
        }
        // Sem sondagem, nada de mostrar a última resposta como se fosse atual.
        self.port_status = if self.probe_http { self.prober.latest() } else { Arc::default() };
        self.update_state = self.updater.latest();
        self.announce_update();
        self.icons.poll(ctx);
        while let Ok(result) = self.relaunch_rx.try_recv() {
            match result {
                Ok(title) => self.toast(format!("{title} reaberto numa janela de terminal"), ToastKind::Ok, false),
                Err(e) => self.toast(format!("Não reabriu o servidor: {e}"), ToastKind::Error, false),
            }
        }
        self.services_logic(ctx);
        self.containers_logic();
        if self.view == View::Startup {
            self.startup.ensure_loaded();
        }
        self.startup.poll();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(244.0)
            .frame(Frame::NONE.fill(GRAPHITE_0).inner_margin(Margin { left: 12, right: 12, top: 14, bottom: 12 }))
            .show(ui, |ui| ui::sidebar::show(self, ui));

        let details_visible = matches!(self.view, View::Processes | View::Ports)
            && self.selected.is_some()
            && self.snap.as_ref().zip(self.selected).is_some_and(|(s, k)| s.find(k).is_some());
        if details_visible {
            // Id novo descarta a largura salva pela versão com o bug do painel que crescia.
            egui::Panel::right("details_panel")
                .resizable(true)
                .default_size(360.0)
                .size_range(320.0..=480.0)
                .frame(Frame::NONE.fill(GRAPHITE_0).inner_margin(Margin::same(0)))
                .show(ui, |ui| ui::details::show(self, ui));
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(GRAPHITE_0).inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 12 }))
            .show(ui, |ui| match self.snap.clone() {
                None => loading(ui),
                Some(snap) => match self.view {
                    View::Processes => ui::processes::show(self, ui, &snap),
                    View::Services => ui::services::show(self, ui, &snap),
                    View::Startup => ui::startup::show(self, ui, &snap),
                    View::Ports => ui::ports::show(self, ui, &snap),
                    other => ui::performance::show(self, ui, &snap, other),
                },
            });

        if self.settings_open {
            ui::settings::show(self, &ctx);
        }
        let dropped: Vec<std::path::PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.locks.open_with(&dropped);
        }
        ui::locks::show(self, &ctx);
        // Depois da janela de arquivos: a confirmação de finalizar abre por cima dela.
        self.confirm_modal(&ctx);
        self.toasts_ui(&ctx);
        ui::locks::drop_overlay(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        save_alert_config(storage, &self.alert_config);
        storage.set_string(INTERVAL_KEY, self.interval_ms.to_string());
        storage.set_string(GROUP_KEY, if self.group { "1" } else { "0" }.to_owned());
        // Faixas inválidas não são salvas: na próxima abertura volta a última que valia.
        if ports::parse_ranges(&self.port_ranges_text).is_ok() {
            storage.set_string(PORT_RANGES_KEY, self.port_ranges_text.clone());
        }
        storage.set_string(PORTS_ALL_KEY, if self.ports_all { "1" } else { "0" }.to_owned());
        storage.set_string(AUTO_UPDATE_KEY, if self.auto_update { "1" } else { "0" }.to_owned());
        storage.set_string(VIEW_KEY, self.view.key());
        storage.set_string(HISTORY_SPAN_KEY, if self.history_span == HistorySpan::Minute { "1" } else { "10" }.to_owned());
        storage.set_string(SORT_KEY, self.sort.key().to_owned());
        storage.set_string(SORT_DESC_KEY, if self.sort_desc { "1" } else { "0" }.to_owned());
        storage.set_string(PORT_SORT_KEY, self.port_sort.key().to_owned());
        storage.set_string(PORT_SORT_DESC_KEY, if self.port_sort_desc { "1" } else { "0" }.to_owned());
        storage.set_string(TRAY_KEY, if self.minimize_to_tray { "1" } else { "0" }.to_owned());
        storage.set_string(TRAY_HINT_KEY, if self.tray_hint_shown { "1" } else { "0" }.to_owned());
        storage.set_string(ON_TOP_KEY, if self.always_on_top { "1" } else { "0" }.to_owned());
        storage.set_string(PROBE_HTTP_KEY, if self.probe_http { "1" } else { "0" }.to_owned());
        storage.set_string(HIDDEN_COLUMNS_KEY, self.hidden_columns.iter().cloned().collect::<Vec<_>>().join(","));
        storage.set_string(HISTORY_WINDOW_KEY, self.history_window.minutes().to_string());
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        GRAPHITE_0.to_normalized_gamma_f32()
    }
}

/// Aviso fixo de versão nova baixada. `Some(true)` = instalar, `Some(false)` = depois.
fn update_notice(ui: &mut egui::Ui, version: updater::Version) -> Option<bool> {
    let mut choice = None;
    Frame::NONE
        .fill(GRAPHITE_3)
        .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
        .corner_radius(CornerRadius::same(R_MD))
        .inner_margin(Margin::symmetric(14, 12))
        .shadow(ui.visuals().popup_shadow)
        .show(ui, |ui| {
            ui.set_width(340.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::ARROW_CIRCLE_UP).font(theme::regular(18.0)).color(theme::ACCENT));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(format!("Versão {version} pronta")).font(theme::semibold(13.5)).color(INK));
                    ui.label(
                        RichText::new("O app fecha, instala e abre de novo em alguns segundos.")
                            .font(theme::regular(12.0))
                            .color(INK_2),
                    );
                });
            });
            ui.add_space(10.0);
            // Altura fixa: numa Area a altura disponível é a tela inteira, e o alinhamento
            // ao centro esticaria o cartão até lá.
            let row = vec2(ui.available_width(), 32.0);
            ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::accent_button(ui, "Reiniciar e atualizar").clicked() {
                    choice = Some(true);
                }
                if widgets::ghost_button(ui, "Depois").clicked() {
                    choice = Some(false);
                }
            });
        });
    choice
}

const ALERT_PORTS_KEY: &str = "alert_ports";
const ALERT_PORTS_ON_KEY: &str = "alert_ports_on";
const ALERT_CPU_ON_KEY: &str = "alert_cpu_on";
const ALERT_CPU_PERCENT_KEY: &str = "alert_cpu_percent";
const ALERT_CPU_SECS_KEY: &str = "alert_cpu_secs";
const ALERT_MEMORY_ON_KEY: &str = "alert_memory_on";
const ALERT_MEMORY_KEY: &str = "alert_memory_bytes";
const ALERT_DISK_ON_KEY: &str = "alert_disk_on";
const ALERT_DISK_PERCENT_KEY: &str = "alert_disk_free_percent";

/// Configuração dos alertas salva; o que faltar ou for inválido fica no padrão.
fn load_alert_config(stored: &dyn Fn(&str) -> Option<String>) -> alerts::Config {
    let defaults = alerts::Config::default();
    let flag = |key: &str, default: bool| stored(key).map_or(default, |v| v == "1");
    alerts::Config {
        watched_ports: stored(ALERT_PORTS_KEY).map(|v| alerts::parse_ports(&v)).unwrap_or_default(),
        ports_enabled: flag(ALERT_PORTS_ON_KEY, defaults.ports_enabled),
        cpu_enabled: flag(ALERT_CPU_ON_KEY, defaults.cpu_enabled),
        cpu_percent: stored(ALERT_CPU_PERCENT_KEY)
            .and_then(|v| v.parse().ok())
            .filter(|v: &f32| (5.0..=100.0).contains(v))
            .unwrap_or(defaults.cpu_percent),
        cpu_secs: stored(ALERT_CPU_SECS_KEY).and_then(|v| v.parse().ok()).filter(|v| *v > 0).unwrap_or(defaults.cpu_secs),
        memory_enabled: flag(ALERT_MEMORY_ON_KEY, defaults.memory_enabled),
        memory_bytes: stored(ALERT_MEMORY_KEY).and_then(|v| v.parse().ok()).filter(|v| *v > 0).unwrap_or(defaults.memory_bytes),
        disk_enabled: flag(ALERT_DISK_ON_KEY, defaults.disk_enabled),
        disk_free_percent: stored(ALERT_DISK_PERCENT_KEY)
            .and_then(|v| v.parse().ok())
            .filter(|v: &f32| (1.0..=50.0).contains(v))
            .unwrap_or(defaults.disk_free_percent),
    }
}

fn save_alert_config(storage: &mut dyn eframe::Storage, c: &alerts::Config) {
    let flag = |on: bool| if on { "1" } else { "0" }.to_owned();
    storage.set_string(ALERT_PORTS_KEY, alerts::ports_text(&c.watched_ports));
    storage.set_string(ALERT_PORTS_ON_KEY, flag(c.ports_enabled));
    storage.set_string(ALERT_CPU_ON_KEY, flag(c.cpu_enabled));
    storage.set_string(ALERT_CPU_PERCENT_KEY, c.cpu_percent.to_string());
    storage.set_string(ALERT_CPU_SECS_KEY, c.cpu_secs.to_string());
    storage.set_string(ALERT_MEMORY_ON_KEY, flag(c.memory_enabled));
    storage.set_string(ALERT_MEMORY_KEY, c.memory_bytes.to_string());
    storage.set_string(ALERT_DISK_ON_KEY, flag(c.disk_enabled));
    storage.set_string(ALERT_DISK_PERCENT_KEY, c.disk_free_percent.to_string());
}

/// Lê a entrada de inicialização. Se ela aponta para outro executável (o app foi
/// reinstalado em outra pasta), passa a apontar para este.
fn read_autostart() -> (bool, bool) {
    let Some(cmd) = platform::autostart_command() else { return (false, false) };
    let minimized = cmd.contains(platform::MINIMIZED_ARG);
    if !platform::autostart_points_here(&cmd) {
        let _ = platform::set_autostart(true, minimized);
    }
    (true, minimized)
}

fn window_level(on_top: bool) -> egui::WindowLevel {
    if on_top { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal }
}

fn loading(ui: &mut egui::Ui) {
    ui.centered_and_justified(|ui| {
        ui.label(RichText::new("Lendo o sistema…").color(INK_3).font(theme::regular(14.0)));
    });
}

/// Linha divisória fina, usada entre blocos dentro dos painéis.
pub fn hairline(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, HAIRLINE));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferencias_salvas_voltam_iguais() {
        for view in [View::Processes, View::Services, View::Startup, View::Ports, View::Cpu, View::Memory, View::Disk, View::Network, View::Gpu(1)] {
            assert_eq!(View::from_key(&view.key()), Some(view));
        }
        assert_eq!(View::from_key("gpu:x"), None);
        assert_eq!(View::from_key(""), None);
        for key in SortKey::ALL {
            assert_eq!(SortKey::ALL.into_iter().find(|k| k.key() == key.key()), Some(key));
        }
        for key in PortSort::ALL {
            assert_eq!(PortSort::ALL.into_iter().find(|k| k.key() == key.key()), Some(key));
        }
    }
}
