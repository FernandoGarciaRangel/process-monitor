// Sem console no build de release; em debug ele fica para ver panics e logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod format;
mod gpu;
mod icons;
mod platform;
mod ports;
mod sampler;
mod theme;
mod ui;
mod updater;

fn main() -> eframe::Result {
    // Tamanho ideal 1240×780, mas nunca maior que a área útil da tela (notebooks com
    // 1366×768 não comportam). Na segunda abertura vale o tamanho que o usuário deixou.
    let area = platform::work_area();
    let size = match area {
        Some(a) => egui::vec2((a.width() * 0.94).min(1240.0), (a.height() * 0.94).min(780.0)),
        None => egui::vec2(1240.0, 780.0),
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Process Monitor")
        .with_inner_size(size)
        .with_min_inner_size([size.x.min(980.0), size.y.min(600.0)]);
    if let Some(a) = area {
        // A posição é da janela externa: conta os ~32 pt da barra de título.
        let outer_h = size.y + 32.0;
        let x = a.center().x - size.x / 2.0;
        let y = (a.center().y - outer_h / 2.0).max(a.min.y);
        viewport = viewport.with_position([x, y]);
    }
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/app.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Process Monitor", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
