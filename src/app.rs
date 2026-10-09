//! Estado do aplicativo e montagem da janela: barra lateral, conteúdo e detalhes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, Frame, Id, Margin, RichText, Stroke, vec2};
use egui_phosphor::regular as icon;

use crate::icons::IconCache;
use crate::platform::{self, KillError};
use crate::ports::{self, Kind, PortRange, Prober, Statuses, Target};
use crate::sampler::{ProcInfo, ProcKey, Sampler, Series, Snapshot};
use crate::theme::{self, GRAPHITE_0, GRAPHITE_3, HAIRLINE, HAIRLINE_STRONG, INK, INK_2, INK_3, R_LG, R_MD};
use crate::ui;
use crate::ui::widgets;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Processes,
    Ports,
    Cpu,
    Memory,
    Disk,
    Network,
    Gpu(usize),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortSort {
    Port,
    Name,
    Pid,
    Response,
    Memory,
    Uptime,
}

/// Pedido de finalização aguardando confirmação no modal.
#[derive(Clone, Debug)]
pub struct KillRequest {
    pub title: String,
    pub detail: String,
    /// Alvos na ordem de execução (numa árvore, filhos antes dos pais).
    pub targets: Vec<KillTarget>,
}

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

/// Histórico só do processo selecionado, para as sparklines do painel de detalhes.
#[derive(Default)]
pub struct ProcHistory {
    pub key: Option<ProcKey>,
    pub cpu: Series,
    pub memory: Series,
    pub last_seq: u64,
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
    pub proc_history: ProcHistory,
    pub confirm: Option<KillRequest>,
    pub toasts: Vec<Toast>,
    pub icons: IconCache,
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
}

const INTERVAL_KEY: &str = "interval_ms";
const GROUP_KEY: &str = "group";
const PORT_RANGES_KEY: &str = "port_ranges";
const PORTS_ALL_KEY: &str = "ports_all";

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
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
        Self {
            sampler: Sampler::spawn(cc.egui_ctx.clone(), Duration::from_millis(interval_ms)),
            snap: None,
            view: View::Processes,
            search: String::new(),
            group,
            expanded: HashSet::new(),
            sort: SortKey::Cpu,
            sort_desc: true,
            selected: None,
            scroll_to_selected: false,
            prev_rank: HashMap::new(),
            prev_group_rank: HashMap::new(),
            proc_history: ProcHistory::default(),
            confirm: None,
            toasts: Vec::new(),
            icons: IconCache::new(cc.egui_ctx.clone()),
            elevated: platform::is_elevated(),
            interval_ms,
            protected_cache: HashMap::new(),
            prober: Prober::spawn(cc.egui_ctx.clone()),
            port_status: Arc::default(),
            port_ranges: ports::parse_ranges(&port_ranges_text).unwrap_or_default(),
            port_ranges_text,
            ports_all: stored(PORTS_ALL_KEY).is_some_and(|v| v == "1"),
            port_search: String::new(),
            port_sort: PortSort::Port,
            port_sort_desc: false,
            selected_port: None,
            scroll_to_port: false,
            dev_port_count: 0,
            port_targets: Vec::new(),
            prober_active: false,
        }
    }

    pub fn set_interval(&mut self, ms: u64) {
        self.interval_ms = ms;
        self.sampler.set_interval(Duration::from_millis(ms));
    }

    pub fn select(&mut self, key: Option<ProcKey>) {
        if self.selected != key {
            self.selected = key;
            self.selected_port = None;
            self.proc_history = ProcHistory::default();
            self.track_selected();
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
            });
        } else {
            self.confirm = Some(KillRequest {
                title: format!("Finalizar {label}?"),
                detail: format!("O processo {exe} (PID {}) será encerrado imediatamente. Dados não salvos serão perdidos.", key.pid),
                targets: vec![KillTarget::of(proc)],
            });
        }
    }

    /// Finalizar o processo que escuta `port`, com a porta no texto da confirmação.
    pub fn request_kill_port(&mut self, key: ProcKey, port: u16) {
        let Some(snap) = self.snap.clone() else { return };
        let Some(proc) = snap.find(key) else { return };
        let label = ports::classify(proc)
            .and_then(|_| ports::project_dir(proc))
            .and_then(|d| ports::project_label(&d))
            .unwrap_or_else(|| proc.display_name().to_owned());
        let others = snap.listeners.iter().filter(|l| l.pid == key.pid && l.port != port).count();
        let also = match others {
            0 => String::new(),
            1 => " Ele também escuta em outra porta, que fecha junto.".to_owned(),
            n => format!(" Ele também escuta em outras {n} portas, que fecham junto."),
        };
        self.confirm = Some(KillRequest {
            title: format!("Liberar a porta {port}?"),
            detail: format!(
                "{label} ({}, PID {}) será encerrado.{also} Dados não salvos serão perdidos.",
                proc.name, key.pid
            ),
            targets: vec![KillTarget::of(proc)],
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
        });
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
        if errors.is_empty() {
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
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => self.toast(format!("Não reiniciou como administrador: {e}"), ToastKind::Error, false),
        }
    }

    /// Guarda o ponto mais recente do processo selecionado (uma vez por coleta).
    fn track_selected(&mut self) {
        let (Some(snap), Some(key)) = (&self.snap, self.selected) else { return };
        if self.proc_history.last_seq == snap.seq && self.proc_history.key == Some(key) {
            return;
        }
        if let Some(p) = snap.find(key) {
            self.proc_history.key = Some(key);
            self.proc_history.last_seq = snap.seq;
            self.proc_history.cpu.push(p.cpu);
            self.proc_history.memory.push(p.memory as f32);
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
                    let label = if request.targets.len() > 1 { "Finalizar todos" } else { "Finalizar" };
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
        if self.toasts.is_empty() {
            return;
        }
        let mut elevate = false;
        let mut dismiss: Option<usize> = None;
        egui::Area::new(Id::new("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -18.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
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
        if let Some(next) = self.toasts.iter().map(|t| t.until).min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(latest) = self.sampler.latest()
            && self.snap.as_ref().is_none_or(|s| s.seq != latest.seq)
        {
            // Processos que sumiram não precisam mais do cache de proteção.
            if latest.seq % 30 == 0 {
                let alive: HashSet<ProcKey> = latest.processes.iter().map(|p| p.key()).collect();
                self.protected_cache.retain(|k, _| alive.contains(k));
            }
            self.snap = Some(latest);
            self.track_selected();
            self.sync_ports();
        }
        // Sonda só quando alguém vê o resultado: na tela de portas ou na seção de portas
        // do painel de detalhes. Fora disso, a resposta mostrada ficaria velha.
        let selected_listens = self.selected.zip(self.snap.as_ref()).is_some_and(|(k, s)| {
            s.listeners.iter().any(|l| l.pid == k.pid)
        });
        let ports_visible = self.view == View::Ports || (self.view == View::Processes && selected_listens);
        if ports_visible != self.prober_active {
            self.prober_active = ports_visible;
            self.prober.set_active(ports_visible);
        }
        self.port_status = self.prober.latest();
        self.icons.poll(ctx);
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
                    View::Ports => ui::ports::show(self, ui, &snap),
                    other => ui::performance::show(self, ui, &snap, other),
                },
            });

        self.confirm_modal(&ctx);
        self.toasts_ui(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(INTERVAL_KEY, self.interval_ms.to_string());
        storage.set_string(GROUP_KEY, if self.group { "1" } else { "0" }.to_owned());
        // Faixas inválidas não são salvas: na próxima abertura volta a última que valia.
        if ports::parse_ranges(&self.port_ranges_text).is_ok() {
            storage.set_string(PORT_RANGES_KEY, self.port_ranges_text.clone());
        }
        storage.set_string(PORTS_ALL_KEY, if self.ports_all { "1" } else { "0" }.to_owned());
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        GRAPHITE_0.to_normalized_gamma_f32()
    }
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
