//! Lista de processos: busca, ordenação, agrupamento por nome e ações.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use egui::{Align, Align2, CornerRadius, Key, Layout, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;

use crate::app::{App, SortKey};
use crate::format;
use crate::sampler::{ProcInfo, ProcKey, Snapshot};
use crate::theme::{self, HAIRLINE, INK, INK_2, INK_3, INK_4, R_SM};
use crate::ui::columns::{self, TableCol};
use crate::ui::widgets;

const ROW_HEIGHT: f32 = 30.0;
const HEADER_HEIGHT: f32 = 42.0;
/// Faixa da seta no começo da linha do grupo: o clique ali abre e fecha o grupo.
const CARET_WIDTH: f32 = 26.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    Pid,
    User,
    Cpu,
    Memory,
    Disk,
    Gpu,
}

impl Col {
    fn sort_key(self) -> SortKey {
        match self {
            Col::Name => SortKey::Name,
            Col::Pid => SortKey::Pid,
            Col::User => SortKey::User,
            Col::Cpu => SortKey::Cpu,
            Col::Memory => SortKey::Memory,
            Col::Disk => SortKey::Disk,
            Col::Gpu => SortKey::Gpu,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Col::Name => "Nome",
            Col::Pid => "PID",
            Col::User => "Usuário",
            Col::Cpu => "CPU",
            Col::Memory => "Memória",
            Col::Disk => "Disco",
            Col::Gpu => "GPU",
        }
    }

    fn numeric(self) -> bool {
        !matches!(self, Col::Name | Col::User)
    }

}

impl TableCol for Col {
    fn key(self) -> &'static str {
        match self {
            Col::Name => "name",
            Col::Pid => "pid",
            Col::User => "user",
            Col::Cpu => "cpu",
            Col::Memory => "memory",
            Col::Disk => "disk",
            Col::Gpu => "gpu",
        }
    }

    fn initial_width(self) -> Option<f32> {
        match self {
            Col::Name => None,
            Col::Pid => Some(66.0),
            Col::User => Some(118.0),
            Col::Cpu => Some(74.0),
            Col::Memory => Some(92.0),
            Col::Disk => Some(88.0),
            Col::Gpu => Some(66.0),
        }
    }

    fn min_width(self) -> f32 {
        match self {
            Col::Name => 170.0,
            Col::Pid | Col::Gpu => 48.0,
            Col::User => 60.0,
            Col::Cpu => 56.0,
            Col::Memory | Col::Disk => 64.0,
        }
    }
}

/// Uma linha da tabela: processo solto, cabeçalho de grupo ou membro de grupo aberto.
enum Row<'a> {
    /// `depth`: 0 fora de grupo, 1 no grupo, mais 1 por nível abaixo do pai no grupo.
    Proc { p: &'a ProcInfo, depth: u8 },
    Group(Group<'a>),
}

struct Group<'a> {
    name: &'a str,
    members: Vec<&'a ProcInfo>,
    /// O processo principal: o único do grupo cujo pai não é do grupo (o processo
    /// principal do ClickUp, com os de renderização embaixo). `None` com várias cópias
    /// independentes, como dois terminais rodando `ping`.
    root: Option<&'a ProcInfo>,
    cpu: f32,
    memory: u64,
    disk: f64,
    gpu: f32,
}

enum Action {
    Select(ProcKey),
    Kill(ProcKey),
    KillTree(ProcKey),
    KillGroup(String),
    Suspend(ProcKey),
    ToggleGroup(String),
    Explore(PathBuf),
    Copy(String),
    Terminal(ProcKey),
    KillOrphans(Vec<ProcKey>),
    DismissOrphans(Vec<ProcKey>),
    ShowOrphans(Vec<ProcKey>),
}

pub fn show(app: &mut App, ui: &mut Ui, snap: &Snapshot) {
    super::system_tabs(app, ui);
    let terms = search_terms(&app.search);
    let visible: Vec<&ProcInfo> = snap.processes.iter().filter(|p| matches(p, &terms)).collect();
    let rows = build_rows(app, &visible, !terms.is_empty());
    // A ordem deste quadro desempata o próximo: processos na mesma faixa não trocam de lugar.
    app.prev_rank.clear();
    app.prev_group_rank.clear();
    for (i, row) in rows.iter().enumerate() {
        match row {
            Row::Proc { p, .. } => {
                app.prev_rank.insert(p.key(), i);
            }
            Row::Group(g) => {
                app.prev_group_rank.insert(g.name.to_owned(), i);
            }
        }
    }

    toolbar(app, ui, snap, visible.len());
    ui.add_space(12.0);
    // Servidores cujo terminal fechou: aviso em cima da lista e marca na linha.
    let orphans: HashSet<ProcKey> =
        crate::ports::orphan_servers(&snap.processes, &snap.listeners, |c| snap.parent_of(c)).iter().map(|p| p.key()).collect();
    let mut actions: Vec<Action> = Vec::new();
    orphans_banner(app, ui, snap, &orphans, &mut actions);

    keyboard(app, ui, &rows);

    let width = ui.available_width();
    let mut cols = vec![Col::Name, Col::Pid];
    if width > 720.0 {
        cols.push(Col::User);
    }
    cols.extend([Col::Cpu, Col::Memory, Col::Disk]);
    if !snap.gpus.is_empty() && width > 600.0 {
        cols.push(Col::Gpu);
    }
    // O nome fica sempre: é por ele que se acha o processo.
    cols.retain(|&c| c == Col::Name || app.column_visible("processes", c.key()));

    if rows.is_empty() {
        empty_state(ui, &app.search);
        apply(app, ui, snap, actions);
        return;
    }

    let selected = app.selected;
    let scroll_row = if app.scroll_to_selected {
        app.scroll_to_selected = false;
        rows.iter().position(|r| matches!(r, Row::Proc { p, .. } if Some(p.key()) == selected))
    } else {
        None
    };

    let mut header_cells = Vec::with_capacity(cols.len());
    let table_columns = columns::columns(ui, "processes", &cols);
    let mut table = TableBuilder::new(ui)
        .id_salt("processes")
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .min_scrolled_height(0.0)
        .auto_shrink([false, false]);
    for column in table_columns {
        table = table.column(column);
    }
    if let Some(row) = scroll_row {
        table = table.scroll_to_row(row, None);
    }

    table
        .header(HEADER_HEIGHT, |mut header| {
            for col in &cols {
                header.col(|ui| {
                    header_cells.push(ui.max_rect());
                    header_cell(app, ui, snap, *col);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                let r = &rows[row.index()];
                match r {
                    Row::Proc { p, depth } => {
                        row.set_selected(Some(p.key()) == selected);
                        for col in &cols {
                            row.col(|ui| proc_cell(app, ui, snap, p, *depth, *col, orphans.contains(&p.key())));
                        }
                        let response = row.response();
                        if response.clicked() || response.secondary_clicked() {
                            actions.push(Action::Select(p.key()));
                        }
                        response.context_menu(|ui| proc_menu(app, ui, p, &mut actions));
                    }
                    Row::Group(g) => {
                        // Fechado, o grupo fica marcado quando o principal está selecionado.
                        let open = app.expanded.contains(g.name);
                        row.set_selected(!open && g.root.is_some_and(|r| Some(r.key()) == selected));
                        for col in &cols {
                            row.col(|ui| group_cell(app, ui, snap, g, *col));
                        }
                        let response = row.response();
                        // A seta e o duplo clique abrem o grupo; o clique no resto da linha
                        // seleciona o processo principal (e daí o painel tem "Árvore").
                        let on_caret = response.interact_pointer_pos().is_some_and(|pos| pos.x < response.rect.left() + CARET_WIDTH);
                        if response.double_clicked() || (response.clicked() && (on_caret || g.root.is_none())) {
                            actions.push(Action::ToggleGroup(g.name.to_owned()));
                        } else if (response.clicked() || response.secondary_clicked())
                            && let Some(root) = g.root
                        {
                            actions.push(Action::Select(root.key()));
                        }
                        response.context_menu(|ui| group_menu(app, ui, g, &mut actions));
                    }
                }
            });
        });
    columns::dividers(ui, "processes", &cols, &header_cells, rows.len(), ROW_HEIGHT);
    apply(app, ui, snap, actions);
}

fn apply(app: &mut App, ui: &mut Ui, snap: &Snapshot, actions: Vec<Action>) {
    for action in actions {
        match action {
            Action::Select(key) => app.select(Some(key)),
            Action::Kill(key) => app.request_kill(key, false),
            Action::KillTree(key) => app.request_kill(key, true),
            Action::KillGroup(name) => app.request_kill_group(&name),
            Action::Suspend(key) => app.toggle_suspend(key),
            Action::ToggleGroup(name) => {
                if !app.expanded.remove(&name) {
                    app.expanded.insert(name);
                }
            }
            Action::Explore(path) => crate::platform::open_in_explorer(&path),
            Action::Copy(text) => ui.ctx().copy_text(text),
            Action::Terminal(key) => {
                if let Some(p) = snap.find(key) {
                    app.open_terminal_for(p);
                }
            }
            Action::KillOrphans(keys) => app.request_kill_orphans(&keys),
            Action::DismissOrphans(keys) => app.orphans_dismissed.extend(keys),
            Action::ShowOrphans(keys) => {
                app.search = keys.iter().map(|k| k.pid.to_string()).collect::<Vec<_>>().join(", ");
            }
        }
    }
}

fn toolbar(app: &mut App, ui: &mut Ui, snap: &Snapshot, visible: usize) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Processos").font(theme::semibold(20.0)).color(INK));
        ui.add_space(2.0);
        let total = snap.processes.len();
        let count = if visible == total { format!("{total}") } else { format!("{visible} de {total}") };
        ui.label(RichText::new(count).font(theme::regular(13.0)).color(INK_3));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let selected = app.selected.and_then(|k| snap.find(k).map(|p| (k, p.name.clone())));
            let protected = selected.as_ref().is_some_and(|(k, name)| app.is_protected(*k, name));
            let response = widgets::danger_button(ui, &format!("{}  Finalizar", icon::X_CIRCLE), selected.is_some() && !protected);
            let response = if protected {
                response.on_disabled_hover_text("Processo crítico do sistema: finalizar derrubaria o Windows.")
            } else {
                response.on_hover_text("Finalizar o processo selecionado (Delete)")
            };
            if response.clicked()
                && let Some((key, _)) = selected
            {
                app.request_kill(key, false);
            }

            let group_text = RichText::new(format!("{}  Agrupar", icon::STACK))
                .font(theme::regular(13.0))
                .color(if app.group { INK } else { INK_2 });
            let group_button = egui::Button::selectable(app.group, group_text)
                .corner_radius(CornerRadius::same(R_SM))
                .min_size(vec2(0.0, 32.0));
            if ui.add(group_button).on_hover_text("Juntar processos com o mesmo nome").clicked() {
                app.group = !app.group;
            }

            let locks_button = egui::Button::new(
                RichText::new(format!("{}  Arquivo em uso", icon::FILE_LOCK)).font(theme::regular(13.0)).color(INK_2),
            )
            .fill(egui::Color32::TRANSPARENT)
            .corner_radius(CornerRadius::same(R_SM))
            .min_size(vec2(0.0, 32.0));
            if ui
                .add(locks_button)
                .on_hover_text("Quem está usando um arquivo ou pasta. Também dá para soltar arquivos na janela.")
                .clicked()
            {
                app.locks.open = true;
            }

            ui.add_space(4.0);
            widgets::search_box(
                ui,
                &mut app.search,
                "search",
                "Nome, PID ou caminho",
                "Ctrl+F. Use vírgula para buscar vários termos de uma vez.",
            );
        });
    });
}

fn header_cell(app: &mut App, ui: &mut Ui, snap: &Snapshot, col: Col) {
    let rect = ui.max_rect();
    let response = ui.interact(rect, ui.id().with(col.label()), Sense::click());
    let active = app.sort == col.sort_key();
    let p = ui.painter();

    let total = match col {
        Col::Cpu => Some(format::percent(snap.cpu)),
        Col::Memory => Some(format::percent(snap.mem_percent())),
        Col::Disk => Some(format::rate(snap.disk_read + snap.disk_write)),
        Col::Gpu => Some(format::percent(snap.gpus.iter().map(|g| g.utilization).fold(0.0, f32::max))),
        _ => None,
    };
    let caret = if app.sort_desc { icon::CARET_DOWN } else { icon::CARET_UP };
    // A seta fica do lado de dentro: antes do rótulo nas colunas alinhadas à direita.
    let label = match (active, col.numeric()) {
        (true, true) => format!("{caret} {}", col.label()),
        (true, false) => format!("{} {caret}", col.label()),
        (false, _) => col.label().to_owned(),
    };
    let label_color = if active || response.hovered() { INK_2 } else { INK_3 };

    let (x, align) = if col.numeric() {
        (rect.right() - 10.0, Align2::RIGHT_BOTTOM)
    } else {
        (rect.left() + 8.0, Align2::LEFT_BOTTOM)
    };
    p.text(pos2(x, rect.bottom() - 7.0), align, label, theme::regular(11.5), label_color);
    if let Some(total) = total {
        p.text(pos2(x, rect.bottom() - 22.0), align, total, theme::semibold(13.5), INK);
    }
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));

    if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        if active {
            app.sort_desc = !app.sort_desc;
        } else {
            app.sort = col.sort_key();
            app.sort_desc = col.numeric() && col != Col::Pid;
        }
    }
}

fn proc_cell(app: &mut App, ui: &mut Ui, snap: &Snapshot, p: &ProcInfo, depth: u8, col: Col, orphan: bool) {
    match col {
        Col::Name => {
            // No grupo, cada nível abaixo do principal recua mais um pouco (até 3).
            ui.add_space(match depth {
                0 => 8.0,
                d => 34.0 + 16.0 * f32::from(d.min(4) - 1),
            });
            let texture = app.icons.get(p.exe.as_deref());
            widgets::process_icon(ui, texture.as_ref(), 16.0);
            ui.add_space(4.0);
            if p.suspended {
                suspended_pill(ui);
            }
            if orphan {
                pill(ui, &format!("{}  sem terminal", icon::LINK_BREAK), theme::ACCENT)
                    .on_hover_text("O terminal ou o editor que abriu este servidor já fechou, e ele continua rodando.");
            }
            name_label(ui, p.display_name(), p.exe_hint(), theme::regular(13.0));
        }
        Col::Pid => widgets::right_text(ui, &p.pid.to_string(), INK_3),
        Col::User => {
            ui.add_space(8.0);
            let user = p.user.as_deref().unwrap_or("—");
            ui.add(egui::Label::new(RichText::new(user).font(theme::regular(12.5)).color(INK_3)).truncate());
        }
        Col::Cpu => widgets::heat_cell(ui, &format::percent(p.cpu), cpu_heat(p.cpu), false),
        Col::Memory => widgets::heat_cell(ui, &format::bytes(p.memory), mem_heat(p.memory, snap.mem_total), false),
        Col::Disk => widgets::heat_cell(ui, &format::rate(p.disk_rate()), disk_heat(p.disk_rate()), false),
        Col::Gpu => widgets::heat_cell(ui, &format::percent(p.gpu), gpu_heat(p.gpu), false),
    }
}

fn group_cell(app: &mut App, ui: &mut Ui, snap: &Snapshot, g: &Group, col: Col) {
    match col {
        Col::Name => {
            let open = app.expanded.contains(g.name);
            ui.add_space(4.0);
            ui.label(
                RichText::new(if open { icon::CARET_DOWN } else { icon::CARET_RIGHT }).font(theme::regular(12.0)).color(INK_3),
            );
            let first = g.members[0];
            let texture = app.icons.get(first.exe.as_deref());
            widgets::process_icon(ui, texture.as_ref(), 16.0);
            ui.add_space(4.0);
            count_pill(ui, g.members.len());
            ui.add_space(2.0);
            name_label(ui, first.display_name(), first.exe_hint(), theme::semibold(13.0));
        }
        Col::Pid | Col::User => {}
        Col::Cpu => widgets::heat_cell(ui, &format::percent(g.cpu), cpu_heat(g.cpu), true),
        Col::Memory => widgets::heat_cell(ui, &format::bytes(g.memory), mem_heat(g.memory, snap.mem_total), true),
        Col::Disk => widgets::heat_cell(ui, &format::rate(g.disk), disk_heat(g.disk), true),
        Col::Gpu => widgets::heat_cell(ui, &format::percent(g.gpu), gpu_heat(g.gpu), true),
    }
}

/// Nome amigável em destaque e, se acrescentar algo, o executável apagado ao lado.
/// Corta com reticências na largura da coluna.
fn name_label(ui: &mut Ui, primary: &str, hint: Option<&str>, font: egui::FontId) {
    let mut job = egui::text::LayoutJob::default();
    job.append(primary, 0.0, egui::TextFormat::simple(font, INK));
    if let Some(hint) = hint {
        job.append(hint, 8.0, egui::TextFormat::simple(theme::regular(12.0), INK_3));
    }
    ui.add(egui::Label::new(job).truncate());
}

/// "Suspenso" antes do nome: o processo não roda nada até ser retomado.
pub(super) fn suspended_pill(ui: &mut Ui) {
    pill(ui, &format!("{}  suspenso", icon::PAUSE), theme::ACCENT);
}

/// Etiqueta pequena antes do nome, na cor dada.
fn pill(ui: &mut Ui, text: &str, color: egui::Color32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), theme::regular(11.0), color);
    let (rect, response) = ui.allocate_exact_size(vec2(galley.size().x + 12.0, 18.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(9), color.gamma_multiply(0.14));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    ui.add_space(2.0);
    response
}

/// Aviso dos servidores de desenvolvimento que ficaram sem terminal. Some quando eles
/// encerram ou quando o usuário dispensa (só nesta execução).
fn orphans_banner(app: &App, ui: &mut Ui, snap: &Snapshot, orphans: &HashSet<ProcKey>, actions: &mut Vec<Action>) {
    let mut keys: Vec<ProcKey> = orphans.iter().filter(|k| !app.orphans_dismissed.contains(k)).copied().collect();
    if keys.is_empty() {
        return;
    }
    keys.sort_by_key(|k| k.start);
    let names: Vec<String> = keys
        .iter()
        .filter_map(|k| snap.find(*k))
        .map(|p| {
            let project = crate::ports::project_dir(p).and_then(|d| crate::ports::project_label(&d));
            let kind = crate::ports::classify(p).map(|k| k.label()).unwrap_or_default();
            match project {
                Some(project) => format!("{project} ({kind})"),
                None => format!("{} ({kind})", p.display_name()),
            }
        })
        .collect();
    let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let more = if names.len() > 3 { format!(" e mais {}", names.len() - 3) } else { String::new() };
    let n = keys.len();
    let text = if n == 1 {
        format!("1 servidor de desenvolvimento continua rodando sem terminal: {shown}.")
    } else {
        format!("{n} servidores de desenvolvimento continuam rodando sem terminal: {shown}{more}.")
    };
    widgets::card().inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::LINK_BREAK).font(theme::regular(16.0)).color(theme::ACCENT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::icon_button(ui, icon::X, "Dispensar até reabrir o app").clicked() {
                    actions.push(Action::DismissOrphans(keys.clone()));
                }
                let label = if n == 1 { "Finalizar".to_owned() } else { "Finalizar todos".to_owned() };
                if widgets::danger_button(ui, &label, true).on_hover_text("Finaliza cada um com os processos filhos").clicked() {
                    actions.push(Action::KillOrphans(keys.clone()));
                }
                if widgets::ghost_button(ui, "Ver").on_hover_text("Filtrar a lista por eles").clicked() {
                    actions.push(Action::ShowOrphans(keys.clone()));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(text).font(theme::regular(12.5)).color(INK_2)).wrap());
                });
            });
        });
    });
    ui.add_space(10.0);
}

fn count_pill(ui: &mut Ui, n: usize) {
    let text = n.to_string();
    let font = theme::regular(11.0);
    let width = 10.0 + 6.5 * text.len() as f32;
    let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(9), theme::GRAPHITE_3);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, INK_2);
}

fn proc_menu(app: &mut App, ui: &mut Ui, p: &ProcInfo, actions: &mut Vec<Action>) {
    ui.set_min_width(220.0);
    let protected = app.is_protected(p.key(), &p.name);
    let kill = ui.add_enabled(
        !protected,
        egui::Button::new(RichText::new(format!("{}  Finalizar processo", icon::X_CIRCLE)).color(theme::DANGER))
            .shortcut_text("Del"),
    );
    if kill.clicked() {
        actions.push(Action::Kill(p.key()));
        ui.close();
    }
    if ui.add_enabled(!protected, egui::Button::new(format!("{}  Finalizar árvore", icon::TREE_STRUCTURE))).clicked() {
        actions.push(Action::KillTree(p.key()));
        ui.close();
    }
    if protected {
        ui.label(RichText::new("Processo crítico do sistema").font(theme::regular(11.5)).color(INK_3));
    }
    let suspended = p.suspended;
    let blocked = crate::control::suspend_blocked(p.pid, &p.name);
    let (glyph, text) = if suspended { (icon::PLAY, "Retomar") } else { (icon::PAUSE, "Suspender") };
    let button = ui.add_enabled(suspended || blocked.is_none(), egui::Button::new(format!("{glyph}  {text}")));
    let button = match blocked {
        Some(reason) if !suspended => button.on_disabled_hover_text(reason),
        _ => button.on_hover_text("Congela o processo sem finalizar: ele não usa CPU até ser retomado"),
    };
    if button.clicked() {
        actions.push(Action::Suspend(p.key()));
        ui.close();
    }
    ui.separator();
    if let Some(exe) = &p.exe {
        if ui.button(format!("{}  Abrir local do arquivo", icon::FOLDER_OPEN)).clicked() {
            actions.push(Action::Explore(exe.clone()));
            ui.close();
        }
        if ui.button(format!("{}  Copiar caminho", icon::COPY)).clicked() {
            actions.push(Action::Copy(exe.display().to_string()));
            ui.close();
        }
    }
    if (p.cwd.is_some() || p.exe.is_some()) && ui.button(format!("{}  Abrir terminal na pasta", icon::TERMINAL_WINDOW)).clicked() {
        actions.push(Action::Terminal(p.key()));
        ui.close();
    }
    if !p.cmd.is_empty() && ui.button(format!("{}  Copiar linha de comando", icon::TERMINAL)).clicked() {
        actions.push(Action::Copy(p.cmd.clone()));
        ui.close();
    }
    if ui.button(format!("{}  Copiar PID", icon::HASH)).clicked() {
        actions.push(Action::Copy(p.pid.to_string()));
        ui.close();
    }
}

fn group_menu(app: &mut App, ui: &mut Ui, g: &Group, actions: &mut Vec<Action>) {
    ui.set_min_width(220.0);
    let open = app.expanded.contains(g.name);
    if ui.button(format!("{}  {}", if open { icon::CARET_UP } else { icon::CARET_DOWN }, if open { "Recolher" } else { "Expandir" })).clicked() {
        actions.push(Action::ToggleGroup(g.name.to_owned()));
        ui.close();
    }
    let protected = g.members.iter().any(|p| app.is_protected(p.key(), &p.name));
    let kill = ui.add_enabled(
        !protected,
        egui::Button::new(RichText::new(format!("{}  Finalizar todos ({})", icon::X_CIRCLE, g.members.len())).color(theme::DANGER)),
    );
    if kill.clicked() {
        actions.push(Action::KillGroup(g.name.to_owned()));
        ui.close();
    }
    // Do principal para baixo: leva junto os filhos com outro nome (crashpad, helpers).
    if let Some(root) = g.root
        && ui.add_enabled(!protected, egui::Button::new(format!("{}  Finalizar árvore", icon::TREE_STRUCTURE))).clicked()
    {
        actions.push(Action::KillTree(root.key()));
        ui.close();
    }
}

fn keyboard(app: &mut App, ui: &mut Ui, rows: &[Row]) {
    if app.confirm.is_some() || ui.ctx().memory(|m| m.focused().is_some()) {
        return;
    }
    let (up, down, delete, escape) = ui.input(|i| {
        (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Delete), i.key_pressed(Key::Escape))
    });
    let keys: Vec<ProcKey> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Proc { p, .. } => Some(p.key()),
            Row::Group(_) => None,
        })
        .collect();
    if up || down {
        let current = app.selected.and_then(|k| keys.iter().position(|x| *x == k));
        let next = match (current, down) {
            (None, _) => keys.first().copied(),
            (Some(i), true) => keys.get((i + 1).min(keys.len().saturating_sub(1))).copied(),
            (Some(i), false) => keys.get(i.saturating_sub(1)).copied(),
        };
        if next.is_some() {
            app.select(next);
            app.scroll_to_selected = true;
        }
    }
    if delete && let Some(key) = app.selected {
        let name = app.snap.as_ref().and_then(|s| s.find(key)).map(|p| p.name.clone());
        if let Some(name) = name
            && !app.is_protected(key, &name)
        {
            app.request_kill(key, false);
        }
    }
    if escape {
        app.select(None);
    }
}

fn empty_state(ui: &mut Ui, search: &str) {
    ui.add_space(60.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(icon::MAGNIFYING_GLASS).font(theme::regular(30.0)).color(INK_4));
        ui.add_space(6.0);
        ui.label(RichText::new(format!("Nenhum processo corresponde a “{search}”")).font(theme::regular(14.0)).color(INK_2));
        ui.label(RichText::new("A busca considera nome, PID, caminho e linha de comando.").font(theme::regular(12.5)).color(INK_3));
    });
}

// Calor por coluna: o ponto de saturação é onde a carga passa a ser motivo de atenção.
fn cpu_heat(v: f32) -> f32 {
    v / 60.0
}
pub(super) fn mem_heat(bytes: u64, total: u64) -> f32 {
    crate::sampler::percent_of(bytes, total) / 20.0
}
fn disk_heat(rate: f64) -> f32 {
    (rate / (25.0 * 1024.0 * 1024.0)) as f32
}
fn gpu_heat(v: f32) -> f32 {
    v / 60.0
}

pub(super) fn search_terms(query: &str) -> Vec<String> {
    query.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect()
}

/// Um processo aparece se casar com qualquer termo (nome, PID, caminho ou linha de comando).
fn matches(p: &ProcInfo, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let name = p.name.to_lowercase();
    let description = p.description.as_deref().unwrap_or_default().to_lowercase();
    let pid = p.pid.to_string();
    let exe = p.exe.as_ref().map(|e| e.display().to_string().to_lowercase()).unwrap_or_default();
    let cmd = p.cmd.to_lowercase();
    terms
        .iter()
        .any(|t| name.contains(t) || description.contains(t) || pid == *t || exe.contains(t) || cmd.contains(t))
}

fn build_rows<'a>(app: &App, procs: &[&'a ProcInfo], searching: bool) -> Vec<Row<'a>> {
    let (key, desc) = (app.sort, app.sort_desc);
    let rank = |p: &ProcInfo| app.prev_rank.get(&p.key()).copied().unwrap_or(usize::MAX);
    let order = |a: &ProcInfo, b: &ProcInfo| {
        let ord = compare(a, b, key);
        let ord = if desc { ord.reverse() } else { ord };
        // Empate na faixa: fica quem já estava na frente; depois, o PID.
        ord.then(rank(a).cmp(&rank(b))).then(a.pid.cmp(&b.pid))
    };
    let group_rank = |name: &str| app.prev_group_rank.get(name).copied().unwrap_or(usize::MAX);

    if !app.group {
        let mut list: Vec<&ProcInfo> = procs.to_vec();
        list.sort_by(|a, b| order(a, b));
        return list.into_iter().map(|p| Row::Proc { p, depth: 0 }).collect();
    }

    let mut by_name: HashMap<&str, Vec<&ProcInfo>> = HashMap::new();
    for p in procs {
        by_name.entry(p.name.as_str()).or_default().push(p);
    }
    let mut groups: Vec<Group> = by_name
        .into_iter()
        .map(|(name, members)| Group {
            name,
            cpu: members.iter().map(|p| p.cpu).sum(),
            memory: members.iter().map(|p| p.memory).sum(),
            disk: members.iter().map(|p| p.disk_rate()).sum(),
            gpu: members.iter().map(|p| p.gpu).fold(0.0, f32::max),
            root: None,
            members,
        })
        .collect();
    groups.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name | SortKey::Pid | SortKey::User => a.members[0]
                .display_name()
                .to_lowercase()
                .cmp(&b.members[0].display_name().to_lowercase()),
            SortKey::Cpu => band(a.cpu as f64, 0.1).cmp(&band(b.cpu as f64, 0.1)),
            SortKey::Memory => band(a.memory as f64, MB).cmp(&band(b.memory as f64, MB)),
            SortKey::Disk => band(a.disk, KB).cmp(&band(b.disk, KB)),
            SortKey::Gpu => band(a.gpu as f64, 0.1).cmp(&band(b.gpu as f64, 0.1)),
        };
        let ord = if desc { ord.reverse() } else { ord };
        ord.then(group_rank(a.name).cmp(&group_rank(b.name))).then(a.name.cmp(b.name))
    });

    let mut rows = Vec::new();
    for mut g in groups {
        if g.members.len() == 1 {
            rows.push(Row::Proc { p: g.members[0], depth: 0 });
            continue;
        }
        g.members.sort_by(|a, b| order(a, b));
        let tree = member_tree(&g.members);
        g.root = match tree.iter().filter(|(_, depth)| *depth == 1).count() {
            1 => Some(tree[0].0),
            _ => None,
        };
        // Durante a busca os grupos abrem sozinhos, para o resultado aparecer.
        let open = searching || app.expanded.contains(g.name);
        rows.push(Row::Group(g));
        if open {
            rows.extend(tree.into_iter().map(|(p, depth)| Row::Proc { p, depth }));
        }
    }
    rows
}

/// Os membros de um grupo como árvore: cada pai que também é do grupo vem antes dos
/// filhos, que ficam um nível abaixo. `members` chega ordenado, e a ordem vale entre irmãos.
fn member_tree<'a>(members: &[&'a ProcInfo]) -> Vec<(&'a ProcInfo, u8)> {
    // Pai e filho casam por PID e hora de criação: o PID do pai pode ter sido reaproveitado.
    let parent_of = |c: &ProcInfo| {
        members.iter().position(|p| Some(p.pid) == c.parent && p.pid != c.pid && p.start_time <= c.start_time)
    };
    let parents: Vec<Option<usize>> = members.iter().map(|c| parent_of(c)).collect();
    fn visit<'a>(
        i: usize,
        depth: u8,
        members: &[&'a ProcInfo],
        parents: &[Option<usize>],
        seen: &mut [bool],
        out: &mut Vec<(&'a ProcInfo, u8)>,
    ) {
        if std::mem::replace(&mut seen[i], true) {
            return;
        }
        out.push((members[i], depth));
        for child in (0..members.len()).filter(|&c| parents[c] == Some(i)) {
            visit(child, depth.saturating_add(1), members, parents, seen, out);
        }
    }
    let mut seen = vec![false; members.len()];
    let mut out = Vec::with_capacity(members.len());
    for i in (0..members.len()).filter(|&i| parents[i].is_none()) {
        visit(i, 1, members, &parents, &mut seen, &mut out);
    }
    // Ciclo (PIDs reaproveitados de um jeito improvável): ninguém some da lista.
    for i in 0..members.len() {
        visit(i, 1, members, &parents, &mut seen, &mut out);
    }
    out
}

fn compare(a: &ProcInfo, b: &ProcInfo, key: SortKey) -> std::cmp::Ordering {
    match key {
        SortKey::Name => {
            a.display_name().to_lowercase().cmp(&b.display_name().to_lowercase()).then(a.pid.cmp(&b.pid))
        }
        SortKey::Pid => a.pid.cmp(&b.pid),
        SortKey::User => a.user.cmp(&b.user).then(a.name.cmp(&b.name)),
        SortKey::Cpu => band(a.cpu as f64, 0.1).cmp(&band(b.cpu as f64, 0.1)),
        SortKey::Memory => band(a.memory as f64, MB).cmp(&band(b.memory as f64, MB)),
        SortKey::Disk => band(a.disk_rate(), KB).cmp(&band(b.disk_rate(), KB)),
        SortKey::Gpu => band(a.gpu as f64, 0.1).cmp(&band(b.gpu as f64, 0.1)),
    }
}

const KB: f64 = 1024.0;
pub(super) const MB: f64 = 1024.0 * 1024.0;

/// Faixa logarítmica de ~15% para ordenar métricas que oscilam a cada coleta.
/// Comparar o valor exato fazia vizinhos parecidos (352,1 MB e 352,4 MB) trocarem
/// de lugar a todo segundo; dentro da mesma faixa vale a ordem anterior.
/// Abaixo de `floor` tudo cai na faixa 0 (os "0%" não disputam posição).
pub(super) fn band(value: f64, floor: f64) -> i32 {
    if value < floor { 0 } else { ((value / floor).ln() / 1.15f64.ln()) as i32 + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::tests::proc;

    #[test]
    fn busca_por_varios_termos() {
        let mut chrome = proc(10, None, 1, "chrome.exe");
        chrome.exe = Some(PathBuf::from(r"C:\Program Files\Google\Chrome\chrome.exe"));
        let mut code = proc(20, None, 1, "Code.exe");
        code.description = Some("Visual Studio Code".to_owned());
        assert!(matches(&code, &search_terms("visual studio")));
        let terms = search_terms("CODE, google");
        assert!(matches(&chrome, &terms));
        assert!(matches(&code, &terms));
        assert!(matches(&code, &search_terms("20")));
        assert!(!matches(&code, &search_terms("2")), "PID só casa por igualdade");
        assert!(matches(&code, &search_terms("  ")));
    }

    #[test]
    fn grupo_em_arvore_com_o_principal_primeiro() {
        // ClickUp: o principal (pai é o explorer, fora do grupo) e os filhos dele, que
        // chegam antes dele na ordem da lista; um neto embaixo do primeiro filho.
        let renderer = proc(30, Some(10), 5, "ClickUp.exe");
        let gpu = proc(20, Some(10), 5, "ClickUp.exe");
        let main = proc(10, Some(1), 4, "ClickUp.exe");
        let grandchild = proc(40, Some(30), 6, "ClickUp.exe");
        let members = vec![&renderer, &gpu, &main, &grandchild];
        let tree = |members: &[&ProcInfo]| member_tree(members).into_iter().map(|(p, d)| (p.pid, d)).collect::<Vec<_>>();
        assert_eq!(tree(&members), vec![(10, 1), (30, 2), (40, 3), (20, 2)]);

        // O pai do 50 é o PID 40, mas reaproveitado: o 50 nasceu antes. Vira outra raiz.
        let stale = proc(50, Some(40), 1, "ClickUp.exe");
        let members = vec![&renderer, &gpu, &main, &grandchild, &stale];
        assert_eq!(tree(&members), vec![(10, 1), (30, 2), (40, 3), (20, 2), (50, 1)]);
    }

    #[test]
    fn ordena_por_cpu_desc() {
        let mut a = proc(1, None, 1, "a");
        let mut b = proc(2, None, 1, "b");
        a.cpu = 5.0;
        b.cpu = 50.0;
        assert_eq!(compare(&a, &b, SortKey::Cpu), std::cmp::Ordering::Less);
    }

    #[test]
    fn oscilacao_pequena_nao_reordena() {
        let mut a = proc(1, None, 1, "a");
        let mut b = proc(2, None, 1, "b");
        a.memory = 352 * 1024 * 1024 + 100 * 1024;
        b.memory = 352 * 1024 * 1024 + 400 * 1024;
        assert_eq!(compare(&a, &b, SortKey::Memory), std::cmp::Ordering::Equal);
        b.memory = 700 * 1024 * 1024;
        assert_eq!(compare(&a, &b, SortKey::Memory), std::cmp::Ordering::Less);
        assert_eq!(band(0.0, 0.1), 0);
        assert_eq!(band(0.05, 0.1), 0);
        assert!(band(50.0, 0.1) > band(5.0, 0.1));
    }
}
