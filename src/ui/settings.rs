//! Configurações do app, num modal aberto pela engrenagem da barra lateral: seções à
//! esquerda, opções da seção à direita.

use egui::{Align2, Color32, CornerRadius, Frame, Id, Margin, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as icon;

use crate::app::{App, HistoryWindow, TABLES};
use crate::format;
use crate::theme::{self, ACCENT, GRAPHITE_1, HAIRLINE_STRONG, INK, INK_2, INK_3, INK_4, R_LG, R_MD, R_SM};
use crate::ui::{columns, ports, widgets};
use crate::updater::{self, State, Version};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    General,
    Processes,
    Ports,
    Alerts,
    Charts,
    Updates,
    About,
}

impl Section {
    const ALL: [Section; 7] = [
        Section::General,
        Section::Processes,
        Section::Ports,
        Section::Alerts,
        Section::Charts,
        Section::Updates,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "Geral",
            Section::Processes => "Processos",
            Section::Ports => "Portas",
            Section::Alerts => "Alertas",
            Section::Charts => "Gráficos",
            Section::Updates => "Atualizações",
            Section::About => "Sobre",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::General => icon::SLIDERS_HORIZONTAL,
            Section::Processes => icon::LIST_BULLETS,
            Section::Ports => icon::PLUGS_CONNECTED,
            Section::Alerts => icon::BELL_SIMPLE,
            Section::Charts => icon::CHART_LINE,
            Section::Updates => icon::ARROW_CIRCLE_UP,
            Section::About => icon::INFO,
        }
    }
}

/// Colunas que dá para esconder (o nome, e a porta na tela de portas, ficam sempre).
const PROCESS_COLUMNS: [(&str, &str); 6] =
    [("pid", "PID"), ("user", "Usuário"), ("cpu", "CPU"), ("memory", "Memória"), ("disk", "Disco"), ("gpu", "GPU")];
const PORT_COLUMNS: [(&str, &str); 6] = [
    ("address", "Endereço"),
    ("pid", "PID"),
    ("response", "Resposta"),
    ("latency", "Conexão"),
    ("memory", "Memória"),
    ("uptime", "Ativo há"),
];

const NAV_WIDTH: f32 = 168.0;

pub fn show(app: &mut App, ctx: &egui::Context) {
    let section_id = Id::new("settings_section");
    let mut section: Section = ctx.data(|d| d.get_temp(section_id)).unwrap_or_default();
    let mut close = false;
    // Cabe na tela de 1366×768 com folga para a barra de título.
    let height = (ctx.content_rect().height() - 120.0).clamp(320.0, 520.0);
    let modal = egui::Modal::new(Id::new("settings"))
        .frame(
            Frame::NONE
                .fill(theme::GRAPHITE_2)
                .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
                .corner_radius(CornerRadius::same(R_LG))
                .inner_margin(Margin::same(18)),
        )
        .backdrop_color(Color32::from_black_alpha(150))
        .show(ctx, |ui| {
            ui.set_width(700.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::GEAR_SIX).font(theme::regular(20.0)).color(INK_2));
                ui.label(RichText::new("Configurações").font(theme::semibold(16.0)).color(INK));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, "Fechar").clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(12.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(NAV_WIDTH);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for s in Section::ALL {
                        if nav_item(ui, s == section, s.icon(), s.label()).clicked() {
                            section = s;
                        }
                    }
                });
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .id_salt(("settings_scroll", section.label()))
                    .max_height(height)
                    .min_scrolled_height(height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // `vertical`: o conteúdo herdaria o layout horizontal da linha.
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width() - 8.0);
                            match section {
                                Section::General => general(app, ui),
                                Section::Processes => processes(app, ui),
                                Section::Ports => ports_section(app, ui),
                                Section::Alerts => alerts_section(app, ui),
                                Section::Charts => charts(app, ui),
                                Section::Updates => updates(app, ui),
                                Section::About => about(app, ui),
                            }
                        });
                    });
            });
        });
    ctx.data_mut(|d| d.insert_temp(section_id, section));
    if close || modal.should_close() {
        app.settings_open = false;
        // A confirmação de redefinir não fica armada para a próxima abertura.
        ctx.data_mut(|d| d.remove_temp::<bool>(Id::new("settings_reset_confirm")));
    }
}

fn nav_item(ui: &mut Ui, selected: bool, glyph: &str, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(NAV_WIDTH, 34.0), Sense::click());
    let p = ui.painter();
    let fill = if selected {
        GRAPHITE_1
    } else if response.hovered() {
        theme::GRAPHITE_3
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(R_MD), fill);
    if selected {
        let bar = Rect::from_min_max(pos2(rect.left(), rect.top() + 10.0), pos2(rect.left() + 3.0, rect.bottom() - 10.0));
        p.rect_filled(bar, CornerRadius::same(2), ACCENT);
    }
    let color = if selected { INK } else { INK_2 };
    p.text(pos2(rect.left() + 14.0, rect.center().y), Align2::LEFT_CENTER, glyph, theme::regular(15.0), color);
    p.text(pos2(rect.left() + 38.0, rect.center().y), Align2::LEFT_CENTER, label, theme::semibold(13.0), color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Título e descrição à esquerda, controle à direita. `control_width` reserva o espaço
/// do controle: sem isso, a descrição quebraria por baixo dele.
fn row(ui: &mut Ui, title: &str, description: &str, control_width: f32, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        // Na passada de medição do modal a largura disponível pode ser zero.
        let text_width = (ui.available_width() - control_width - 12.0).max(60.0);
        ui.vertical(|ui| {
            ui.set_max_width(text_width);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).font(theme::semibold(13.0)).color(INK));
            if !description.is_empty() {
                ui.add(egui::Label::new(RichText::new(description).font(theme::regular(12.0)).color(INK_3)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

fn divider(ui: &mut Ui) {
    ui.add_space(8.0);
    crate::app::hairline(ui);
    ui.add_space(8.0);
}

fn heading(ui: &mut Ui, text: &str) {
    widgets::section_label(ui, text);
    ui.add_space(6.0);
}

/// Botões lado a lado, um selecionado (intervalo, janela de histórico).
fn segmented<T: Copy + PartialEq>(ui: &mut Ui, current: T, options: &[(T, String)]) -> Option<T> {
    let mut chosen = None;
    ui.spacing_mut().item_spacing.x = 2.0;
    // right_to_left: o último da lista fica mais à direita.
    for (value, text) in options.iter().rev() {
        let selected = *value == current;
        let button = egui::Button::selectable(
            selected,
            RichText::new(text).font(theme::regular(12.5)).color(if selected { INK } else { INK_3 }),
        )
        .corner_radius(CornerRadius::same(R_SM))
        .min_size(vec2(52.0, 28.0));
        if ui.add(button).clicked() && !selected {
            chosen = Some(*value);
        }
    }
    chosen
}

fn general(app: &mut App, ui: &mut Ui) {
    heading(ui, "Janela");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        let (autostart, minimized) = app.autostart;
        row(ui, "Iniciar com o Windows", "Abre o Process Monitor quando você entra na sua conta.", 40.0, |ui| {
            let mut on = autostart;
            if widgets::toggle(ui, &mut on).changed() {
                app.set_autostart(on, minimized);
            }
        });
        divider(ui);
        let hint = if app.minimize_to_tray {
            "Na inicialização do Windows, abre só como ícone na bandeja."
        } else {
            "Na inicialização do Windows, abre minimizado na barra de tarefas."
        };
        ui.add_enabled_ui(autostart, |ui| {
            row(ui, "Abrir minimizado", hint, 40.0, |ui| {
                let mut on = minimized;
                if widgets::toggle(ui, &mut on).changed() {
                    app.set_autostart(true, on);
                }
            });
        });
        divider(ui);
        row(
            ui,
            "Minimizar para a bandeja",
            "O X esconde a janela e o app continua no ícone da bandeja. Para fechar de vez, use Sair no menu do ícone.",
            40.0,
            |ui| {
                let mut on = app.minimize_to_tray;
                if widgets::toggle(ui, &mut on).changed() {
                    app.set_minimize_to_tray(on);
                }
            },
        );
        divider(ui);
        row(ui, "Sempre visível", "A janela fica por cima das outras.", 40.0, |ui| {
            let mut on = app.always_on_top;
            if widgets::toggle(ui, &mut on).changed() {
                let ctx = ui.ctx().clone();
                app.set_always_on_top(&ctx, on);
            }
        });
    });

    ui.add_space(16.0);
    heading(ui, "Sistema");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        row(
            ui,
            "Substituir o Gerenciador de Tarefas",
            "Abre o Process Monitor com Ctrl+Shift+Esc ou ao escolher Gerenciador de Tarefas. Pede administrador.",
            40.0,
            |ui| {
                let mut on = app.replaces_taskmgr;
                if widgets::toggle(ui, &mut on).changed() {
                    app.set_replace_task_manager(on);
                }
            },
        );
        // A chave vale para o PC inteiro, mas o exe está na pasta deste usuário: os
        // outros usuários ficariam sem Gerenciador de Tarefas nenhum.
        if crate::platform::installed_per_user() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::WARNING).font(theme::regular(14.0)).color(theme::ACCENT));
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Instalado só para o seu usuário, mas a troca vale para todos do computador: \
                             os outros usuários ficariam sem Gerenciador de Tarefas. Se o PC é compartilhado, \
                             reinstale escolhendo \"para todos os usuários\".",
                        )
                        .font(theme::regular(12.0))
                        .color(INK_2),
                    )
                    .wrap(),
                );
            });
        }
    });

    ui.add_space(16.0);
    heading(ui, "Coleta");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        row(
            ui,
            "Intervalo de atualização",
            "De quanto em quanto tempo os números e gráficos são lidos. Menos tempo, mais CPU do próprio app.",
            170.0,
            |ui| {
                let options = [(500, "0,5 s".to_owned()), (1000, "1 s".to_owned()), (2000, "2 s".to_owned())];
                if let Some(ms) = segmented(ui, app.interval_ms, &options) {
                    app.set_interval(ms);
                }
            },
        );
    });
}

fn column_checks(app: &mut App, ui: &mut Ui, table: &str, columns: &[(&str, &str)]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(18.0, 8.0);
        for (key, label) in columns {
            let mut visible = app.column_visible(table, key);
            if ui.checkbox(&mut visible, RichText::new(*label).font(theme::regular(13.0)).color(INK_2)).changed() {
                app.set_column_visible(table, key, visible);
            }
        }
    });
}

fn reset_widths_row(ui: &mut Ui, table: &str) {
    row(ui, "Larguras das colunas", "Volta as colunas desta tabela para a largura inicial.", 90.0, |ui| {
        if widgets::ghost_button(ui, "Restaurar").clicked() {
            columns::reset_widths(ui.ctx(), table);
        }
    });
}

fn processes(app: &mut App, ui: &mut Ui) {
    heading(ui, "Colunas visíveis");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        column_checks(app, ui, TABLES[0], &PROCESS_COLUMNS);
        ui.add_space(6.0);
        ui.label(
            RichText::new("O nome aparece sempre. Em janelas estreitas, Usuário e GPU somem sozinhos.")
                .font(theme::regular(12.0))
                .color(INK_4),
        );
        divider(ui);
        reset_widths_row(ui, TABLES[0]);
    });

    ui.add_space(16.0);
    heading(ui, "Regras por programa");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        if app.rules.is_empty() {
            ui.label(
                RichText::new(
                    "Nenhuma regra. Ajuste a prioridade, os núcleos ou o modo eficiência de um processo e use \"Lembrar para sempre que abrir\" no painel de detalhes.",
                )
                .font(theme::regular(12.0))
                .color(INK_3),
            );
            return;
        }
        ui.label(
            RichText::new("Aplicadas toda vez que o programa abre, também com a janela na bandeja.")
                .font(theme::regular(12.0))
                .color(INK_3),
        );
        ui.add_space(6.0);
        let mut forget: Option<String> = None;
        for rule in &app.rules {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&rule.exe).font(theme::semibold(13.0)).color(INK));
                ui.label(RichText::new(rule.summary()).font(theme::regular(12.5)).color(INK_2));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, "Esquecer a regra").clicked() {
                        forget = Some(rule.exe.clone());
                    }
                });
            });
        }
        if let Some(exe) = forget {
            app.forget_rule(&exe);
        }
    });
}

/// Avisos na bandeja. Valem com a janela escondida: quem avalia é a thread da coleta.
fn alerts_section(app: &mut App, ui: &mut Ui) {
    let mut config = app.alert_config.clone();

    heading(ui, "Portas vigiadas");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        row(ui, "Avisar quando cair ou voltar", "Depois de 3 s sem escutar, e de novo quando a porta volta.", 40.0, |ui| {
            widgets::toggle(ui, &mut config.ports_enabled);
        });
        divider(ui);
        if config.watched_ports.is_empty() {
            ui.label(
                RichText::new("Nenhuma porta vigiada. Use \"Vigiar porta\" no menu de uma porta, ou adicione aqui.")
                    .font(theme::regular(12.0))
                    .color(INK_3),
            );
        } else {
            let mut remove: Option<u16> = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for &port in &config.watched_ports {
                    let chip = egui::Button::new(
                        RichText::new(format!("{port}  {}", icon::X)).font(theme::regular(12.5)).color(INK_2),
                    )
                    .fill(GRAPHITE_1)
                    .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
                    .corner_radius(CornerRadius::same(R_SM));
                    if ui.add(chip).on_hover_text("Parar de vigiar").clicked() {
                        remove = Some(port);
                    }
                }
            });
            if let Some(port) = remove {
                config.watched_ports.retain(|p| *p != port);
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let id = Id::new("alert_port_input");
            let mut text: String = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
            let edit = egui::TextEdit::singleline(&mut text).hint_text("Porta, ex.: 5173").desired_width(120.0);
            let response = ui.add(edit);
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let port = text.trim().parse::<u16>().ok().filter(|p| *p > 0);
            if (ui.add_enabled(port.is_some(), egui::Button::new("Adicionar")).clicked() || enter)
                && let Some(port) = port
            {
                if !config.watched_ports.contains(&port) {
                    config.watched_ports.push(port);
                    config.watched_ports.sort_unstable();
                }
                text.clear();
            }
            ui.data_mut(|d| d.insert_temp(id, text));
        });
    });

    ui.add_space(16.0);
    heading(ui, "Processos");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        row(ui, "CPU alta", "Um processo acima do limite pelo tempo escolhido. Avisa uma vez até ele baixar.", 40.0, |ui| {
            widgets::toggle(ui, &mut config.cpu_enabled);
        });
        ui.add_enabled_ui(config.cpu_enabled, |ui| {
            row(ui, "Limite", "Do total da máquina, como a coluna CPU.", 230.0, |ui| {
                let options: Vec<(u32, String)> = [25, 50, 75, 90].iter().map(|p| (*p, format!("{p}%"))).collect();
                if let Some(p) = segmented(ui, config.cpu_percent.round() as u32, &options) {
                    config.cpu_percent = p as f32;
                }
            });
            row(ui, "Por quanto tempo", "Picos mais curtos que isso não avisam.", 230.0, |ui| {
                let options =
                    [(10, "10 s".to_owned()), (30, "30 s".to_owned()), (60, "1 min".to_owned()), (300, "5 min".to_owned())];
                if let Some(s) = segmented(ui, config.cpu_secs, &options) {
                    config.cpu_secs = s;
                }
            });
        });
        divider(ui);
        row(ui, "Memória alta", "Um processo com mais memória física que o limite.", 40.0, |ui| {
            widgets::toggle(ui, &mut config.memory_enabled);
        });
        ui.add_enabled_ui(config.memory_enabled, |ui| {
            row(ui, "Limite", "Memória física do processo.", 270.0, |ui| {
                const GB: u64 = 1024 * 1024 * 1024;
                let options: Vec<(u64, String)> = [1, 2, 4, 8, 16].iter().map(|g| (g * GB, format!("{g} GB"))).collect();
                if let Some(b) = segmented(ui, config.memory_bytes, &options) {
                    config.memory_bytes = b;
                }
            });
        });
        divider(ui);
        row(
            ui,
            "Travado ou reiniciando",
            "Janela que não responde há 10 s, ou programa que fecha com erro e volta em loop.",
            40.0,
            |ui| {
                widgets::toggle(ui, &mut config.problems_enabled);
            },
        );
    });

    ui.add_space(16.0);
    heading(ui, "Disco");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        row(ui, "Disco quase cheio", "Uma unidade com pouco espaço livre. Avisa uma vez até liberar espaço.", 40.0, |ui| {
            widgets::toggle(ui, &mut config.disk_enabled);
        });
        ui.add_enabled_ui(config.disk_enabled, |ui| {
            row(ui, "Limite", "Espaço livre abaixo disso.", 230.0, |ui| {
                let options: Vec<(u32, String)> = [5, 10, 15, 20].iter().map(|p| (*p, format!("{p}%"))).collect();
                if let Some(p) = segmented(ui, config.disk_free_percent.round() as u32, &options) {
                    config.disk_free_percent = p as f32;
                }
            });
        });
    });

    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if widgets::ghost_button(ui, &format!("{}  Testar aviso", icon::BELL_SIMPLE_RINGING)).clicked() {
            app.tray.notifier().notify("Teste de alerta", "Os avisos do Process Monitor aparecem assim.");
        }
        ui.label(
            RichText::new("Clicar num aviso abre o processo ou a porta dele.").font(theme::regular(12.0)).color(INK_4),
        );
    });

    app.set_alert_config(config);
}

fn ports_section(app: &mut App, ui: &mut Ui) {
    heading(ui, "Lista");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Faixas de portas monitoradas").font(theme::semibold(13.0)).color(INK));
        ui.label(
            RichText::new("As portas das faixas aparecem sempre na lista, mesmo de programas que não são de desenvolvimento.")
                .font(theme::regular(12.0))
                .color(INK_3),
        );
        ui.add_space(6.0);
        ports::ranges_field(app, ui);
        divider(ui);
        row(
            ui,
            "Mostrar todas as portas",
            "Inclui serviços do Windows e outros programas, além dos servidores de desenvolvimento e das faixas.",
            40.0,
            |ui| {
                widgets::toggle(ui, &mut app.ports_all);
            },
        );
        divider(ui);
        row(
            ui,
            "Sondar HTTP",
            "Abre uma conexão com cada servidor a cada 2 s e faz um GET a cada 30 s para mostrar se ele responde. \
             Desligado, nenhuma requisição é feita e as colunas Resposta e Conexão somem.",
            40.0,
            |ui| {
                let mut on = app.probe_http;
                if widgets::toggle(ui, &mut on).changed() {
                    app.set_probe_http(on);
                }
            },
        );
    });

    ui.add_space(16.0);
    heading(ui, "Colunas visíveis");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        column_checks(app, ui, TABLES[1], &PORT_COLUMNS);
        ui.add_space(6.0);
        ui.label(
            RichText::new("Porta e projeto aparecem sempre. Em janelas estreitas, Endereço, Conexão e Ativo há somem sozinhos.")
                .font(theme::regular(12.0))
                .color(INK_4),
        );
        divider(ui);
        reset_widths_row(ui, TABLES[1]);
    });
}

fn charts(app: &mut App, ui: &mut Ui) {
    heading(ui, "Desempenho");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        row(
            ui,
            "Janela de histórico",
            "Quanto tempo os gráficos de CPU, memória, disco, rede e GPU mostram. Os cartões da barra lateral \
             ficam sempre no último minuto. Ao aumentar, o gráfico enche aos poucos.",
            190.0,
            |ui| {
                let options: Vec<(HistoryWindow, String)> = HistoryWindow::ALL.iter().map(|w| (*w, w.label())).collect();
                if let Some(w) = segmented(ui, app.history_window, &options) {
                    app.set_history_window(w);
                }
            },
        );
    });
}

fn updates(app: &mut App, ui: &mut Ui) {
    heading(ui, "Atualizações");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        row(
            ui,
            "Baixar atualizações automaticamente",
            "Confere o GitHub ao abrir e a cada 6 horas. A instalação só acontece quando você mandar.",
            40.0,
            |ui| {
                let mut on = app.auto_update;
                if widgets::toggle(ui, &mut on).changed() {
                    app.set_auto_update(on);
                }
            },
        );
        divider(ui);

        let state = app.update_state.clone();
        let mut install = false;
        let (text, color) = status_text(&state);
        let title = format!("Versão {}", Version::current());
        ui.horizontal(|ui| {
            let text_width = (ui.available_width() - 170.0).max(60.0);
            ui.vertical(|ui| {
                ui.set_max_width(text_width);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(title).font(theme::semibold(13.0)).color(INK));
                ui.add(egui::Label::new(RichText::new(text).font(theme::regular(12.0)).color(color)).wrap());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match &*state {
                State::Ready { .. } => install = widgets::accent_button(ui, "Reiniciar e atualizar").clicked(),
                State::Checking | State::Downloading { .. } => {
                    ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                }
                _ => {
                    if widgets::ghost_button(ui, "Verificar agora").clicked() {
                        app.updater.check_now();
                    }
                }
            });
        });
        if let State::Downloading { received, total: Some(total), .. } = &*state {
            ui.add_space(8.0);
            widgets::meter(ui, *received as f32 / (*total).max(1) as f32, ACCENT, 4.0);
        }
        if install {
            let ctx = ui.ctx().clone();
            app.install_update(&ctx);
        }
    });
}

fn status_text(state: &State) -> (String, Color32) {
    match state {
        State::Idle => ("Ainda não verificado nesta sessão".to_owned(), INK_3),
        State::Checking => ("Verificando…".to_owned(), INK_2),
        State::UpToDate { at } => (format!("Você está na versão mais recente · verificado às {}", at.format("%H:%M")), INK_3),
        State::Downloading { version, received, total } => {
            let progress = match total {
                Some(t) => format!("{} de {}", format::bytes(*received), format::bytes(*t)),
                None => format::bytes(*received),
            };
            (format!("Baixando a versão {version}… {progress}"), INK_2)
        }
        State::Ready { version, .. } => (format!("Versão {version} baixada e pronta para instalar"), ACCENT),
        State::Failed(e) => (format!("A atualização falhou: {e}"), theme::DANGER),
    }
}

fn about(app: &mut App, ui: &mut Ui) {
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            crate::ui::sidebar::logo_mark(ui, rect);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new("Process Monitor").font(theme::semibold(16.0)).color(INK));
                ui.label(
                    RichText::new(format!("Versão {} · Monitor de sistema para Windows", Version::current()))
                        .font(theme::regular(12.5))
                        .color(INK_3),
                );
            });
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let repo = format!("https://github.com/{}", updater::REPO);
            if link(ui, icon::GITHUB_LOGO, "Código no GitHub").clicked() {
                crate::platform::shell_open(&repo);
            }
            ui.add_space(12.0);
            if link(ui, icon::NOTE, "Notas desta versão").clicked() {
                crate::platform::shell_open(&format!("{repo}/releases/tag/v{}", Version::current()));
            }
        });
    });

    ui.add_space(16.0);
    heading(ui, "Dados");
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        let data_dir = eframe::storage_dir(crate::platform::APP_NAME);
        let path = data_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
        row(ui, "Pasta de dados", &path, 90.0, |ui| {
            if ui.add_enabled(data_dir.is_some(), egui::Button::new("Abrir")).clicked()
                && let Some(dir) = &data_dir
            {
                let _ = std::fs::create_dir_all(dir);
                crate::platform::shell_open(&dir.display().to_string());
            }
        });
        divider(ui);

        let confirm_id = Id::new("settings_reset_confirm");
        let confirming: bool = ui.data(|d| d.get_temp(confirm_id)).unwrap_or(false);
        if confirming {
            ui.label(RichText::new("Redefinir todas as configurações?").font(theme::semibold(13.0)).color(INK));
            ui.label(
                RichText::new(
                    "Volta tudo ao padrão: janela, colunas, larguras, portas, alertas, gráficos e atualização. \
                     A inicialização com o Windows é desligada.",
                )
                .font(theme::regular(12.0))
                .color(INK_3),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if widgets::danger_button(ui, "Redefinir", true).clicked() {
                    let ctx = ui.ctx().clone();
                    app.reset_settings(&ctx);
                    ui.data_mut(|d| d.insert_temp(confirm_id, false));
                }
                if widgets::ghost_button(ui, "Cancelar").clicked() {
                    ui.data_mut(|d| d.insert_temp(confirm_id, false));
                }
            });
        } else {
            row(ui, "Redefinir configurações", "Volta todas as opções ao padrão.", 100.0, |ui| {
                if widgets::ghost_button(ui, "Redefinir…").clicked() {
                    ui.data_mut(|d| d.insert_temp(confirm_id, true));
                }
            });
        }
    });
}

/// Texto clicável em laranja, com ícone.
fn link(ui: &mut Ui, glyph: &str, text: &str) -> egui::Response {
    let response = ui.add(
        egui::Label::new(RichText::new(format!("{glyph}  {text}")).font(theme::regular(13.0)).color(ACCENT))
            .sense(Sense::click()),
    );
    if response.hovered() {
        let r = response.rect;
        ui.painter().hline(r.x_range(), r.bottom(), Stroke::new(1.0, ACCENT.gamma_multiply(0.6)));
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
