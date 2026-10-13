//! Programas que abrem com o Windows: liga e desliga cada um, como o Gerenciador de
//! Tarefas, e mostra se ele está rodando agora.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use egui::{Align, Layout, RichText, Sense, Ui};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;

use crate::app::{App, ToastKind, View};
use crate::platform;
use crate::sampler::{ProcKey, Snapshot};
use crate::startup::{self, Entry, Source};
use crate::theme::{self, INK, INK_2, INK_3, INK_4};
use crate::ui::columns::{self, TableCol};
use crate::ui::processes::search_terms;
use crate::ui::widgets;

const ROW_HEIGHT: f32 = 42.0;
const HEADER_HEIGHT: f32 = 30.0;

/// A lista é lida numa thread: a descrição de cada executável abre o arquivo, e o
/// antivírus pode deixar isso lento na primeira vez.
pub struct State {
    entries: Option<Vec<Entry>>,
    pending: Option<Receiver<Vec<Entry>>>,
    search: String,
    selected: Option<(Source, String)>,
    ctx: egui::Context,
}

impl State {
    pub fn new(ctx: egui::Context) -> Self {
        Self { entries: None, pending: None, search: String::new(), selected: None, ctx }
    }

    /// Lê a lista na primeira vez que a tela abre.
    pub fn ensure_loaded(&mut self) {
        if self.entries.is_none() && self.pending.is_none() {
            self.reload();
        }
    }

    pub fn reload(&mut self) {
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("startup".into())
            .spawn(move || {
                let _ = tx.send(startup::list());
                ctx.request_repaint();
            })
            .expect("thread de inicialização");
        self.pending = Some(rx);
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.pending
            && let Ok(entries) = rx.try_recv()
        {
            self.entries = Some(entries);
            self.pending = None;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    State,
    Running,
    Source,
}

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Name => "Programa",
            Col::State => "Abre com o Windows",
            Col::Running => "Agora",
            Col::Source => "Origem",
        }
    }
}

impl TableCol for Col {
    fn key(self) -> &'static str {
        match self {
            Col::Name => "name",
            Col::State => "state",
            Col::Running => "running",
            Col::Source => "source",
        }
    }

    fn initial_width(self) -> Option<f32> {
        match self {
            Col::Name => None,
            Col::State => Some(150.0),
            Col::Running => Some(110.0),
            Col::Source => Some(210.0),
        }
    }

    fn min_width(self) -> f32 {
        match self {
            Col::Name => 200.0,
            Col::State => 120.0,
            Col::Running => 70.0,
            Col::Source => 90.0,
        }
    }
}

enum Action {
    Select(Source, String),
    Toggle(usize, bool),
    Explore(PathBuf),
    Copy(String),
    ShowProcess(ProcKey),
}

pub fn show(app: &mut App, ui: &mut Ui, snap: &Snapshot) {
    super::system_tabs(app, ui);
    let Some(entries) = app.startup.entries.clone() else {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Lendo os programas de inicialização…").color(INK_3).font(theme::regular(14.0)));
        });
        return;
    };
    // Executável → processo vivo, para a coluna "Agora".
    let running: HashMap<String, ProcKey> = snap
        .processes
        .iter()
        .filter_map(|p| Some((p.exe.as_ref()?.to_string_lossy().to_lowercase(), p.key())))
        .collect();
    let terms = search_terms(&app.startup.search);
    let mut rows: Vec<usize> = (0..entries.len())
        .filter(|&i| {
            let e = &entries[i];
            let text = format!("{} {} {}", e.display_name(), e.name, e.command).to_lowercase();
            terms.is_empty() || terms.iter().any(|t| text.contains(t.as_str()))
        })
        .collect();
    let sort = columns::sort_state(ui, "startup", ("name", false));
    let process_of =
        |e: &Entry| e.exe.as_ref().and_then(|x| running.get(&x.to_string_lossy().to_lowercase()).copied());
    sort_rows(&mut rows, &entries, &sort, |e| process_of(e).is_some());

    let mut actions = Vec::new();
    toolbar(app, ui, &entries, rows.len());
    ui.add_space(12.0);

    if rows.is_empty() {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            let text = if entries.is_empty() { "Nenhum programa abre com o Windows" } else { "Nenhum programa corresponde à busca" };
            ui.label(RichText::new(text).font(theme::regular(14.0)).color(INK_2));
        });
        return;
    }

    let width = ui.available_width();
    let mut cols = vec![Col::Name, Col::State, Col::Running];
    if width > 640.0 {
        cols.push(Col::Source);
    }
    let mut header_cells = Vec::with_capacity(cols.len());
    let table_columns = columns::columns(ui, "startup", &cols);
    let mut builder = TableBuilder::new(ui)
        .id_salt("startup")
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .min_scrolled_height(0.0)
        .auto_shrink([false, false]);
    for column in table_columns {
        builder = builder.column(column);
    }
    builder
        .header(HEADER_HEIGHT, |mut header| {
            for col in &cols {
                header.col(|ui| {
                    header_cells.push(ui.max_rect());
                    columns::sort_header(ui, "startup", *col, col.label(), false, false, &sort);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                let index = rows[row.index()];
                let e = &entries[index];
                let process = e.exe.as_ref().and_then(|x| running.get(&x.to_string_lossy().to_lowercase()).copied());
                let selected = app.startup.selected.as_ref().is_some_and(|(s, n)| *s == e.source && *n == e.name);
                row.set_selected(selected);
                for col in &cols {
                    row.col(|ui| cell(app, ui, e, index, process, *col, &mut actions));
                }
                let response = row.response();
                if response.clicked() || response.secondary_clicked() {
                    actions.push(Action::Select(e.source, e.name.clone()));
                }
                response.context_menu(|ui| menu(ui, e, index, process, &mut actions));
            });
        });
    columns::dividers(ui, "startup", &cols, &header_cells, rows.len(), ROW_HEIGHT);

    for action in actions {
        match action {
            Action::Select(source, name) => app.startup.selected = Some((source, name)),
            Action::Toggle(index, on) => toggle(app, index, on),
            Action::Explore(path) => platform::open_in_explorer(&path),
            Action::Copy(text) => ui.ctx().copy_text(text),
            Action::ShowProcess(key) => {
                app.view = View::Processes;
                app.select(Some(key));
                app.scroll_to_selected = true;
            }
        }
    }
}

fn toggle(app: &mut App, index: usize, on: bool) {
    let Some(entry) = app.startup.entries.as_ref().and_then(|e| e.get(index)).cloned() else { return };
    match startup::set_enabled(&entry, on) {
        Ok(()) => {
            if let Some(e) = app.startup.entries.as_mut().and_then(|e| e.get_mut(index)) {
                e.enabled = on;
            }
            let text = if on { "vai abrir com o Windows" } else { "não abre mais com o Windows" };
            app.toast(format!("{} {text}", entry.display_name()), ToastKind::Ok, false);
        }
        Err(e) => {
            let denied = e == startup::Error::AccessDenied;
            let hint = if denied && entry.source.machine() { " (vale para todos os usuários)" } else { "" };
            let text = format!("Não alterou {}{hint}: {}", entry.display_name(), e.describe());
            let elevate = denied && !app.elevated;
            app.toast(text, ToastKind::Error, elevate);
        }
    }
}

fn toolbar(app: &mut App, ui: &mut Ui, entries: &[Entry], shown: usize) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Inicialização").font(theme::semibold(20.0)).color(INK));
        ui.add_space(2.0);
        let on = entries.iter().filter(|e| e.enabled).count();
        let count = if shown == entries.len() {
            format!("{on} de {} ligados", entries.len())
        } else {
            format!("{shown} de {}", entries.len())
        };
        ui.label(RichText::new(count).font(theme::regular(13.0)).color(INK_3));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::icon_button(ui, icon::ARROW_CLOCKWISE, "Ler de novo").clicked() {
                app.startup.reload();
            }
            ui.add_space(4.0);
            widgets::search_box(ui, &mut app.startup.search, "startup_search", "Programa ou comando", "Ctrl+F");
        });
    });
}

/// Ordena pela coluna escolhida; empate fica pelo nome, de A a Z. No sentido crescente,
/// ligados e os que estão rodando vêm primeiro.
fn sort_rows(rows: &mut [usize], entries: &[Entry], sort: &(String, bool), running: impl Fn(&Entry) -> bool) {
    let (key, desc) = (sort.0.as_str(), sort.1);
    rows.sort_by_cached_key(|&i| {
        let e = &entries[i];
        let rank = match key {
            "state" => String::from(if e.enabled { "0" } else { "1" }),
            "running" => String::from(if running(e) { "0" } else { "1" }),
            "source" => e.source.label().to_owned(),
            _ => String::new(),
        };
        (std::cmp::Reverse(desc.then(|| rank.clone())), rank, e.display_name().to_lowercase())
    });
    if desc && key == "name" {
        rows.reverse();
    }
}

fn cell(app: &mut App, ui: &mut Ui, e: &Entry, index: usize, process: Option<ProcKey>, col: Col, actions: &mut Vec<Action>) {
    match col {
        Col::Name => {
            ui.add_space(8.0);
            let texture = app.icons.get(e.exe.as_deref());
            widgets::process_icon(ui, texture.as_ref(), 18.0);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(egui::Label::new(RichText::new(e.display_name()).font(theme::semibold(13.0)).color(INK)).truncate());
                ui.add(egui::Label::new(RichText::new(&e.command).font(theme::regular(11.5)).color(INK_3)).truncate());
            });
        }
        Col::State => {
            ui.add_space(8.0);
            let mut on = e.enabled;
            if widgets::toggle(ui, &mut on).changed() {
                actions.push(Action::Toggle(index, on));
            }
            ui.add_space(4.0);
            let (text, color) = if e.enabled { ("Ligado", INK_2) } else { ("Desligado", INK_4) };
            ui.label(RichText::new(text).font(theme::regular(12.5)).color(color));
        }
        Col::Running => {
            ui.add_space(8.0);
            match process {
                Some(key) => {
                    let link = egui::Button::new(RichText::new(format!("PID {}", key.pid)).font(theme::regular(12.5)).color(theme::ACCENT))
                        .frame(false);
                    if ui.add(link).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Ver o processo").clicked() {
                        actions.push(Action::ShowProcess(key));
                    }
                }
                None => {
                    ui.label(RichText::new("—").font(theme::regular(12.5)).color(INK_4));
                }
            }
        }
        Col::Source => {
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new(e.source.label()).font(theme::regular(12.5)).color(INK_3)).truncate());
        }
    }
}

fn menu(ui: &mut Ui, e: &Entry, index: usize, process: Option<ProcKey>, actions: &mut Vec<Action>) {
    ui.set_min_width(230.0);
    let (glyph, text) = if e.enabled { (icon::TOGGLE_LEFT, "Desligar") } else { (icon::TOGGLE_RIGHT, "Ligar") };
    if ui.button(format!("{glyph}  {text}")).clicked() {
        actions.push(Action::Toggle(index, !e.enabled));
        ui.close();
    }
    ui.separator();
    if let Some(path) = startup::location(e)
        && ui.button(format!("{}  Abrir local do arquivo", icon::FOLDER_OPEN)).clicked()
    {
        actions.push(Action::Explore(path));
        ui.close();
    }
    if ui.button(format!("{}  Copiar comando", icon::TERMINAL)).clicked() {
        actions.push(Action::Copy(e.command.clone()));
        ui.close();
    }
    if let Some(key) = process
        && ui.button(format!("{}  Ver na lista de processos", icon::LIST_BULLETS)).clicked()
    {
        actions.push(Action::ShowProcess(key));
        ui.close();
    }
}
