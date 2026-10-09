//! Barra lateral: marca, navegação e um cartão vivo por recurso.

use egui::{Align2, Color32, CornerRadius, Rect, Response, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as icon;

use crate::app::{App, View};
use crate::format;
use crate::sampler::Snapshot;
use crate::theme::{self, CPU, DISK, GPU, GRAPHITE_1, GRAPHITE_2, INK, INK_2, INK_3, MEMORY, NETWORK, R_MD};
use crate::ui::widgets::{self, ChartStyle, Layer};

pub fn show(app: &mut App, ui: &mut Ui) {
    brand(ui, app.snap.as_deref());
    ui.add_space(14.0);

    let count = app.snap.as_ref().map(|s| s.processes.len()).unwrap_or(0);
    if nav_item(ui, app.view == View::Processes, icon::LIST_BULLETS, "Processos", &count.to_string()).clicked() {
        app.view = View::Processes;
    }
    // O contador acompanha o filtro da tela: só desenvolvimento e faixas, ou todas.
    let ports = match &app.snap {
        Some(s) if app.ports_all => s.listeners.len(),
        Some(_) => app.dev_port_count,
        None => 0,
    };
    if nav_item(ui, app.view == View::Ports, icon::PLUGS_CONNECTED, "Portas", &ports.to_string()).clicked() {
        app.view = View::Ports;
    }

    ui.add_space(12.0);
    widgets::section_label(ui, "Desempenho");
    ui.add_space(4.0);

    // Os cartões rolam se a janela for baixa e houver várias GPUs; o rodapé fica fixo.
    let footer_height = 96.0;
    let scroll_height = (ui.available_height() - footer_height).max(80.0);
    egui::ScrollArea::vertical()
        .max_height(scroll_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            if let Some(snap) = app.snap.clone() {
                resource_cards(app, ui, &snap);
            }
        });

    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| footer(app, ui));
}

fn resource_cards(app: &mut App, ui: &mut Ui, s: &Snapshot) {
    let h = &s.history;
    let cards = [
        (
            View::Cpu,
            icon::CPU,
            "CPU",
            format::percent(s.cpu),
            format!("{} núcleos lógicos", s.cores.len()),
            CPU,
            vec![Layer::area(&h.cpu, CPU)],
            100.0,
        ),
        (
            View::Memory,
            icon::MEMORY,
            "Memória",
            format::percent(s.mem_percent()),
            format!("{} de {}", format::bytes(s.mem_used), format::bytes(s.mem_total)),
            MEMORY,
            vec![Layer::area(&h.memory, MEMORY)],
            100.0,
        ),
        (
            View::Disk,
            icon::HARD_DRIVES,
            "Disco",
            format::rate(s.disk_read + s.disk_write),
            format!("L {} · E {}", format::rate(s.disk_read), format::rate(s.disk_write)),
            DISK,
            vec![Layer::area(&h.disk_read, DISK), Layer::line(&h.disk_write, theme::lerp(DISK, Color32::WHITE, 0.45))],
            widgets::nice_max(h.disk_read.max().max(h.disk_write.max()), 1024.0 * 1024.0),
        ),
        (
            View::Network,
            icon::WIFI_HIGH,
            "Rede",
            format::rate(s.net_rx + s.net_tx),
            format!("↓ {} · ↑ {}", format::rate(s.net_rx), format::rate(s.net_tx)),
            NETWORK,
            vec![Layer::area(&h.net_rx, NETWORK), Layer::line(&h.net_tx, theme::lerp(NETWORK, Color32::WHITE, 0.45))],
            widgets::nice_max(h.net_rx.max().max(h.net_tx.max()), 64.0 * 1024.0),
        ),
    ];
    for (view, glyph, label, value, sub, color, layers, max) in cards {
        if resource_card(ui, app.view == view, glyph, label, &value, &sub, color, &layers, max).clicked() {
            app.view = view;
        }
    }
    for (i, gpu) in s.gpus.iter().enumerate() {
        let label = if s.gpus.len() > 1 { format!("GPU {i}") } else { "GPU".to_owned() };
        let series = &h.gpus[i];
        if resource_card(
            ui,
            app.view == View::Gpu(i),
            icon::GRAPHICS_CARD,
            &label,
            &format::percent(gpu.utilization),
            &gpu.name,
            GPU,
            &[Layer::area(series, GPU)],
            100.0,
        )
        .clicked()
        {
            app.view = View::Gpu(i);
        }
    }
}

fn brand(ui: &mut Ui, snap: Option<&Snapshot>) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
        logo_mark(ui, rect);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new("Process Monitor").font(theme::semibold(14.5)).color(INK));
            let host = snap.map(|s| s.host.as_str()).unwrap_or("");
            ui.label(RichText::new(host).font(theme::regular(11.5)).color(INK_3));
        });
    });
}

/// Marca desenhada: o traço de um sinal terminando no ponto luminoso dos gráficos.
fn logo_mark(ui: &Ui, rect: Rect) {
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(8), GRAPHITE_2);
    p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, theme::HAIRLINE_STRONG), egui::StrokeKind::Inside);
    let r = rect.shrink(6.0);
    let pts = [
        pos2(r.left(), r.center().y + 2.0),
        pos2(r.left() + r.width() * 0.28, r.center().y + 2.0),
        pos2(r.left() + r.width() * 0.42, r.top() + 1.0),
        pos2(r.left() + r.width() * 0.60, r.bottom() - 1.0),
        pos2(r.left() + r.width() * 0.74, r.center().y - 1.0),
        pos2(r.right() - 2.0, r.center().y - 1.0),
    ];
    p.line(pts.to_vec(), Stroke::new(1.6, CPU));
    p.circle_filled(pts[5], 4.5, CPU.gamma_multiply(0.2));
    p.circle_filled(pts[5], 2.0, CPU);
}

fn nav_item(ui: &mut Ui, selected: bool, glyph: &str, label: &str, badge: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    let p = ui.painter();
    let fill = if selected {
        GRAPHITE_2
    } else if response.hovered() {
        GRAPHITE_1
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(R_MD), fill);
    if selected {
        accent_bar(ui, rect, INK_2);
    }
    let text_color = if selected { INK } else { INK_2 };
    p.text(pos2(rect.left() + 14.0, rect.center().y), Align2::LEFT_CENTER, glyph, theme::regular(16.0), text_color);
    p.text(pos2(rect.left() + 38.0, rect.center().y), Align2::LEFT_CENTER, label, theme::semibold(13.5), text_color);
    p.text(pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, badge, theme::regular(12.0), INK_3);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[allow(clippy::too_many_arguments)]
fn resource_card(
    ui: &mut Ui,
    selected: bool,
    glyph: &str,
    label: &str,
    value: &str,
    sub: &str,
    color: Color32,
    layers: &[Layer],
    max: f32,
) -> Response {
    // 76 px: os cinco cartões cabem numa tela de 768 px sem rolar.
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 76.0), Sense::click());
    let p = ui.painter();
    let fill = if selected {
        GRAPHITE_2
    } else if response.hovered() {
        GRAPHITE_1
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(R_MD), fill);
    if selected {
        accent_bar(ui, rect, color);
    }

    let inner = rect.shrink2(vec2(14.0, 9.0));
    let label_color = if selected { INK } else { INK_2 };
    p.text(inner.left_top(), Align2::LEFT_TOP, glyph, theme::regular(15.0), color);
    p.text(inner.left_top() + vec2(22.0, 0.0), Align2::LEFT_TOP, label, theme::semibold(13.0), label_color);
    p.text(inner.right_top(), Align2::RIGHT_TOP, value, theme::semibold(13.0), INK);

    // O subtítulo é cortado com reticências para não invadir o valor.
    let sub_font = theme::regular(11.0);
    let galley = ui.fonts_mut(|f| {
        let mut job = egui::text::LayoutJob::simple_singleline(sub.to_owned(), sub_font, INK_3);
        job.wrap = egui::text::TextWrapping::truncate_at_width(inner.width());
        f.layout_job(job)
    });
    p.galley(inner.left_top() + vec2(0.0, 18.0), galley, INK_3);

    let chart_rect = Rect::from_min_max(pos2(inner.left(), inner.top() + 34.0), inner.right_bottom());
    widgets::paint_chart(ui, chart_rect, layers, max, ChartStyle::SPARK);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn accent_bar(ui: &Ui, rect: Rect, color: Color32) {
    let bar = Rect::from_min_max(pos2(rect.left(), rect.top() + 12.0), pos2(rect.left() + 3.0, rect.bottom() - 12.0));
    ui.painter().rect_filled(bar, CornerRadius::same(2), color);
}

fn footer(app: &mut App, ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = 8.0;
    // bottom_up: o primeiro item fica embaixo.
    ui.horizontal(|ui| {
        ui.label(RichText::new("Atualizar").font(theme::regular(12.0)).color(INK_3));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (ms, text) in [(2000, "2 s"), (1000, "1 s"), (500, "0,5 s")] {
                let selected = app.interval_ms == ms;
                let button = egui::Button::selectable(
                    selected,
                    RichText::new(text).font(theme::regular(12.0)).color(if selected { INK } else { INK_3 }),
                )
                .corner_radius(CornerRadius::same(theme::R_SM))
                .min_size(vec2(40.0, 24.0));
                if ui.add(button).clicked() && !selected {
                    app.set_interval(ms);
                }
            }
        });
    });

    if app.elevated {
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::SHIELD_CHECK).font(theme::regular(15.0)).color(theme::OK));
            ui.label(RichText::new("Executando como administrador").font(theme::regular(12.0)).color(INK_2));
        });
    } else {
        let button = egui::Button::new(
            RichText::new(format!("{}  Executar como administrador", icon::SHIELD_WARNING)).font(theme::regular(12.5)).color(INK_2),
        )
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, theme::HAIRLINE_STRONG))
        .corner_radius(CornerRadius::same(theme::R_SM))
        .min_size(vec2(ui.available_width(), 30.0));
        let response = ui
            .add(button)
            .on_hover_text("Sem administrador, processos do sistema e de outros usuários não podem ser finalizados nem inspecionados.");
        if response.clicked() {
            let ctx = ui.ctx().clone();
            app.relaunch_elevated(&ctx);
        }
    }
    crate::app::hairline(ui);
}
