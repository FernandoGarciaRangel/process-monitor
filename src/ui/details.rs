//! Painel de detalhes do processo selecionado.

use egui::{Align, CornerRadius, Frame, Layout, Margin, RichText, Stroke, Ui, vec2};
use egui_phosphor::regular as icon;

use crate::app::{self, App};
use crate::format;
use crate::sampler::ProcKey;
use crate::theme::{self, CPU, HAIRLINE, INK, INK_2, INK_3, MEMORY, R_SM, WELL};
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
            live_charts(app, ui, p.cpu, p.memory);
            ui.add_space(18.0);

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
                        let link = egui::Button::new(RichText::new(format!("{name} · {}", pk.pid)).font(theme::regular(12.5)).color(CPU))
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

/// CPU e memória do processo nos últimos segundos, lado a lado.
fn live_charts(app: &App, ui: &mut Ui, cpu: f32, memory: u64) {
    let h = &app.proc_history;
    let gap = 10.0;
    // Arredondado para baixo: meio pixel a mais por cartão, somado quadro a quadro,
    // fazia o painel crescer sozinho até o limite.
    let width = ((ui.available_width() - gap) / 2.0).floor() - 1.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (label, value, layer, max, color) in [
            ("CPU", format::percent(cpu), Layer::area(&h.cpu, CPU), widgets::nice_max(h.cpu.max(), 5.0).min(100.0), CPU),
            (
                "Memória",
                format::bytes(memory),
                Layer::area(&h.memory, MEMORY),
                h.memory.max() * 1.25,
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
