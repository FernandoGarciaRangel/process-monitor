pub mod columns;
pub mod details;
pub mod locks;
pub mod performance;
pub mod ports;
pub mod processes;
pub mod services;
pub mod settings;
pub mod sidebar;
pub mod startup;
pub mod system;
pub mod widgets;

use egui::{Sense, Stroke, Ui, vec2};

use crate::app::{App, View};
use crate::theme::{self, ACCENT, HAIRLINE, INK, INK_2, INK_3};

/// Abas de Processos, Serviços e Inicialização. Ficam dentro do item "Processos" da
/// barra lateral: mais dois itens lá não cabem na tela de 768 px sem rolar os cartões.
pub fn system_tabs(app: &mut App, ui: &mut Ui) {
    let tabs = [(View::Processes, "Processos"), (View::Services, "Serviços"), (View::Startup, "Inicialização")];
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    ui.painter().hline(row.x_range(), row.bottom() - 0.5, Stroke::new(1.0, HAIRLINE));
    let mut x = row.left();
    for (view, label) in tabs {
        let active = app.view == view;
        let galley = ui.painter().layout_no_wrap(label.to_owned(), theme::semibold(13.0), INK);
        let rect = egui::Rect::from_min_size(egui::pos2(x, row.top()), vec2(galley.size().x + 4.0, row.height()));
        let response = ui.interact(rect, ui.id().with(("system_tab", label)), Sense::click());
        let color = if active {
            INK
        } else if response.hovered() {
            INK_2
        } else {
            INK_3
        };
        ui.painter().galley_with_override_text_color(
            egui::pos2(rect.left() + 2.0, rect.center().y - galley.size().y / 2.0 - 2.0),
            galley,
            color,
        );
        if active {
            ui.painter().hline(rect.x_range(), row.bottom() - 1.0, Stroke::new(2.0, ACCENT));
        }
        if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            app.view = view;
        }
        x = rect.right() + 22.0;
    }
    ui.add_space(14.0);
}
