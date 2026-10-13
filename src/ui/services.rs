//! Serviços do Windows: estado, tipo de inicialização e o processo de cada um, com
//! iniciar, parar e reiniciar. Quase tudo pede administrador; o erro oferece elevar.

use std::collections::HashMap;

use egui::{Align, Color32, CornerRadius, Id, Layout, RichText, Sense, Stroke, Ui, vec2};
use egui_extras::TableBuilder;
use egui_phosphor::regular as icon;

use crate::app::{App, View};
use crate::sampler::{ProcKey, Snapshot};
use crate::services::{Action, Service, StartType, State};
use crate::theme::{self, HAIRLINE_STRONG, INK, INK_2, INK_3, INK_4, R_SM};
use crate::ui::columns::{self, TableCol};
use crate::ui::processes::search_terms;
use crate::ui::widgets;

const ROW_HEIGHT: f32 = 30.0;
const HEADER_HEIGHT: f32 = 30.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    State,
    Start,
    Pid,
}

impl Col {
    fn label(self) -> &'static str {
        match self {
            Col::Name => "Serviço",
            Col::State => "Estado",
            Col::Start => "Inicialização",
            Col::Pid => "PID",
        }
    }
}

impl TableCol for Col {
    fn key(self) -> &'static str {
        match self {
            Col::Name => "name",
            Col::State => "state",
            Col::Start => "start",
            Col::Pid => "pid",
        }
    }

    fn initial_width(self) -> Option<f32> {
        match self {
            Col::Name => None,
            Col::State => Some(120.0),
            Col::Start => Some(170.0),
            Col::Pid => Some(70.0),
        }
    }

    fn min_width(self) -> f32 {
        match self {
            Col::Name => 220.0,
            Col::State => 90.0,
            Col::Start => 90.0,
            Col::Pid => 48.0,
        }
    }
}

enum Do {
    Select(String),
    Act(String, String, Action),
    Copy(String),
    ShowProcess(ProcKey),
}

pub fn show(app: &mut App, ui: &mut Ui, snap: &Snapshot) {
    super::system_tabs(app, ui);
    let listing = app.services.as_ref().and_then(|m| m.latest());
    let Some(listing) = listing else {
        message(ui, "Lendo os serviços…");
        return;
    };
    let services: &Vec<Service> = match &*listing {
        Ok(list) => list,
        Err(e) => {
            message(ui, &format!("Não leu os serviços: {e}"));
            return;
        }
    };

    let running_only_id = Id::new("services_running_only");
    let mut running_only: bool = ui.data_mut(|d| d.get_persisted(running_only_id)).unwrap_or(false);
    let terms = search_terms(&app.service_search);
    let mut rows: Vec<&Service> = services
        .iter()
        .filter(|s| !running_only || s.state == State::Running)
        .filter(|s| {
            let text = format!("{} {} {}", s.display, s.name, s.description.as_deref().unwrap_or_default()).to_lowercase();
            terms.is_empty() || terms.iter().any(|t| text.contains(t.as_str()) || s.pid.to_string() == *t)
        })
        .collect();
    let sort = columns::sort_state(ui, "services", ("name", false));
    sort_rows(&mut rows, &sort);

    let mut actions = Vec::new();
    let selected = app.selected_service.clone().and_then(|n| services.iter().find(|s| s.name == n));
    toolbar(app, ui, services, rows.len(), selected, &mut running_only, &mut actions);
    ui.data_mut(|d| d.insert_persisted(running_only_id, running_only));
    ui.add_space(12.0);

    if rows.is_empty() {
        message(ui, "Nenhum serviço corresponde à busca");
        return;
    }

    let width = ui.available_width();
    let mut cols = vec![Col::Name, Col::State];
    if width > 560.0 {
        cols.push(Col::Start);
    }
    cols.push(Col::Pid);
    let mut header_cells = Vec::with_capacity(cols.len());
    let table_columns = columns::columns(ui, "services", &cols);
    let mut builder = TableBuilder::new(ui)
        .id_salt("services")
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .min_scrolled_height(0.0)
        .auto_shrink([false, false]);
    for column in table_columns {
        builder = builder.column(column);
    }
    let processes: HashMap<u32, ProcKey> = snap.processes.iter().map(|p| (p.pid, p.key())).collect();
    builder
        .header(HEADER_HEIGHT, |mut header| {
            for col in &cols {
                header.col(|ui| {
                    header_cells.push(ui.max_rect());
                    columns::sort_header(ui, "services", *col, col.label(), *col == Col::Pid, false, &sort);
                });
            }
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                let s = rows[row.index()];
                row.set_selected(app.selected_service.as_deref() == Some(s.name.as_str()));
                for col in &cols {
                    row.col(|ui| cell(ui, s, *col, &processes, &mut actions));
                }
                let response = row.response();
                if response.clicked() || response.secondary_clicked() {
                    actions.push(Do::Select(s.name.clone()));
                }
                let response = match &s.description {
                    Some(d) => response.on_hover_text_at_pointer(d),
                    None => response,
                };
                response.context_menu(|ui| menu(ui, s, &processes, &mut actions));
            });
        });
    columns::dividers(ui, "services", &cols, &header_cells, rows.len(), ROW_HEIGHT);

    let ctx = ui.ctx().clone();
    for action in actions {
        match action {
            Do::Select(name) => app.selected_service = Some(name),
            Do::Act(name, display, action) => app.service_action(&ctx, &name, &display, action),
            Do::Copy(text) => ctx.copy_text(text),
            Do::ShowProcess(key) => {
                app.view = View::Processes;
                app.select(Some(key));
                app.scroll_to_selected = true;
            }
        }
    }
}

/// Ordena pela coluna escolhida; empate fica pelo nome. Em execução vem antes de parado,
/// e automático antes de desativado, no sentido crescente.
fn sort_rows(rows: &mut [&Service], sort: &(String, bool)) {
    fn state_rank(s: State) -> u8 {
        match s {
            State::Running => 0,
            State::Starting => 1,
            State::Stopping => 2,
            State::Paused => 3,
            State::Stopped => 4,
        }
    }
    fn start_rank(s: Option<StartType>) -> u8 {
        match s {
            Some(StartType::Auto) => 0,
            Some(StartType::AutoDelayed) => 1,
            Some(StartType::Manual) => 2,
            Some(StartType::Disabled) => 3,
            Some(StartType::System) => 4,
            None => 5,
        }
    }
    let (key, desc) = (sort.0.as_str(), sort.1);
    rows.sort_by_cached_key(|s| {
        let rank = match key {
            "state" => state_rank(s.state) as u64,
            "start" => start_rank(s.start) as u64,
            "pid" => s.pid as u64,
            _ => 0,
        };
        // O sentido vale para a coluna; o desempate pelo nome fica sempre de A a Z.
        (if desc { u64::MAX - rank } else { rank }, s.display.to_lowercase())
    });
    if desc && key == "name" {
        rows.reverse();
    }
}

fn message(ui: &mut Ui, text: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).color(INK_3).font(theme::regular(14.0)));
    });
}

fn toolbar(
    app: &mut App,
    ui: &mut Ui,
    services: &[Service],
    shown: usize,
    selected: Option<&Service>,
    running_only: &mut bool,
    actions: &mut Vec<Do>,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Serviços").font(theme::semibold(20.0)).color(INK));
        ui.add_space(2.0);
        let running = services.iter().filter(|s| s.state == State::Running).count();
        let count = if shown == services.len() {
            format!("{running} em execução de {}", services.len())
        } else {
            format!("{shown} de {}", services.len())
        };
        ui.label(RichText::new(count).font(theme::regular(13.0)).color(INK_3));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Direita para a esquerda: o último botão daqui fica mais à esquerda.
            let restart = ui
                .add_enabled(selected.is_some_and(|s| s.state == State::Running && s.can_stop), bordered(icon::ARROW_CLOCKWISE, "Reiniciar"))
                .on_hover_text("Parar e iniciar de novo");
            if restart.clicked()
                && let Some(s) = selected
            {
                actions.push(Do::Act(s.name.clone(), s.display.clone(), Action::Restart));
            }
            match selected {
                Some(s) if s.state == State::Running => {
                    if ui.add_enabled(s.can_stop, bordered(icon::STOP, "Parar")).clicked() {
                        actions.push(Do::Act(s.name.clone(), s.display.clone(), Action::Stop));
                    }
                }
                _ => {
                    let can_start = selected.is_some_and(|s| s.state == State::Stopped && s.start != Some(StartType::Disabled));
                    if ui.add_enabled(can_start, bordered(icon::PLAY, "Iniciar")).clicked()
                        && let Some(s) = selected
                    {
                        actions.push(Do::Act(s.name.clone(), s.display.clone(), Action::Start));
                    }
                }
            }
            let text = RichText::new(format!("{}  Em execução", icon::PLAY_CIRCLE))
                .font(theme::regular(13.0))
                .color(if *running_only { INK } else { INK_2 });
            let filter = egui::Button::selectable(*running_only, text).corner_radius(CornerRadius::same(R_SM)).min_size(vec2(0.0, 32.0));
            if ui.add(filter).on_hover_text("Mostrar só os serviços em execução").clicked() {
                *running_only = !*running_only;
            }
            ui.add_space(4.0);
            widgets::search_box(ui, &mut app.service_search, "service_search", "Nome, descrição ou PID", "Ctrl+F");
        });
    });
}

fn bordered(glyph: &str, text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(format!("{glyph}  {text}")).font(theme::regular(13.0)).color(INK_2))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
        .corner_radius(CornerRadius::same(R_SM))
        .min_size(vec2(0.0, 32.0))
}

fn state_color(state: State) -> Color32 {
    match state {
        State::Running => theme::OK,
        State::Starting | State::Stopping => theme::ACCENT,
        State::Stopped | State::Paused => INK_4,
    }
}

fn cell(ui: &mut Ui, s: &Service, col: Col, processes: &HashMap<u32, ProcKey>, actions: &mut Vec<Do>) {
    match col {
        Col::Name => {
            ui.add_space(8.0);
            let mut job = egui::text::LayoutJob::default();
            job.append(&s.display, 0.0, egui::TextFormat::simple(theme::regular(13.0), INK));
            if !s.display.eq_ignore_ascii_case(&s.name) {
                job.append(&s.name, 8.0, egui::TextFormat::simple(theme::regular(12.0), INK_3));
            }
            ui.add(egui::Label::new(job).truncate());
        }
        Col::State => {
            ui.add_space(10.0);
            let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.0, state_color(s.state));
            ui.add_space(2.0);
            let color = if s.state == State::Running { INK_2 } else { INK_3 };
            ui.label(RichText::new(s.state.label()).font(theme::regular(12.5)).color(color));
        }
        Col::Start => {
            ui.add_space(8.0);
            let text = s.start.map(StartType::label).unwrap_or("—");
            let color = if s.start == Some(StartType::Disabled) { INK_4 } else { INK_3 };
            ui.label(RichText::new(text).font(theme::regular(12.5)).color(color));
        }
        Col::Pid => {
            let key = (s.pid != 0).then(|| processes.get(&s.pid).copied()).flatten();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                match key {
                    Some(key) => {
                        let link = egui::Button::new(RichText::new(s.pid.to_string()).font(theme::regular(12.5)).color(theme::ACCENT))
                            .frame(false);
                        if ui.add(link).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Ver o processo").clicked() {
                            actions.push(Do::ShowProcess(key));
                        }
                    }
                    None => {
                        ui.label(RichText::new("—").font(theme::regular(12.5)).color(INK_4));
                    }
                }
            });
        }
    }
}

fn menu(ui: &mut Ui, s: &Service, processes: &HashMap<u32, ProcKey>, actions: &mut Vec<Do>) {
    ui.set_min_width(230.0);
    let act = |action| Do::Act(s.name.clone(), s.display.clone(), action);
    let can_start = s.state == State::Stopped && s.start != Some(StartType::Disabled);
    if ui.add_enabled(can_start, egui::Button::new(format!("{}  Iniciar", icon::PLAY))).clicked() {
        actions.push(act(Action::Start));
        ui.close();
    }
    let running = s.state == State::Running && s.can_stop;
    if ui.add_enabled(running, egui::Button::new(format!("{}  Parar", icon::STOP))).clicked() {
        actions.push(act(Action::Stop));
        ui.close();
    }
    if ui.add_enabled(running, egui::Button::new(format!("{}  Reiniciar", icon::ARROW_CLOCKWISE))).clicked() {
        actions.push(act(Action::Restart));
        ui.close();
    }
    ui.separator();
    let editable = s.start.is_some_and(|t| t != StartType::System);
    ui.add_enabled_ui(editable, |ui| {
        ui.menu_button(format!("{}  Inicialização", icon::POWER), |ui| {
            for kind in StartType::CHOICES {
                if ui.radio(s.start == Some(kind), kind.label()).clicked() && s.start != Some(kind) {
                    actions.push(act(Action::SetStart(kind)));
                    ui.close();
                }
            }
        });
    });
    ui.separator();
    if s.pid != 0
        && let Some(&key) = processes.get(&s.pid)
        && ui.button(format!("{}  Ver processo", icon::LIST_BULLETS)).clicked()
    {
        actions.push(Do::ShowProcess(key));
        ui.close();
    }
    if ui.button(format!("{}  Copiar nome", icon::COPY)).clicked() {
        actions.push(Do::Copy(s.name.clone()));
        ui.close();
    }
}
