//! Configurações do app, num modal aberto pela engrenagem da barra lateral.

use egui::{Color32, CornerRadius, Frame, Id, Margin, RichText, Stroke, Ui};
use egui_phosphor::regular as icon;

use crate::app::App;
use crate::format;
use crate::theme::{self, ACCENT, HAIRLINE_STRONG, INK, INK_2, INK_3, R_LG};
use crate::ui::widgets;
use crate::updater::{State, Version};

pub fn show(app: &mut App, ctx: &egui::Context) {
    let mut close = false;
    let modal = egui::Modal::new(Id::new("settings"))
        .frame(
            Frame::NONE
                .fill(theme::GRAPHITE_2)
                .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
                .corner_radius(CornerRadius::same(R_LG))
                .inner_margin(Margin::same(22)),
        )
        .backdrop_color(Color32::from_black_alpha(150))
        .show(ctx, |ui| {
            ui.set_width(440.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::GEAR_SIX).font(theme::regular(20.0)).color(INK_2));
                ui.label(RichText::new("Configurações").font(theme::semibold(16.0)).color(INK));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, "Fechar").clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(14.0);
            updates(app, ui);
        });
    if close || modal.should_close() {
        app.settings_open = false;
    }
}

fn updates(app: &mut App, ui: &mut Ui) {
    widgets::section_label(ui, "Atualizações");
    ui.add_space(6.0);
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            // Reserva o espaço do interruptor: sem isso, o texto quebra por baixo dele.
            let text_width = ui.available_width() - 56.0;
            ui.vertical(|ui| {
                ui.set_max_width(text_width);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new("Baixar atualizações automaticamente").font(theme::semibold(13.0)).color(INK));
                ui.label(
                    RichText::new("Confere o GitHub ao abrir e a cada 6 horas. A instalação só acontece quando você mandar.")
                        .font(theme::regular(12.0))
                        .color(INK_3),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut on = app.auto_update;
                if widgets::toggle(ui, &mut on).changed() {
                    app.set_auto_update(on);
                }
            });
        });

        ui.add_space(8.0);
        crate::app::hairline(ui);
        ui.add_space(8.0);

        let state = app.update_state.clone();
        let mut install = false;
        ui.horizontal(|ui| {
            let text_width = ui.available_width() - 170.0;
            ui.vertical(|ui| {
                ui.set_max_width(text_width);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(format!("Versão {}", Version::current())).font(theme::semibold(13.0)).color(INK));
                let (text, color) = status_text(&state);
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
        State::Failed(e) => (format!("Não foi possível verificar: {e}"), theme::DANGER),
    }
}
