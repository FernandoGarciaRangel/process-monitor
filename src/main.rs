// Sem console no build de release; em debug ele fica para ver panics e logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alerts;
mod app;
mod containers;
mod control;
mod export;
mod format;
mod gpu;
mod icons;
mod locks;
mod platform;
mod ports;
mod sampler;
mod services;
mod startup;
mod theme;
mod tray;
mod ui;
mod updater;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    // Substituição do Gerenciador de Tarefas (antes da instância única, para não
    // colidir com ela). A instância elevada grava direto no HKLM e sai com código.
    if let Some(v) = args.iter().find_map(|a| a.strip_prefix("--set-taskmgr=")) {
        std::process::exit(i32::from(platform::write_task_manager_replacement(v == "1").is_err()));
    }
    // Chamado pelo instalador (não elevado): liga pedindo administrador.
    if has("--replace-taskmgr") {
        let _ = platform::request_replace_task_manager(true);
        return Ok(());
    }
    // Chamado pelo instalador em toda instalação, inclusive a atualização silenciosa: se
    // a chave aponta para uma cópia em outra pasta, passa a apontar para esta. Só pede
    // administrador nesse caso; no resto, sai na hora.
    if has("--repoint-taskmgr") {
        if platform::task_manager_points_to_other_copy() {
            let _ = platform::request_replace_task_manager(true);
        }
        return Ok(());
    }
    // Chamado pelo desinstalador: só restaura se a chave ainda aponta para nós, para
    // não apagar a de outro programa (Process Explorer) nem pedir UAC à toa.
    if has("--restore-taskmgr") {
        if platform::replaces_task_manager() {
            let _ = platform::request_replace_task_manager(false);
        }
        return Ok(());
    }

    // Já aberto: a outra instância aparece e esta sai.
    let Some(_instance) = tray::single_instance(has("--replace")) else { return Ok(()) };
    let start_minimized = has(platform::MINIMIZED_ARG);

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
    eframe::run_native(platform::APP_NAME, options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, start_minimized)))))
}
