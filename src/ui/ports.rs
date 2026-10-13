//! Portas em escuta: quem ocupa cada uma, se responde HTTP e o que serve.
//!
//! Veio do monitor do V-Monitor: as faixas de portas em chips (o que está livre
//! aparece de relance), o título da página ou da API como subtítulo, o tempo de
//! resposta e liberar a porta finalizando o processo.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use egui::{Align, Align2, Color32, CornerRadius, Key, Layout, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;

use crate::app::{App, PortSort, View};
use crate::containers::{self, Container};
use crate::format;
use crate::platform;
use crate::ports::{self, Kind, Listener, PortRange, PortStatus, Reply, Statuses};
use crate::sampler::{ProcInfo, ProcKey, Snapshot};
use crate::theme::{self, GRAPHITE_3, HAIRLINE, HAIRLINE_STRONG, INK, INK_2, INK_3, INK_4, NETWORK, R_SM};
use crate::ui::columns::{self, TableCol};
use crate::ui::processes::{MB, band, mem_heat, search_terms};
use crate::ui::widgets::{self, ChartStyle, Layer};

const ROW_HEIGHT: f32 = 42.0;
const HEADER_HEIGHT: f32 = 30.0;

/// Uma linha: uma porta e o processo que a escuta.
struct Row<'a> {
    l: &'a Listener,
    p: Option<&'a ProcInfo>,
    kind: Option<Kind>,
    dir: Option<PathBuf>,
    project: Option<String>,
    status: Option<&'a PortStatus>,
    /// Porta publicada por um container: o processo é o do Docker Desktop.
    container: Option<&'a Container>,
    /// Porta escutada pelo Docker ou pelo `wslrelay`: (etiqueta, nome a mostrar).
    external: Option<(&'static str, String)>,
}

impl Row<'_> {
    fn key(&self) -> Option<ProcKey> {
        self.p.map(|p| p.key())
    }

    /// O projeto, se for um servidor de desenvolvimento; senão o nome do processo.
    fn name(&self) -> String {
        self.external
            .as_ref()
            .map(|(_, name)| name.clone())
            .or_else(|| self.project.clone())
            .or_else(|| self.p.map(|p| p.display_name().to_owned()))
            .unwrap_or_else(|| format!("PID {}", self.l.pid))
    }

    fn reply(&self) -> Option<Reply> {
        self.status.and_then(|s| s.reply)
    }

    fn http(&self) -> bool {
        matches!(self.reply(), Some(Reply::Http { .. }))
    }

    /// Segunda linha: o título da página ou da API; sem ele, os argumentos do comando
    /// (num servidor de desenvolvimento) ou o executável (nos demais).
    fn detail(&self) -> Option<String> {
        if let Some(title) = self.status.and_then(|s| s.title()) {
            return Some(title.to_owned());
        }
        if let Some(c) = self.container {
            return Some(c.image.clone());
        }
        let p = self.p?;
        if self.project.is_some() {
            let args = args_of(&p.cmd);
            return (!args.is_empty()).then(|| args.to_owned());
        }
        p.exe_hint().map(str::to_owned)
    }

    /// Etiqueta ao lado do nome: a plataforma, ou Docker e WSL.
    fn tag(&self) -> Option<&'static str> {
        self.external.as_ref().map(|(tag, _)| *tag).or(self.kind.map(Kind::label))
    }

    /// Finalizar o dono da porta derrubaria o Docker Desktop inteiro (ou só o
    /// encaminhamento do WSL, com o servidor ainda de pé): a ação certa é outra.
    fn kill_blocked(&self) -> Option<&'static str> {
        match &self.external {
            Some(("Docker", _)) if self.container.is_some() => Some("Porta de container: use Parar container."),
            Some(("Docker", _)) => Some("Porta do Docker Desktop: finalizar derrubaria o Docker inteiro."),
            Some(_) => Some("Porta encaminhada do WSL: pare o servidor dentro do WSL."),
            None => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Port,
    Name,
    Address,
    Pid,
    Response,
    Latency,
    Memory,
    Uptime,
}

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Port => "Porta",
            Col::Name => "Projeto ou processo",
            Col::Address => "Endereço",
            Col::Pid => "PID",
            Col::Response => "Resposta",
            Col::Latency => "Conexão · 2 min",
            Col::Memory => "Memória",
            Col::Uptime => "Ativo há",
        }
    }

    fn sort(self) -> Option<PortSort> {
        match self {
            Col::Port => Some(PortSort::Port),
            Col::Name => Some(PortSort::Name),
            Col::Pid => Some(PortSort::Pid),
            Col::Response => Some(PortSort::Response),
            Col::Memory => Some(PortSort::Memory),
            Col::Uptime => Some(PortSort::Uptime),
            Col::Address | Col::Latency => None,
        }
    }

    fn right(self) -> bool {
        matches!(self, Col::Pid | Col::Memory | Col::Uptime)
    }

}

impl TableCol for Col {
    fn key(self) -> &'static str {
        match self {
            Col::Port => "port",
            Col::Name => "name",
            Col::Address => "address",
            Col::Pid => "pid",
            Col::Response => "response",
            Col::Latency => "latency",
            Col::Memory => "memory",
            Col::Uptime => "uptime",
        }
    }

    fn initial_width(self) -> Option<f32> {
        match self {
            Col::Port => Some(76.0),
            Col::Name => None,
            Col::Address => Some(120.0),
            Col::Pid => Some(64.0),
            Col::Response => Some(118.0),
            Col::Latency => Some(92.0),
            Col::Memory | Col::Uptime => Some(88.0),
        }
    }

    fn min_width(self) -> f32 {
        match self {
            Col::Port | Col::Memory | Col::Uptime => 64.0,
            Col::Name => 200.0,
            Col::Address | Col::Latency => 60.0,
            Col::Pid => 48.0,
            Col::Response => 70.0,
        }
    }
}

enum Action {
    Select(ProcKey, u16),
    Open(String),
    Copy(String),
    Kill(ProcKey, u16),
    Restart(ProcKey, u16),
    Watch(u16),
    Explore(PathBuf),
    ShowProcess(ProcKey),
    ShowAll,
    StopContainer(String),
}

pub fn show(app: &mut App, ui: &mut Ui, snap: &Snapshot) {
    let statuses = app.port_status.clone();
    let procs: HashMap<u32, &ProcInfo> = snap.processes.iter().map(|p| (p.pid, p)).collect();
    let info = app.container_info.clone();
    let listed = build_rows(snap, &procs, &statuses, &app.port_ranges, app.ports_all, &info);
    let total = listed.len();
    let terms = search_terms(&app.port_search);
    let mut rows: Vec<Row> = listed.into_iter().filter(|r| matches(r, &terms)).collect();
    rows.sort_by(|a, b| {
        let ord = compare(a, b, app.port_sort);
        if app.port_sort_desc { ord.reverse() } else { ord }
    });
    let selected = rows.iter().position(|r| Some(r.l.port) == app.selected_port && r.key() == app.selected);

    let mut actions: Vec<Action> = Vec::new();
    toolbar(app, ui, rows.len(), total, selected.map(|i| &rows[i]), &mut actions);
    ui.add_space(12.0);
    let visible: HashSet<(u16, u32)> = rows.iter().map(|r| (r.l.port, r.l.pid)).collect();
    ranges_bar(app, ui, snap, &procs, &statuses, &visible, &mut actions);
    ui.add_space(12.0);
    conflicts_banner(app, ui, snap, &procs, &mut actions);
    keyboard(app, ui, &rows, selected, &mut actions);

    if rows.is_empty() {
        empty_state(app, ui, &mut actions);
    } else {
        table(app, ui, snap, &rows, &mut actions);
    }

    for action in actions {
        match action {
            Action::Select(key, port) => app.select_port(key, port),
            Action::Open(url) => platform::shell_open(&url),
            Action::Copy(text) => ui.ctx().copy_text(text),
            Action::Kill(key, port) => app.request_kill_port(key, port),
            Action::Restart(key, port) => app.request_restart(key, port),
            Action::Watch(port) => app.toggle_watch(port),
            Action::Explore(dir) => platform::shell_open(&dir.display().to_string()),
            Action::ShowProcess(key) => {
                app.view = View::Processes;
                app.select(Some(key));
                app.scroll_to_selected = true;
            }
            Action::ShowAll => app.ports_all = true,
            Action::StopContainer(name) => app.containers.stop(name),
        }
    }
}

fn build_rows<'a>(
    snap: &'a Snapshot,
    procs: &HashMap<u32, &'a ProcInfo>,
    statuses: &'a Statuses,
    ranges: &[PortRange],
    all: bool,
    info: &'a containers::Info,
) -> Vec<Row<'a>> {
    snap.listeners
        .iter()
        .filter_map(|l| {
            let p = procs.get(&l.pid).copied();
            let kind = p.and_then(ports::classify);
            // O Docker Desktop escuta a porta publicada duas vezes: pelo backend e pelo
            // `wslrelay` (no `::1`). As duas linhas são do container.
            let container = p
                .filter(|p| containers::is_docker(&p.name) || containers::is_wsl(&p.name))
                .and_then(|_| info.docker.get(&l.port));
            let external = match p.map(|p| p.name.as_str()) {
                Some(n) if containers::is_docker(n) || container.is_some() => {
                    Some(("Docker", container.map(|c| c.name.clone()).unwrap_or_else(|| "Docker Desktop".to_owned())))
                }
                Some(n) if containers::is_wsl(n) => Some(("WSL", info.wsl_label())),
                _ => None,
            };
            // Containers e servidores no WSL são de desenvolvimento: aparecem sem "Todas".
            let wsl = external.as_ref().is_some_and(|(tag, _)| *tag == "WSL");
            if !all && kind.is_none() && container.is_none() && !wsl && !ports::in_ranges(ranges, l.port) {
                return None;
            }
            let dir = kind.and(p).and_then(ports::project_dir);
            let project = dir.as_deref().and_then(ports::project_label);
            Some(Row { l, p, kind, dir, project, status: statuses.get(&(l.port, l.pid)), container, external })
        })
        .collect()
}

fn toolbar(app: &mut App, ui: &mut Ui, shown: usize, total: usize, selected: Option<&Row>, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Portas").font(theme::semibold(20.0)).color(INK));
        ui.add_space(2.0);
        let count = if shown == total { format!("{total}") } else { format!("{shown} de {total}") };
        ui.label(RichText::new(count).font(theme::regular(13.0)).color(INK_3));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(c) = selected.and_then(|r| r.container) {
                // Porta de container: o botão para o container, não o Docker Desktop.
                if widgets::danger_button(ui, &format!("{}  Parar container", icon::STOP), true)
                    .on_hover_text(format!("docker stop {} (Delete)", c.name))
                    .clicked()
                {
                    actions.push(Action::StopContainer(c.name.clone()));
                }
            } else {
                let blocked = selected.and_then(Row::kill_blocked);
                let target = selected.and_then(|r| r.p.map(|p| (p.key(), p.name.clone(), r.l.port)));
                let protected = target.as_ref().is_some_and(|(k, name, _)| app.is_protected(*k, name));
                let enabled = target.is_some() && !protected && blocked.is_none();
                let kill = widgets::danger_button(ui, &format!("{}  Liberar porta", icon::X_CIRCLE), enabled);
                let kill = match blocked {
                    Some(reason) => kill.on_disabled_hover_text(reason),
                    None if protected => kill.on_disabled_hover_text("Processo crítico do sistema: finalizar derrubaria o Windows."),
                    None => kill.on_hover_text("Finalizar o processo que escuta a porta selecionada (Delete)"),
                };
                if kill.clicked()
                    && let Some((key, _, port)) = target
                {
                    actions.push(Action::Kill(key, port));
                }
            }

            let open = selected.filter(|r| r.http());
            let button = egui::Button::new(
                RichText::new(format!("{}  Abrir", icon::ARROW_SQUARE_OUT)).font(theme::regular(13.0)).color(INK_2),
            )
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
            .corner_radius(CornerRadius::same(R_SM))
            .min_size(vec2(0.0, 32.0));
            let response = ui.add_enabled(open.is_some(), button);
            if response.on_hover_text("Abrir no navegador (Enter ou clique duplo)").clicked()
                && let Some(r) = open
            {
                actions.push(Action::Open(r.l.url()));
            }

            let all_text = RichText::new(format!("{}  Todas", icon::LIST_BULLETS))
                .font(theme::regular(13.0))
                .color(if app.ports_all { INK } else { INK_2 });
            let all_button = egui::Button::selectable(app.ports_all, all_text)
                .corner_radius(CornerRadius::same(R_SM))
                .min_size(vec2(0.0, 32.0));
            let hint = "Mostrar também serviços do Windows e outros programas, além dos servidores de desenvolvimento e das faixas";
            if ui.add(all_button).on_hover_text(hint).clicked() {
                app.ports_all = !app.ports_all;
            }

            ui.add_space(4.0);
            widgets::search_box(
                ui,
                &mut app.port_search,
                "port_search",
                "Porta, projeto ou título",
                "Ctrl+F. Use vírgula para buscar vários termos de uma vez.",
            );
        });
    });
}

/// Uma linha de chips por faixa. Livre fica apagado; ocupado acende na cor da rede.
#[allow(clippy::too_many_arguments)]
fn ranges_bar(
    app: &mut App,
    ui: &mut Ui,
    snap: &Snapshot,
    procs: &HashMap<u32, &ProcInfo>,
    statuses: &Statuses,
    visible: &HashSet<(u16, u32)>,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Faixas");
        ranges_editor(app, ui);
    });
    if app.port_ranges.is_empty() {
        ui.label(RichText::new("Nenhuma faixa definida.").font(theme::regular(12.0)).color(INK_3));
        return;
    }
    // Uma porta pode ter mais de um dono (IPv4 e IPv6 em processos diferentes): vale o primeiro.
    let mut by_port: HashMap<u16, &Listener> = HashMap::new();
    for l in &snap.listeners {
        by_port.entry(l.port).or_insert(l);
    }
    for range in app.port_ranges.clone() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            let (label_rect, _) = ui.allocate_exact_size(vec2(82.0, 24.0), Sense::hover());
            let text = format!("{}–{}", range.start(), range.end());
            ui.painter().text(label_rect.left_center(), Align2::LEFT_CENTER, text, theme::regular(12.0), INK_3);
            for port in range {
                let owner = by_port.get(&port).copied();
                let status = owner.and_then(|l| statuses.get(&(l.port, l.pid)));
                let proc = owner.and_then(|l| procs.get(&l.pid).copied());
                let selected = owner.is_some_and(|l| {
                    app.selected_port == Some(port) && app.selected.is_some_and(|k| k.pid == l.pid)
                });
                let response = chip(ui, port, owner, proc, status, selected);
                if response.clicked()
                    && let (Some(l), Some(p)) = (owner, proc)
                {
                    // A linha pode estar escondida pela busca: limpa para ela aparecer.
                    if !visible.contains(&(l.port, l.pid)) {
                        app.port_search.clear();
                    }
                    actions.push(Action::Select(p.key(), port));
                    app.scroll_to_port = true;
                }
            }
        });
    }
}

fn chip(
    ui: &mut Ui,
    port: u16,
    owner: Option<&Listener>,
    proc: Option<&ProcInfo>,
    status: Option<&PortStatus>,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(46.0, 24.0), Sense::click());
    let reply = status.and_then(|s| s.reply);
    let painter = ui.painter();
    match owner {
        None => {
            painter.rect_stroke(rect, CornerRadius::same(R_SM), Stroke::new(1.0, HAIRLINE_STRONG), egui::StrokeKind::Inside);
            painter.text(rect.center(), Align2::CENTER_CENTER, port.to_string(), theme::regular(12.0), INK_4);
            response.on_hover_text(format!("{port} · livre"))
        }
        Some(l) => {
            // Calado ou com erro 5xx: vermelho. Ocupada e saudável, ou sem HTTP: cor da rede.
            let failing = matches!(reply, Some(r @ (Reply::Http { .. } | Reply::Timeout)) if !r.healthy());
            let tone = if failing { theme::DANGER } else { NETWORK };
            let fill = tone.gamma_multiply(if response.hovered() || selected { 0.28 } else { 0.16 });
            let stroke = tone.gamma_multiply(if selected { 0.95 } else { 0.45 });
            painter.rect(rect, CornerRadius::same(R_SM), fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
            painter.text(rect.center(), Align2::CENTER_CENTER, port.to_string(), theme::regular(12.0), INK);

            let name = proc
                .map(|p| {
                    let project = ports::classify(p).and_then(|_| ports::project_dir(p)).and_then(|d| ports::project_label(&d));
                    project.unwrap_or_else(|| p.display_name().to_owned())
                })
                .unwrap_or_else(|| "processo desconhecido".to_owned());
            let (reply_text, _) = reply_text(reply);
            let mut tip = format!("{port} · {name} · PID {}", l.pid);
            if reply.is_some() {
                tip.push_str(&format!("\n{reply_text}"));
            }
            if let Some(title) = status.and_then(|s| s.title()) {
                tip.push_str(&format!("\n{title}"));
            }
            response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
        }
    }
}

/// Avisos acima da tabela: o mesmo projeto rodando duas vezes (a instância nova pulou de
/// porta) e portas com mais de um dono. Sem "Todas", só o que envolve desenvolvimento
/// ou as faixas, como a própria lista.
fn conflicts_banner(app: &mut App, ui: &mut Ui, snap: &Snapshot, procs: &HashMap<u32, &ProcInfo>, actions: &mut Vec<Action>) {
    let relevant = |port: u16, pids: &[u32]| {
        app.ports_all
            || ports::in_ranges(&app.port_ranges, port)
            || pids.iter().any(|pid| procs.get(pid).is_some_and(|p| ports::classify(p).is_some()))
    };
    let found: Vec<ports::Conflict> = ports::conflicts(&snap.listeners, &snap.processes, |c| snap.parent_of(c))
        .into_iter()
        .filter(|c| match c {
            // Docker e WSL escutam a mesma porta em dois processos de propósito.
            ports::Conflict::SharedPort { port, pids } => {
                relevant(*port, pids)
                    && !pids.iter().all(|pid| {
                        procs.get(pid).is_some_and(|p| containers::is_docker(&p.name) || containers::is_wsl(&p.name))
                    })
            }
            ports::Conflict::Duplicate { .. } => true,
        })
        .take(3)
        .collect();
    if found.is_empty() {
        return;
    }
    let name = |pid: u32| procs.get(&pid).map(|p| p.display_name().to_owned()).unwrap_or_else(|| "?".to_owned());
    for conflict in found {
        widgets::card().inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::WARNING).font(theme::regular(16.0)).color(theme::ACCENT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let text = match &conflict {
                        ports::Conflict::Duplicate { project, kind, old, new } => {
                            let (old_port, old_key) = *old;
                            let (new_port, new_key) = *new;
                            if widgets::danger_button(ui, &format!("Liberar {old_port}"), true)
                                .on_hover_text("Finalizar a instância antiga")
                                .clicked()
                            {
                                actions.push(Action::Kill(old_key, old_port));
                            }
                            if widgets::ghost_button(ui, "Ver").clicked() {
                                actions.push(Action::Select(old_key, old_port));
                                app.scroll_to_port = true;
                            }
                            let age = |k: ProcKey| procs.get(&k.pid).map(|p| format::duration(p.run_time)).unwrap_or_default();
                            format!(
                                "{project} ({}) está rodando duas vezes: a porta {old_port} é de uma instância aberta há {}, \
                                 e a de {} subiu na {new_port}.",
                                kind.label(),
                                age(old_key),
                                age(new_key),
                            )
                        }
                        ports::Conflict::SharedPort { port, pids } => {
                            if let Some(key) = pids.iter().find_map(|pid| procs.get(pid).map(|p| p.key()))
                                && widgets::ghost_button(ui, "Ver").clicked()
                            {
                                actions.push(Action::Select(key, *port));
                                app.scroll_to_port = true;
                            }
                            let owners: Vec<String> = pids.iter().map(|pid| format!("{} (PID {pid})", name(*pid))).collect();
                            format!(
                                "A porta {port} tem {} donos: {}. Quem conecta cai num ou noutro conforme o endereço (IPv4 ou IPv6).",
                                pids.len(),
                                owners.join(", ")
                            )
                        }
                    };
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(text).font(theme::regular(12.5)).color(INK_2)).wrap());
                    });
                });
            });
        });
        ui.add_space(8.0);
    }
    ui.add_space(4.0);
}

/// Ícone que abre o campo das faixas. Texto inválido fica no campo com o erro embaixo.
fn ranges_editor(app: &mut App, ui: &mut Ui) {
    let button = widgets::icon_button(ui, icon::SLIDERS_HORIZONTAL, "Editar faixas de portas");
    egui::Popup::from_toggle_button_response(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(300.0);
            ui.label(RichText::new("Faixas de portas").font(theme::semibold(13.5)).color(INK));
            ui.label(
                RichText::new("As portas das faixas aparecem sempre na lista e são sondadas por HTTP.")
                    .font(theme::regular(12.0))
                    .color(INK_3),
            );
            ui.add_space(6.0);
            ranges_field(app, ui);
        });
}

/// Campo das faixas com a validação embaixo; também nas configurações.
pub fn ranges_field(app: &mut App, ui: &mut Ui) {
    let mut text = app.port_ranges_text.clone();
    let edit = egui::TextEdit::singleline(&mut text)
        .hint_text(ports::DEFAULT_RANGES)
        .font(theme::mono(12.5))
        .desired_width(f32::INFINITY);
    if ui.add(edit).changed() {
        let _ = app.set_port_ranges(text);
    }
    match ports::parse_ranges(&app.port_ranges_text) {
        Err(e) => {
            ui.label(RichText::new(e).font(theme::regular(12.0)).color(theme::DANGER));
        }
        Ok(_) => {
            ui.label(
                RichText::new("Ex.: 5173-5183, 8000-8010, 3000. Até 64 portas por faixa.")
                    .font(theme::regular(12.0))
                    .color(INK_4),
            );
        }
    }
    ui.add_space(4.0);
    if app.port_ranges_text != ports::DEFAULT_RANGES && widgets::ghost_button(ui, "Restaurar padrão").clicked() {
        let _ = app.set_port_ranges(ports::DEFAULT_RANGES.to_owned());
    }
}

fn table(app: &mut App, ui: &mut Ui, snap: &Snapshot, rows: &[Row], actions: &mut Vec<Action>) {
    let width = ui.available_width();
    let mut cols = vec![Col::Port, Col::Name];
    if width > 780.0 {
        cols.push(Col::Address);
    }
    cols.extend([Col::Pid, Col::Response]);
    if width > 660.0 {
        cols.push(Col::Latency);
    }
    cols.push(Col::Memory);
    if width > 880.0 {
        cols.push(Col::Uptime);
    }
    // Porta e nome ficam sempre: são a linha.
    cols.retain(|&c| matches!(c, Col::Port | Col::Name) || app.column_visible("ports", c.key()));
    // Sem sondagem HTTP, as colunas de resposta só teriam traços.
    if !app.probe_http {
        cols.retain(|&c| !matches!(c, Col::Response | Col::Latency));
    }

    let scroll_row = if app.scroll_to_port {
        app.scroll_to_port = false;
        rows.iter().position(|r| Some(r.l.port) == app.selected_port && r.key() == app.selected)
    } else {
        None
    };

    let mut header_cells = Vec::with_capacity(cols.len());
    let table_columns = columns::columns(ui, "ports", &cols);
    let mut builder = TableBuilder::new(ui)
        .id_salt("ports")
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .min_scrolled_height(0.0)
        .auto_shrink([false, false]);
    for column in table_columns {
        builder = builder.column(column);
    }
    if let Some(row) = scroll_row {
        builder = builder.scroll_to_row(row, None);
    }

    builder
        .header(HEADER_HEIGHT, |mut header| {
            for col in &cols {
                header.col(|ui| {
                    header_cells.push(ui.max_rect());
                    header_cell(app, ui, *col);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                let r = &rows[row.index()];
                row.set_selected(Some(r.l.port) == app.selected_port && r.key().is_some() && r.key() == app.selected);
                for col in &cols {
                    row.col(|ui| cell(app, ui, snap, r, *col));
                }
                let response = row.response();
                if let Some(key) = r.key()
                    && (response.clicked() || response.secondary_clicked())
                {
                    actions.push(Action::Select(key, r.l.port));
                }
                if response.double_clicked() && r.http() {
                    actions.push(Action::Open(r.l.url()));
                }
                response.context_menu(|ui| port_menu(app, ui, r, actions));
            });
        });
    columns::dividers(ui, "ports", &cols, &header_cells, rows.len(), ROW_HEIGHT);
}

fn header_cell(app: &mut App, ui: &mut Ui, col: Col) {
    let rect = ui.max_rect();
    let response = col.sort().map(|_| ui.interact(rect, ui.id().with(col.label()), Sense::click()));
    let active = col.sort() == Some(app.port_sort);
    let caret = if app.port_sort_desc { icon::CARET_DOWN } else { icon::CARET_UP };
    let label = match (active, col.right()) {
        (true, true) => format!("{caret} {}", col.label()),
        (true, false) => format!("{} {caret}", col.label()),
        (false, _) => col.label().to_owned(),
    };
    let hovered = response.as_ref().is_some_and(|r| r.hovered());
    let color = if active || hovered { INK_2 } else { INK_3 };
    let (x, align) = if col.right() {
        (rect.right() - 10.0, Align2::RIGHT_BOTTOM)
    } else {
        (rect.left() + 8.0, Align2::LEFT_BOTTOM)
    };
    let painter = ui.painter();
    painter.text(pos2(x, rect.bottom() - 7.0), align, label, theme::regular(11.5), color);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));

    if let Some(response) = response
        && response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
        && let Some(sort) = col.sort()
    {
        if active {
            app.port_sort_desc = !app.port_sort_desc;
        } else {
            app.port_sort = sort;
            app.port_sort_desc = matches!(sort, PortSort::Memory | PortSort::Uptime);
        }
    }
}

fn cell(app: &mut App, ui: &mut Ui, snap: &Snapshot, r: &Row, col: Col) {
    match col {
        Col::Port => port_cell(ui, r, app.is_watched(r.l.port)),
        Col::Name => name_cell(app, ui, r),
        Col::Address => address_cell(ui, r.l),
        Col::Pid => widgets::right_text(ui, &r.l.pid.to_string(), INK_3),
        Col::Response => {
            let rect = ui.max_rect();
            let (text, color) = reply_text(r.reply());
            ui.painter().text(pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, text, theme::regular(12.5), color);
        }
        Col::Latency => {
            if let Some(s) = r.status
                && s.latency.values().len() >= 2
            {
                let rect = ui.max_rect().shrink2(vec2(8.0, 11.0));
                let max = widgets::nice_max(s.latency.max(), 10.0);
                widgets::paint_chart(ui, rect, &[Layer::area(&s.latency, NETWORK)], max, ChartStyle::SPARK);
            }
        }
        Col::Memory => match r.p {
            Some(p) => widgets::heat_cell(ui, &format::bytes(p.memory), mem_heat(p.memory, snap.mem_total), false),
            None => widgets::right_text(ui, "—", INK_4),
        },
        Col::Uptime => {
            let text = r.p.map(|p| format::duration(p.run_time)).unwrap_or_else(|| "—".to_owned());
            widgets::right_text(ui, &text, INK_3);
        }
    }
}

/// Número da porta com o ponto de estado: aceso quando responde, vermelho quando falha.
fn port_cell(ui: &mut Ui, r: &Row, watched: bool) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let dot = pos2(rect.left() + 14.0, rect.center().y);
    match r.reply() {
        Some(reply @ (Reply::Http { .. } | Reply::Timeout)) => {
            let color = if reply.healthy() { theme::OK } else { theme::DANGER };
            painter.circle_filled(dot, 6.0, color.gamma_multiply(0.16));
            painter.circle_filled(dot, 2.8, color);
        }
        _ => {
            painter.circle_filled(dot, 2.4, INK_4);
        }
    }
    let number = painter.text(
        pos2(rect.left() + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        r.l.port.to_string(),
        theme::semibold(13.5),
        INK,
    );
    // Porta vigiada: o sino ao lado do número (avisa quando ela cai ou volta).
    if watched {
        painter.text(
            pos2(number.right() + 5.0, rect.center().y),
            Align2::LEFT_CENTER,
            icon::BELL_SIMPLE_RINGING,
            theme::regular(12.0),
            theme::ACCENT,
        );
    }
}

/// Ícone, nome com a etiqueta da plataforma e, embaixo, o título ou o comando.
/// Desenhado direto no retângulo da célula, cortando com reticências.
fn name_cell(app: &mut App, ui: &mut Ui, r: &Row) {
    let rect = ui.max_rect();
    let texture = app.icons.get(r.p.and_then(|p| p.exe.as_deref()));
    let icon_rect = Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(18.0, 18.0));
    let painter = ui.painter();
    match &texture {
        Some(tex) => {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(tex.id(), icon_rect, uv, Color32::WHITE);
        }
        None => {
            painter.text(icon_rect.center(), Align2::CENTER_CENTER, icon::APP_WINDOW, theme::regular(16.0), INK_4);
        }
    }

    let x = icon_rect.right() + 10.0;
    let width = (rect.right() - x - 8.0).max(0.0);
    let detail = r.detail();
    let line1_y = if detail.is_some() { rect.center().y - 8.5 } else { rect.center().y };

    let tag = r.tag();
    let tag_galley = tag.map(|t| text_galley(ui, t, theme::regular(11.0), INK_2, f32::INFINITY));
    let tag_width = tag_galley.as_ref().map_or(0.0, |g| g.size().x + 10.0 + 6.0);
    let name = text_galley(ui, &r.name(), theme::semibold(13.0), INK, (width - tag_width).max(20.0));
    let name_width = name.size().x;
    let painter = ui.painter();
    painter.galley(pos2(x, line1_y - name.size().y / 2.0), name, INK);
    if let Some(tag) = tag_galley {
        let pill = Rect::from_min_size(pos2(x + name_width + 6.0, line1_y - 8.0), vec2(tag.size().x + 10.0, 16.0));
        painter.rect_filled(pill, CornerRadius::same(8), GRAPHITE_3);
        painter.galley(pill.center() - tag.size() / 2.0, tag, INK_2);
    }
    if let Some(detail) = detail {
        let galley = text_galley(ui, &detail, theme::regular(12.0), INK_3, width);
        ui.painter().galley(pos2(x, rect.center().y + 9.0 - galley.size().y / 2.0), galley, INK_3);
    }
}

/// Endereços em que a porta escuta. Exposta na rede ganha o globo e o texto claro.
fn address_cell(ui: &mut Ui, l: &Listener) {
    let rect = ui.max_rect();
    let mut x = rect.left() + 8.0;
    let exposed = l.exposed();
    if exposed {
        ui.painter().text(pos2(x, rect.center().y), Align2::LEFT_CENTER, icon::GLOBE_SIMPLE, theme::regular(13.0), INK_2);
        x += 18.0;
    }
    let color = if exposed { INK_2 } else { INK_3 };
    let galley = text_galley(ui, &l.addrs_text(), theme::regular(12.0), color, (rect.right() - x - 6.0).max(0.0));
    ui.painter().galley(pos2(x, rect.center().y - galley.size().y / 2.0), galley, color);
}

fn text_galley(ui: &Ui, text: &str, font: egui::FontId, color: Color32, max_width: f32) -> Arc<egui::Galley> {
    ui.fonts_mut(|f| {
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
        f.layout_job(job)
    })
}

/// Texto e cor da última resposta. Também usado no painel de detalhes.
pub fn reply_text(reply: Option<Reply>) -> (String, Color32) {
    match reply {
        Some(Reply::Http { status, ms }) => {
            let ms = if ms < 10.0 { format!("{ms:.1}").replace('.', ",") } else { format!("{ms:.0}") };
            (format!("{status} · {ms} ms"), if status < 500 { INK_2 } else { theme::DANGER })
        }
        Some(Reply::Timeout) => ("sem resposta".to_owned(), theme::DANGER),
        Some(Reply::NotHttp) => ("não é HTTP".to_owned(), INK_4),
        Some(Reply::Refused) => ("recusou a conexão".to_owned(), INK_4),
        None => ("—".to_owned(), INK_4),
    }
}

fn port_menu(app: &mut App, ui: &mut Ui, r: &Row, actions: &mut Vec<Action>) {
    ui.set_min_width(240.0);
    let url = r.l.url();
    let open = egui::Button::new(format!("{}  Abrir no navegador", icon::ARROW_SQUARE_OUT)).shortcut_text("Enter");
    if ui.add_enabled(r.http(), open).clicked() {
        actions.push(Action::Open(url.clone()));
        ui.close();
    }
    if ui.button(format!("{}  Copiar URL", icon::LINK)).clicked() {
        actions.push(Action::Copy(url));
        ui.close();
    }
    if ui.button(format!("{}  Copiar porta", icon::HASH)).clicked() {
        actions.push(Action::Copy(r.l.port.to_string()));
        ui.close();
    }
    let Some(p) = r.p else { return };
    ui.separator();
    if let Some(c) = r.container {
        let stop = egui::Button::new(RichText::new(format!("{}  Parar container", icon::STOP)).color(theme::DANGER))
            .shortcut_text("Del");
        if ui.add(stop).on_hover_text(format!("docker stop {}", c.name)).clicked() {
            actions.push(Action::StopContainer(c.name.clone()));
            ui.close();
        }
        if ui.button(format!("{}  Copiar nome do container", icon::COPY)).clicked() {
            actions.push(Action::Copy(c.name.clone()));
            ui.close();
        }
    }
    let protected = app.is_protected(p.key(), &p.name);
    let blocked = r.kill_blocked();
    if r.container.is_none() {
        let kill = egui::Button::new(RichText::new(format!("{}  Liberar porta", icon::X_CIRCLE)).color(theme::DANGER))
            .shortcut_text("Del");
        let kill = ui.add_enabled(!protected && blocked.is_none(), kill);
        let kill = match blocked {
            Some(reason) => kill.on_disabled_hover_text(reason),
            None => kill.on_hover_text("Finaliza o processo que escuta a porta"),
        };
        if kill.clicked() {
            actions.push(Action::Kill(p.key(), r.l.port));
            ui.close();
        }
    }
    if protected {
        ui.label(RichText::new("Processo crítico do sistema").font(theme::regular(11.5)).color(INK_3));
    }
    let watched = app.is_watched(r.l.port);
    let (glyph, text) = if watched {
        (icon::BELL_SIMPLE_SLASH, "Parar de vigiar a porta")
    } else {
        (icon::BELL_SIMPLE_RINGING, "Vigiar porta")
    };
    if ui
        .button(format!("{glyph}  {text}"))
        .on_hover_text("Avisa na bandeja quando a porta cair e quando voltar")
        .clicked()
    {
        actions.push(Action::Watch(r.l.port));
        ui.close();
    }
    let restart = app.restart_target(p.key());
    let button = ui
        .add_enabled(restart.is_ok(), egui::Button::new(format!("{}  Reiniciar servidor", icon::ARROW_CLOCKWISE)))
        .on_hover_text("Finaliza e roda o mesmo comando numa janela de terminal nova");
    let button = match restart {
        Err(reason) => button.on_disabled_hover_text(reason),
        Ok(_) => button,
    };
    if button.clicked() {
        actions.push(Action::Restart(p.key(), r.l.port));
        ui.close();
    }
    ui.separator();
    if let Some(dir) = &r.dir
        && ui.button(format!("{}  Abrir pasta do projeto", icon::FOLDER_OPEN)).clicked()
    {
        actions.push(Action::Explore(dir.clone()));
        ui.close();
    }
    if ui.button(format!("{}  Ver na lista de processos", icon::LIST_BULLETS)).clicked() {
        actions.push(Action::ShowProcess(p.key()));
        ui.close();
    }
}

fn keyboard(app: &mut App, ui: &Ui, rows: &[Row], selected: Option<usize>, actions: &mut Vec<Action>) {
    if app.confirm.is_some() || ui.ctx().memory(|m| m.focused().is_some()) {
        return;
    }
    let (up, down, enter, delete, escape) = ui.input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::Enter),
            i.key_pressed(Key::Delete),
            i.key_pressed(Key::Escape),
        )
    });
    // Linhas sem processo conhecido não podem ser selecionadas.
    let selectable: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].p.is_some()).collect();
    if up || down {
        let current = selected.and_then(|s| selectable.iter().position(|&i| i == s));
        let next = match (current, down) {
            (None, _) => selectable.first(),
            (Some(i), true) => selectable.get((i + 1).min(selectable.len().saturating_sub(1))),
            (Some(i), false) => selectable.get(i.saturating_sub(1)),
        };
        if let Some(&i) = next
            && let Some(key) = rows[i].key()
        {
            actions.push(Action::Select(key, rows[i].l.port));
            app.scroll_to_port = true;
        }
    }
    if let Some(r) = selected.map(|i| &rows[i]) {
        if enter && r.http() {
            actions.push(Action::Open(r.l.url()));
        }
        if delete && let Some(c) = r.container {
            actions.push(Action::StopContainer(c.name.clone()));
        } else if delete
            && let Some(p) = r.p
            && r.kill_blocked().is_none()
            && !app.is_protected(p.key(), &p.name)
        {
            actions.push(Action::Kill(p.key(), r.l.port));
        }
    }
    if escape {
        app.select(None);
    }
}

fn empty_state(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    ui.add_space(48.0);
    ui.vertical_centered(|ui| {
        if !app.port_search.trim().is_empty() {
            ui.label(RichText::new(icon::MAGNIFYING_GLASS).font(theme::regular(30.0)).color(INK_4));
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Nenhuma porta corresponde a “{}”", app.port_search.trim()))
                    .font(theme::regular(14.0))
                    .color(INK_2),
            );
            ui.label(
                RichText::new("A busca considera porta, PID, projeto, processo e título.").font(theme::regular(12.5)).color(INK_3),
            );
            return;
        }
        ui.label(RichText::new(icon::PLUGS).font(theme::regular(30.0)).color(INK_4));
        ui.add_space(6.0);
        ui.label(RichText::new("Nenhum servidor de desenvolvimento escutando").font(theme::regular(14.0)).color(INK_2));
        ui.label(
            RichText::new("Vite, Node, Python, .NET e o que estiver nas faixas aparece aqui.")
                .font(theme::regular(12.5))
                .color(INK_3),
        );
        if !app.ports_all {
            ui.add_space(10.0);
            if widgets::ghost_button(ui, "Mostrar todas as portas").clicked() {
                actions.push(Action::ShowAll);
            }
        }
    });
}

/// Uma linha aparece se casar com qualquer termo. A porta casa pelo começo
/// (`517` acha 5173 a 5179); o PID, só inteiro.
fn matches(r: &Row, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let port = r.l.port.to_string();
    let pid = r.l.pid.to_string();
    let mut text = r.name();
    if let Some(p) = r.p {
        text.push(' ');
        text.push_str(&p.name);
    }
    if let Some(title) = r.status.and_then(|s| s.title()) {
        text.push(' ');
        text.push_str(title);
    }
    if let Some(tag) = r.tag() {
        text.push(' ');
        text.push_str(tag);
    }
    if let Some(c) = r.container {
        text.push(' ');
        text.push_str(&c.image);
    }
    let text = text.to_lowercase();
    terms.iter().any(|t| port.starts_with(t.as_str()) || pid == *t || text.contains(t.as_str()))
}

fn compare(a: &Row, b: &Row, key: PortSort) -> std::cmp::Ordering {
    let by_port = a.l.port.cmp(&b.l.port).then(a.l.pid.cmp(&b.l.pid));
    match key {
        PortSort::Port => by_port,
        PortSort::Name => a.name().to_lowercase().cmp(&b.name().to_lowercase()).then(by_port),
        PortSort::Pid => a.l.pid.cmp(&b.l.pid).then(by_port),
        // Em faixas, como na tela de processos: o tempo de resposta e a memória oscilam a
        // cada rodada, e o valor exato fazia portas parecidas trocarem de lugar.
        PortSort::Response => band(response_rank(a) as f64, 1.0).cmp(&band(response_rank(b) as f64, 1.0)).then(by_port),
        PortSort::Memory => {
            let mem = |r: &Row| r.p.map_or(-1, |p| band(p.memory as f64, MB));
            mem(a).cmp(&mem(b)).then(by_port)
        }
        PortSort::Uptime => a.p.map(|p| p.run_time).cmp(&b.p.map(|p| p.run_time)).then(by_port),
    }
}

/// Mais rápida primeiro; depois as caladas e por fim as que não falam HTTP.
fn response_rank(r: &Row) -> f32 {
    match r.reply() {
        Some(Reply::Http { ms, .. }) => ms,
        Some(Reply::Timeout) => 1e6,
        Some(Reply::NotHttp | Reply::Refused) => 1e7,
        None => 1e8,
    }
}

/// Argumentos da linha de comando, sem o executável.
fn args_of(cmd: &str) -> &str {
    let cmd = cmd.trim_start();
    let rest = match cmd.strip_prefix('"') {
        Some(quoted) => quoted.split_once('"').map_or("", |(_, rest)| rest),
        None => cmd.split_once(' ').map_or("", |(_, rest)| rest),
    };
    rest.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argumentos_sem_o_executavel() {
        assert_eq!(args_of(r#""C:\Program Files\Python\python.exe" -m uvicorn main:app"#), "-m uvicorn main:app");
        assert_eq!(args_of("python.exe -m uvicorn main:app --port 8001"), "-m uvicorn main:app --port 8001");
        assert_eq!(args_of("node"), "");
    }

    #[test]
    fn texto_da_resposta() {
        assert_eq!(reply_text(Some(Reply::Http { status: 200, ms: 3.24 })).0, "200 · 3,2 ms");
        assert_eq!(reply_text(Some(Reply::Http { status: 404, ms: 18.4 })).0, "404 · 18 ms");
        assert_eq!(reply_text(Some(Reply::Http { status: 502, ms: 5.0 })).1, theme::DANGER);
        assert_eq!(reply_text(None).0, "—");
    }
}
