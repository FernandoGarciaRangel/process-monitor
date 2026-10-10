//! Painel de detalhes do processo selecionado.

use egui::{Align, CornerRadius, Frame, Layout, Margin, RichText, Stroke, Ui, vec2};
use egui_phosphor::regular as icon;

use crate::app::{self, App, HistorySpan, View};
use crate::format;
use crate::ports::{Connection, Listener, Reply, TcpState};
use crate::sampler::{ProcKey, Series, Snapshot};
use crate::theme::{self, ACCENT, CPU, HAIRLINE, INK, INK_2, INK_3, MEMORY, R_SM, WELL};
use crate::ui::widgets::{self, ChartStyle, Layer};

pub fn show(app: &mut App, ui: &mut Ui) {
    // O conteúdo é desenhado num filho que não repassa o próprio tamanho ao painel,
    // e o painel ocupa exatamente o espaço que recebeu. Sem isso, qualquer widget mais
    // largo que o painel (um nome longo, um botão) o alargava até o máximo, e a parte
    // de fora ficava cortada pela borda da janela.
    let rect = ui.max_rect();
    let mut content = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
    content.set_clip_rect(rect.intersect(ui.clip_rect()));
    body(app, &mut content);
    ui.allocate_rect(rect, egui::Sense::hover());
}

fn body(app: &mut App, ui: &mut Ui) {
    let (Some(snap), Some(key)) = (app.snap.clone(), app.selected) else { return };
    let Some(p) = snap.find(key) else { return };
    let protected = app.is_protected(key, &p.name);

    // Linha que separa o painel do conteúdo central.
    let rect = ui.max_rect();
    ui.painter().vline(rect.left(), rect.y_range(), Stroke::new(1.0, HAIRLINE));

    // Ações fixas no rodapé; o resto rola.
    egui::Panel::bottom("details_actions")
        .frame(Frame::NONE.inner_margin(Margin { left: 20, right: 20, top: 12, bottom: 16 }))
        .show(ui, |ui| {
            app::hairline(ui);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let kill = widgets::danger_button(ui, &format!("{}  Finalizar processo", icon::X_CIRCLE), !protected);
                let kill = if protected {
                    kill.on_disabled_hover_text("Processo crítico do sistema: finalizar derrubaria o Windows.")
                } else {
                    kill
                };
                if kill.clicked() {
                    app.request_kill(key, false);
                }
                let tree = ui.add_enabled(
                    !protected,
                    egui::Button::new(RichText::new(format!("{}  Árvore", icon::TREE_STRUCTURE)).color(INK_2))
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0, theme::HAIRLINE_STRONG))
                        .corner_radius(CornerRadius::same(R_SM))
                        .min_size(vec2(0.0, 32.0)),
                );
                if tree.on_hover_text("Finalizar o processo e todos os que ele abriu").clicked() {
                    app.request_kill(key, true);
                }
            });
        });

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        Frame::NONE.inner_margin(Margin { left: 20, right: 18, top: 18, bottom: 12 }).show(ui, |ui| {
            header(app, ui, p.display_name(), &p.name, p.pid, p.exe.as_deref(), p.user.as_deref());
            ui.add_space(16.0);
            live_charts(app, ui, key, p.cpu, p.memory);
            ui.add_space(18.0);

            let listening: Vec<&Listener> = snap.listeners.iter().filter(|l| l.pid == p.pid).collect();
            if !listening.is_empty() {
                ports_section(app, ui, key, &listening);
                ui.add_space(18.0);
            }

            let mut conns: Vec<&Connection> = snap.connections.iter().filter(|c| c.pid == p.pid).collect();
            if !conns.is_empty() {
                // Estabelecidas primeiro; dentro do estado, pelo endereço remoto.
                conns.sort_by_key(|c| (c.state != TcpState::Established, c.remote));
                connections_section(app, ui, &snap, &conns);
                ui.add_space(18.0);
            }

            widgets::section_label(ui, "Arquivo");
            ui.add_space(6.0);
            path_row(ui, "Caminho", p.exe.as_ref().map(|e| e.display().to_string()), p.exe.as_deref());
            ui.add_space(8.0);
            command_line(ui, &p.cmd);
            ui.add_space(8.0);
            path_row(ui, "Diretório de trabalho", p.cwd.as_ref().map(|c| c.display().to_string()), None);
            ui.add_space(18.0);

            widgets::section_label(ui, "Execução");
            ui.add_space(6.0);
            let parent = snap.parent_of(p).map(|pp| (pp.key(), pp.display_name().to_owned()));
            egui::Grid::new("exec").num_columns(2).spacing(vec2(16.0, 8.0)).min_col_width(110.0).show(ui, |ui| {
                ui.label(RichText::new("Processo pai").font(theme::regular(12.0)).color(INK_3));
                match &parent {
                    Some((pk, name)) => {
                        // Botão sem moldura em vez de `link`, que não corta nomes longos.
                        let link = egui::Button::new(RichText::new(format!("{name} · {}", pk.pid)).font(theme::regular(12.5)).color(ACCENT))
                            .frame(false)
                            .truncate();
                        if ui.add(link).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            app.select(Some(*pk));
                            app.scroll_to_selected = true;
                        }
                    }
                    None => {
                        let text = p.parent.map(|pid| format!("PID {pid} (encerrado)")).unwrap_or_else(|| "—".to_owned());
                        ui.label(RichText::new(text).font(theme::regular(12.5)).color(INK_2));
                    }
                }
                ui.end_row();
                widgets::field(ui, "Iniciado em", &format::datetime(p.start_time));
                widgets::field(ui, "Em execução há", &format::duration(p.run_time));
                widgets::field(ui, "Usuário", p.user.as_deref().unwrap_or("— (sem acesso)"));
            });
            ui.add_space(18.0);

            widgets::section_label(ui, "Recursos");
            ui.add_space(6.0);
            egui::Grid::new("resources").num_columns(2).spacing(vec2(16.0, 8.0)).min_col_width(110.0).show(ui, |ui| {
                widgets::field(ui, "CPU", &format::percent(p.cpu));
                widgets::field(ui, "Memória física", &format::bytes(p.memory));
                widgets::field(ui, "Memória virtual", &format::bytes(p.virtual_memory));
                widgets::field(ui, "Disco agora", &format!("L {} · E {}", format::rate(p.disk_read), format::rate(p.disk_write)));
                widgets::field(
                    ui,
                    "Disco acumulado",
                    &format!("L {} · E {}", format::bytes(p.disk_total_read), format::bytes(p.disk_total_written)),
                );
                if !snap.gpus.is_empty() {
                    widgets::field(ui, "GPU", &format::percent(p.gpu));
                }
            });

            let children = snap.children_of(p);
            if !children.is_empty() {
                ui.add_space(18.0);
                widgets::section_label(ui, &format!("Processos filhos · {}", children.len()));
                ui.add_space(4.0);
                let mut chosen: Option<ProcKey> = None;
                for child in children.iter().take(40) {
                    let text = RichText::new(format!("{}  {} · {}", icon::ARROW_ELBOW_DOWN_RIGHT, child.display_name(), child.pid))
                        .font(theme::regular(12.5))
                        .color(INK_2);
                    if ui.add(egui::Button::new(text).frame_when_inactive(false).truncate()).clicked() {
                        chosen = Some(child.key());
                    }
                }
                if children.len() > 40 {
                    ui.label(RichText::new(format!("e mais {}", children.len() - 40)).color(INK_3).font(theme::regular(12.0)));
                }
                if chosen.is_some() {
                    app.select(chosen);
                    app.scroll_to_selected = true;
                }
            }
        });
    });
}

fn header(
    app: &mut App,
    ui: &mut Ui,
    title: &str,
    exe_name: &str,
    pid: u32,
    exe: Option<&std::path::Path>,
    user: Option<&str>,
) {
    ui.horizontal(|ui| {
        let texture = app.icons.get(exe);
        widgets::process_icon(ui, texture.as_ref(), 32.0);
        ui.add_space(6.0);
        // O × entra primeiro, pela direita, e o título fica com o que sobra. Na ordem
        // inversa, um título cortado com reticências ocupava a linha inteira e o ×
        // era empurrado para fora do painel.
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            if widgets::icon_button(ui, icon::X, "Fechar detalhes (Esc)").clicked() {
                app.select(None);
            }
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(egui::Label::new(RichText::new(title).font(theme::semibold(17.0)).color(INK)).truncate());
                let meta = match user {
                    Some(u) => format!("{exe_name} · PID {pid} · {u}"),
                    None => format!("{exe_name} · PID {pid}"),
                };
                ui.add(egui::Label::new(RichText::new(meta).font(theme::regular(12.0)).color(INK_3)).truncate());
            });
        });
    });
}

/// CPU e memória do processo no período escolhido, lado a lado, com a tendência da memória.
fn live_charts(app: &mut App, ui: &mut Ui, key: ProcKey, cpu: f32, memory: u64) {
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Histórico");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for span in [HistorySpan::TenMinutes, HistorySpan::Minute] {
                let on = app.history_span == span;
                let button = egui::Button::selectable(
                    on,
                    RichText::new(span.label()).font(theme::regular(11.5)).color(if on { INK } else { INK_3 }),
                )
                .corner_radius(CornerRadius::same(R_SM));
                if ui.add(button).clicked() {
                    app.history_span = span;
                }
            }
        });
    });
    ui.add_space(6.0);

    let points = app.history_span.points(app.interval_ms);
    let (cpu_series, mem_series) = match app.histories.get(&key) {
        Some(h) => (h.cpu.tail(points), h.memory.tail(points)),
        None => (Series::with_capacity(points), Series::with_capacity(points)),
    };
    let gap = 10.0;
    // Arredondado para baixo: meio pixel a mais por cartão, somado quadro a quadro,
    // fazia o painel crescer sozinho até o limite.
    let width = ((ui.available_width() - gap) / 2.0).floor() - 1.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (label, value, layer, max, color) in [
            ("CPU", format::percent(cpu), Layer::area(&cpu_series, CPU), widgets::nice_max(cpu_series.max(), 5.0).min(100.0), CPU),
            (
                "Memória",
                format::bytes(memory),
                Layer::area(&mem_series, MEMORY),
                mem_series.max() * 1.25,
                MEMORY,
            ),
        ] {
            // O Frame herda o layout do pai, que aqui é horizontal: sem o `vertical`, o
            // gráfico ia para o lado do rótulo e o cartão saía mais largo que o painel.
            widgets::card().inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(width - 22.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).font(theme::regular(11.5)).color(INK_3));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(value).font(theme::semibold(13.0)).color(color));
                        });
                    });
                    ui.add_space(4.0);
                    widgets::chart(ui, vec2(ui.available_width(), 44.0), &[layer], max, ChartStyle::SPARK);
                });
            });
        }
    });

    // Quanto a memória andou no período: é o que denuncia um vazamento.
    let samples = mem_series.values().len();
    if samples >= 2 {
        let secs = (samples as u64 - 1) * app.interval_ms / 1000;
        let delta = mem_series.last() as i64 - mem_series.first() as i64;
        let text = memory_trend(delta, secs);
        ui.add_space(6.0);
        ui.label(RichText::new(text).font(theme::regular(12.0)).color(INK_3));
    }
}

/// "Memória +120 MB em 10 min", "Memória estável em 45 s".
fn memory_trend(delta: i64, secs: u64) -> String {
    let period = if secs >= 60 { format!("{} min", (secs + 30) / 60) } else { format!("{secs} s") };
    // Menos de 1 MB é ruído do coletor de lixo, não tendência.
    if delta.unsigned_abs() < 1024 * 1024 {
        format!("Memória estável em {period}")
    } else {
        let sign = if delta > 0 { "+" } else { "−" };
        format!("Memória {sign}{} em {period}", format::bytes(delta.unsigned_abs()))
    }
}

/// Portas em que o processo escuta, com a última resposta HTTP. O número leva à tela de portas.
fn ports_section(app: &mut App, ui: &mut Ui, key: ProcKey, listening: &[&Listener]) {
    const MAX: usize = 12;
    widgets::section_label(ui, &format!("Portas em escuta · {}", listening.len()));
    ui.add_space(4.0);
    for l in listening.iter().take(MAX) {
        let reply = app.port_status.get(&(l.port, l.pid)).and_then(|s| s.reply);
        ui.horizontal(|ui| {
            let number = egui::Button::new(RichText::new(l.port.to_string()).font(theme::semibold(13.0)).color(ACCENT)).frame(false);
            if ui.add(number).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Ver na tela de portas").clicked() {
                app.view = View::Ports;
                app.select_port(key, l.port);
                app.scroll_to_port = true;
            }
            // Os botões entram primeiro, pela direita; o texto fica com o que sobra e corta.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if widgets::icon_button(ui, icon::COPY, "Copiar URL").clicked() {
                    ui.ctx().copy_text(l.url());
                }
                if matches!(reply, Some(Reply::Http { .. }))
                    && widgets::icon_button(ui, icon::ARROW_SQUARE_OUT, "Abrir no navegador").clicked()
                {
                    crate::platform::shell_open(&l.url());
                }
                let (reply_text, reply_color) = crate::ui::ports::reply_text(reply);
                let mut job = egui::text::LayoutJob::default();
                job.append(&l.addrs_text(), 0.0, egui::TextFormat::simple(theme::regular(12.0), INK_3));
                if reply.is_some() {
                    job.append(&reply_text, 8.0, egui::TextFormat::simple(theme::regular(12.0), reply_color));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(job).truncate());
                });
            });
        });
    }
    if listening.len() > MAX {
        ui.label(RichText::new(format!("e mais {}", listening.len() - MAX)).color(INK_3).font(theme::regular(12.0)));
    }
}

/// Conexões TCP do processo. Uma conexão local para uma porta em escuta mostra o
/// processo do outro lado ("→ uvicorn"), que leva a ele com um clique.
fn connections_section(app: &mut App, ui: &mut Ui, snap: &Snapshot, conns: &[&Connection]) {
    const MAX: usize = 12;
    widgets::section_label(ui, &format!("Conexões · {}", conns.len()));
    ui.add_space(4.0);
    let mut chosen: Option<ProcKey> = None;
    for c in conns.iter().take(MAX) {
        let peer = c
            .remote
            .ip()
            .is_loopback()
            .then(|| snap.listeners.iter().find(|l| l.port == c.remote.port()))
            .flatten()
            .and_then(|l| snap.processes.iter().find(|pp| pp.pid == l.pid));
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if widgets::icon_button(ui, icon::COPY, "Copiar endereço remoto").clicked() {
                    ui.ctx().copy_text(c.remote.to_string());
                }
                let state_color = if c.state == TcpState::Established { INK_3 } else { theme::INK_4 };
                ui.label(RichText::new(c.state.label()).font(theme::regular(11.5)).color(state_color));
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(c.remote.to_string()).font(theme::mono(12.0)).color(INK_2)).truncate());
                    if let Some(pp) = peer {
                        let link = egui::Button::new(
                            RichText::new(format!("→ {}", pp.display_name())).font(theme::regular(12.0)).color(ACCENT),
                        )
                        .frame(false)
                        .truncate();
                        if ui.add(link).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Ver o processo").clicked() {
                            chosen = Some(pp.key());
                        }
                    }
                });
            });
        });
    }
    if conns.len() > MAX {
        ui.label(RichText::new(format!("e mais {}", conns.len() - MAX)).color(INK_3).font(theme::regular(12.0)));
    }
    if chosen.is_some() {
        app.select(chosen);
        app.scroll_to_selected = true;
    }
}

fn path_row(ui: &mut Ui, label: &str, value: Option<String>, explore: Option<&std::path::Path>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).font(theme::regular(12.0)).color(INK_3));
        if let Some(v) = &value {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if let Some(path) = explore
                    && widgets::icon_button(ui, icon::FOLDER_OPEN, "Abrir local do arquivo").clicked()
                {
                    crate::platform::open_in_explorer(path);
                }
                if widgets::icon_button(ui, icon::COPY, "Copiar").clicked() {
                    ui.ctx().copy_text(v.clone());
                }
            });
        }
    });
    let text = value.unwrap_or_else(|| "— (sem acesso; execute como administrador)".to_owned());
    widgets::long_text(ui, &text, theme::regular(12.5), INK);
}

fn command_line(ui: &mut Ui, cmd: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Linha de comando").font(theme::regular(12.0)).color(INK_3));
        if !cmd.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, icon::COPY, "Copiar").clicked() {
                    ui.ctx().copy_text(cmd.to_owned());
                }
            });
        }
    });
    if cmd.is_empty() {
        ui.label(RichText::new("— (sem acesso)").font(theme::regular(12.5)).color(INK_2));
        return;
    }
    Frame::NONE
        .fill(WELL)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(R_SM))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::long_text(ui, cmd, theme::mono(12.0), INK_2);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tendencia_da_memoria() {
        const MB: i64 = 1024 * 1024;
        assert_eq!(memory_trend(120 * MB, 600), "Memória +120 MB em 10 min");
        assert_eq!(memory_trend(-3 * MB, 45), "Memória −3,00 MB em 45 s");
        assert_eq!(memory_trend(200 * 1024, 590), "Memória estável em 10 min");
    }

    #[test]
    fn periodo_em_pontos() {
        assert_eq!(HistorySpan::Minute.points(1000), 60);
        assert_eq!(HistorySpan::TenMinutes.points(1000), 600);
        assert_eq!(HistorySpan::Minute.points(500), 120);
        assert_eq!(HistorySpan::TenMinutes.points(500), app::PROC_HISTORY_LEN);
        assert_eq!(HistorySpan::TenMinutes.points(2000), 300);
    }
}
