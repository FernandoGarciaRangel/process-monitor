//! Telas de desempenho: um gráfico grande por recurso e as estatísticas em volta.

use egui::{Align, Align2, Color32, Layout, Margin, RichText, Ui, pos2, vec2};

use crate::app::{App, View};
use crate::format;
use crate::sampler::{Snapshot, percent_of};
use crate::theme::{self, CPU, DISK, GPU, INK, INK_2, INK_3, INK_4, MEMORY, NETWORK};
use crate::ui::widgets::{self, ChartStyle, Layer};

pub fn show(app: &mut App, ui: &mut Ui, s: &Snapshot, view: View) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match view {
        View::Cpu => cpu(ui, s),
        View::Memory => memory(ui, s),
        View::Disk => disk(ui, s),
        View::Network => network(ui, s),
        View::Gpu(i) => gpu(app, ui, s, i),
        View::Processes | View::Services | View::Startup | View::Ports => {}
    });
}

/// Título à esquerda e o valor atual, grande e na cor do recurso, à direita: o foco da tela.
fn title(ui: &mut Ui, name: &str, subtitle: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(name).font(theme::semibold(22.0)).color(INK));
            ui.label(RichText::new(subtitle).font(theme::regular(12.5)).color(INK_3));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).font(theme::semibold(30.0)).color(color));
        });
    });
    ui.add_space(14.0);
}

/// Gráfico principal dentro de um cartão, com a escala no canto e a janela de tempo embaixo.
fn big_chart(ui: &mut Ui, s: &Snapshot, layers: &[Layer], max: f32, scale_label: &str, height: f32) {
    widgets::card().show(ui, |ui| {
        let size = vec2(ui.available_width(), height);
        let response = widgets::chart(ui, size, layers, max, ChartStyle::FULL);
        let r = response.rect;
        let p = ui.painter();
        p.text(r.left_top() + vec2(2.0, 0.0), Align2::LEFT_TOP, scale_label, theme::regular(11.0), INK_4);
        p.text(
            pos2(r.left() + 2.0, r.bottom() + 6.0),
            Align2::LEFT_TOP,
            format!("{} min", s.history_window.as_secs() / 60),
            theme::regular(11.0),
            INK_4,
        );
        p.text(pos2(r.right(), r.bottom() + 6.0), Align2::RIGHT_TOP, "agora", theme::regular(11.0), INK_4);
        ui.add_space(16.0);
    });
}

/// Linha de estatísticas: colunas de largura igual, sem caixas em volta.
fn stats_row(ui: &mut Ui, items: &[(&str, String)]) {
    let cols = items.len().max(1);
    ui.columns(cols, |columns| {
        for (col, (label, value)) in columns.iter_mut().zip(items) {
            widgets::stat(col, label, value.clone());
        }
    });
}

fn cpu(ui: &mut Ui, s: &Snapshot) {
    title(ui, "CPU", &s.cpu_brand, &format::percent(s.cpu), CPU);
    big_chart(ui, s, &[Layer::area(&s.history.cpu, CPU)], 100.0, "100%", 220.0);
    ui.add_space(18.0);
    let threads_hint = match s.physical_cores {
        Some(p) => format!("{p} físicos · {} lógicos", s.cores.len()),
        None => format!("{} lógicos", s.cores.len()),
    };
    stats_row(
        ui,
        &[
            ("Utilização", format::percent(s.cpu)),
            ("Velocidade", format::ghz(s.cpu_freq_mhz)),
            ("Núcleos", threads_hint),
            ("Processos", s.processes.len().to_string()),
            ("Tempo ligado", format::duration(s.uptime)),
        ],
    );

    ui.add_space(22.0);
    widgets::section_label(ui, "Por núcleo lógico");
    ui.add_space(8.0);
    let per_row = ((ui.available_width() + 8.0) / 150.0).floor().max(2.0) as usize;
    let cell_w = ((ui.available_width() - 8.0 * (per_row as f32 - 1.0)) / per_row as f32).floor() - 1.0;
    for chunk in s.history.cores.chunks(per_row).enumerate() {
        let (row_idx, series_chunk) = chunk;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (i, series) in series_chunk.iter().enumerate() {
                let core = row_idx * per_row + i;
                // `vertical`: o Frame herdaria o layout horizontal da linha de cartões.
                widgets::card().inner_margin(Margin::same(8)).show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.set_width(cell_w - 18.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{core}")).font(theme::regular(11.0)).color(INK_3));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format::percent(series.last())).font(theme::regular(11.0)).color(INK_2),
                                );
                            });
                        });
                        widgets::chart(
                            ui,
                            vec2(ui.available_width(), 36.0),
                            &[Layer::area(series, CPU)],
                            100.0,
                            ChartStyle::SPARK,
                        );
                    });
                });
            }
        });
        ui.add_space(4.0);
    }
}

fn memory(ui: &mut Ui, s: &Snapshot) {
    let subtitle = format!("{} instalados", format::bytes(s.mem_total));
    title(ui, "Memória", &subtitle, &format::percent(s.mem_percent()), MEMORY);
    big_chart(ui, s, &[Layer::area(&s.history.memory, MEMORY)], 100.0, "100%", 220.0);
    ui.add_space(18.0);

    stats_row(
        ui,
        &[
            ("Em uso", format::bytes(s.mem_used)),
            ("Disponível", format::bytes(s.mem_available)),
            ("Total", format::bytes(s.mem_total)),
            ("Arquivo de paginação", format!("{} de {}", format::bytes(s.swap_used), format::bytes(s.swap_total))),
        ],
    );
    ui.add_space(20.0);
    widgets::section_label(ui, "Composição");
    ui.add_space(8.0);
    widgets::meter(ui, s.mem_percent() / 100.0, MEMORY, 10.0);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        widgets::legend(ui, MEMORY, &format!("Em uso {}", format::bytes(s.mem_used)));
        ui.add_space(12.0);
        widgets::legend(ui, theme::GRAPHITE_3, &format!("Disponível {}", format::bytes(s.mem_available)));
    });

    ui.add_space(22.0);
    widgets::section_label(ui, "Quem mais usa");
    ui.add_space(6.0);
    let mut top: Vec<_> = s.processes.iter().collect();
    top.sort_by_key(|p| std::cmp::Reverse(p.memory));
    top_list(ui, top.iter().take(6).map(|p| (p.display_name(), p.pid, format::bytes(p.memory), percent_of(p.memory, s.mem_total) / 100.0)), MEMORY);
}

fn disk(ui: &mut Ui, s: &Snapshot) {
    let write_color = theme::lerp(DISK, Color32::WHITE, 0.45);
    let max = widgets::nice_max(s.history.disk_read.max().max(s.history.disk_write.max()), 1024.0 * 1024.0);
    title(ui, "Disco", &format!("{} unidades", s.disks.len()), &format::rate(s.disk_read + s.disk_write), DISK);
    big_chart(
        ui,
        s,
        &[Layer::area(&s.history.disk_read, DISK), Layer::line(&s.history.disk_write, write_color)],
        max,
        &format::rate(max as f64),
        200.0,
    );
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        widgets::legend(ui, DISK, &format!("Leitura {}", format::rate(s.disk_read)));
        ui.add_space(12.0);
        widgets::legend(ui, write_color, &format!("Escrita {}", format::rate(s.disk_write)));
    });

    ui.add_space(20.0);
    widgets::section_label(ui, "Unidades");
    ui.add_space(8.0);
    for d in &s.disks {
        widgets::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let used = d.total.saturating_sub(d.available);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&d.mount).font(theme::semibold(14.0)).color(INK));
                let label = if d.label.is_empty() { String::new() } else { format!("{} · ", d.label) };
                ui.label(RichText::new(format!("{label}{} · {}", d.file_system, d.kind)).font(theme::regular(12.0)).color(INK_3));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("L {} · E {}", format::rate(d.read_rate), format::rate(d.write_rate)))
                            .font(theme::regular(12.0))
                            .color(INK_2),
                    );
                });
            });
            ui.add_space(6.0);
            widgets::meter(ui, percent_of(used, d.total) / 100.0, DISK, 8.0);
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} livres de {}", format::bytes(d.available), format::bytes(d.total)))
                    .font(theme::regular(12.0))
                    .color(INK_3),
            );
        });
        ui.add_space(6.0);
    }

    ui.add_space(16.0);
    widgets::section_label(ui, "Quem mais lê e escreve agora");
    ui.add_space(6.0);
    let mut top: Vec<_> = s.processes.iter().filter(|p| p.disk_rate() > 0.0).collect();
    top.sort_by(|a, b| b.disk_rate().total_cmp(&a.disk_rate()));
    let peak = top.first().map(|p| p.disk_rate()).unwrap_or(1.0).max(1.0);
    top_list(ui, top.iter().take(6).map(|p| (p.display_name(), p.pid, format::rate(p.disk_rate()), (p.disk_rate() / peak) as f32)), DISK);
}

fn network(ui: &mut Ui, s: &Snapshot) {
    let tx_color = theme::lerp(NETWORK, Color32::WHITE, 0.45);
    let max = widgets::nice_max(s.history.net_rx.max().max(s.history.net_tx.max()), 64.0 * 1024.0);
    let subtitle = if s.net_adapters.is_empty() { "Sem adaptador ativo".to_owned() } else { s.net_adapters.join(" · ") };
    title(ui, "Rede", &subtitle, &format::rate(s.net_rx + s.net_tx), NETWORK);
    big_chart(
        ui,
        s,
        &[Layer::area(&s.history.net_rx, NETWORK), Layer::line(&s.history.net_tx, tx_color)],
        max,
        &format::rate(max as f64),
        220.0,
    );
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        widgets::legend(ui, NETWORK, &format!("Recebendo {}", format::rate(s.net_rx)));
        ui.add_space(12.0);
        widgets::legend(ui, tx_color, &format!("Enviando {}", format::rate(s.net_tx)));
    });
    ui.add_space(18.0);
    stats_row(
        ui,
        &[
            ("Recebendo", format::rate(s.net_rx)),
            ("Enviando", format::rate(s.net_tx)),
            ("Recebido desde o boot", format::bytes(s.net_rx_total)),
            ("Enviado desde o boot", format::bytes(s.net_tx_total)),
        ],
    );
}

fn gpu(_app: &mut App, ui: &mut Ui, s: &Snapshot, index: usize) {
    let (Some(g), Some(series)) = (s.gpus.get(index), s.history.gpus.get(index)) else {
        ui.label(RichText::new("GPU não encontrada.").color(INK_3));
        return;
    };
    let label = if s.gpus.len() > 1 { format!("GPU {index}") } else { "GPU".to_owned() };
    title(ui, &label, &g.name, &format::percent(g.utilization), GPU);
    big_chart(ui, s, &[Layer::area(series, GPU)], 100.0, "100%", 220.0);
    ui.add_space(18.0);
    stats_row(
        ui,
        &[
            ("Utilização", format::percent(g.utilization)),
            ("Memória dedicada", format!("{} de {}", format::bytes(g.vram_used), format::bytes(g.vram_total))),
        ],
    );
    ui.add_space(10.0);
    widgets::meter(ui, percent_of(g.vram_used, g.vram_total) / 100.0, GPU, 8.0);

    ui.add_space(22.0);
    widgets::section_label(ui, "Quem mais usa");
    ui.add_space(6.0);
    let mut top: Vec<_> = s.processes.iter().filter(|p| p.gpu > 0.05).collect();
    top.sort_by(|a, b| b.gpu.total_cmp(&a.gpu));
    top_list(ui, top.iter().take(6).map(|p| (p.display_name(), p.pid, format::percent(p.gpu), p.gpu / 100.0)), GPU);
}

/// Ranking curto com barra de proporção, usado no fim das telas.
fn top_list<'a>(ui: &mut Ui, items: impl Iterator<Item = (&'a str, u32, String, f32)>, color: Color32) {
    let mut any = false;
    for (name, pid, value, fraction) in items {
        any = true;
        ui.horizontal(|ui| {
            ui.set_height(26.0);
            ui.label(RichText::new(name).font(theme::regular(13.0)).color(INK));
            ui.label(RichText::new(pid.to_string()).font(theme::regular(11.5)).color(INK_4));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_sized(vec2(90.0, 20.0), egui::Label::new(RichText::new(value).font(theme::semibold(12.5)).color(INK_2)));
                let (rect, _) = ui.allocate_exact_size(vec2(120.0, 6.0), egui::Sense::hover());
                let p = ui.painter();
                p.rect_filled(rect, egui::CornerRadius::same(3), theme::WELL);
                let w = rect.width() * fraction.clamp(0.0, 1.0);
                if w > 0.5 {
                    p.rect_filled(
                        egui::Rect::from_min_size(rect.min, vec2(w.max(6.0), rect.height())),
                        egui::CornerRadius::same(3),
                        color,
                    );
                }
            });
        });
    }
    if !any {
        ui.label(RichText::new("Nada relevante agora.").font(theme::regular(12.5)).color(INK_3));
    }
}
