//! "Quem está usando": caminho de um arquivo ou pasta → processos que o mantêm aberto.
//! Abre pelo botão da tela de processos ou soltando arquivos na janela.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use egui::{Align, Color32, CornerRadius, Frame, Id, Layout, Margin, RichText, Stroke, Ui, vec2};
use egui_phosphor::regular as icon;

use crate::app::{App, View};
use crate::locks::{self, Report};
use crate::sampler::{ProcInfo, ProcKey, Snapshot};
use crate::theme::{self, HAIRLINE_STRONG, INK, INK_2, INK_3, R_LG, WELL};
use crate::ui::widgets;

/// Diferença tolerada entre a hora de criação do Restart Manager e a do sysinfo.
const START_TOLERANCE: u64 = 2;

pub struct State {
    pub open: bool,
    /// Um caminho por linha.
    pub input: String,
    pending: Option<Receiver<Result<Report, String>>>,
    result: Option<Result<Report, String>>,
    ctx: egui::Context,
}

impl State {
    pub fn new(ctx: egui::Context) -> Self {
        Self { open: false, input: String::new(), pending: None, result: None, ctx }
    }

    /// Abre a janela já verificando estes caminhos (arquivos soltos na janela).
    pub fn open_with(&mut self, paths: &[PathBuf]) {
        self.input = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        self.open = true;
        self.check();
    }

    /// Verifica o que está no campo, numa thread: uma pasta grande leva segundos.
    pub fn check(&mut self) {
        self.check_after(Duration::ZERO);
    }

    /// Verifica depois de `delay`: logo após finalizar, o processo ainda pode estar saindo.
    pub fn check_after(&mut self, delay: Duration) {
        let paths = parse_paths(&self.input);
        if paths.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("locks".into())
            .spawn(move || {
                std::thread::sleep(delay);
                let _ = tx.send(locks::find(&paths));
                ctx.request_repaint();
            })
            .expect("thread de verificação");
        self.pending = Some(rx);
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.pending
            && let Ok(result) = rx.try_recv()
        {
            self.result = Some(result);
            self.pending = None;
        }
    }
}

/// Um caminho por linha. Aspas em volta saem: o "Copiar como caminho" do Explorer as põe.
fn parse_paths(text: &str) -> Vec<PathBuf> {
    text.lines().map(|l| l.trim().trim_matches('"').trim()).filter(|l| !l.is_empty()).map(PathBuf::from).collect()
}

enum Action {
    Show(ProcKey),
    Kill(ProcKey),
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    app.locks.poll();
    if !app.locks.open {
        return;
    }
    let snap = app.snap.clone();
    // Calculado antes da janela: `is_protected` precisa do `app` inteiro.
    let mut protected: HashSet<ProcKey> = HashSet::new();
    if let Some(Ok(report)) = &app.locks.result {
        let procs: Vec<(ProcKey, String)> = report
            .lockers
            .iter()
            .filter_map(|l| matching(snap.as_deref(), l.pid, l.start))
            .map(|p| (p.key(), p.name.clone()))
            .collect();
        for (key, name) in procs {
            if app.is_protected(key, &name) {
                protected.insert(key);
            }
        }
    }
    let mut close = false;
    let mut actions: Vec<Action> = Vec::new();
    let modal = egui::Modal::new(Id::new("locks"))
        .frame(
            Frame::NONE
                .fill(theme::GRAPHITE_2)
                .stroke(Stroke::new(1.0, HAIRLINE_STRONG))
                .corner_radius(CornerRadius::same(R_LG))
                .inner_margin(Margin::same(22)),
        )
        .backdrop_color(Color32::from_black_alpha(150))
        .show(ctx, |ui| {
            ui.set_width(520.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::FILE_LOCK).font(theme::regular(20.0)).color(INK_2));
                ui.label(RichText::new("Quem está usando").font(theme::semibold(16.0)).color(INK));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, "Fechar (Esc)").clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new("Caminho de um arquivo ou pasta, um por linha. Também dá para soltar arquivos na janela.")
                    .font(theme::regular(12.5))
                    .color(INK_3),
            );
            ui.add_space(10.0);
            // Enter verifica; Shift+Enter quebra a linha para mais um caminho. O Enter é
            // consumido antes do campo, senão vira uma quebra no meio do caminho.
            let input_id = Id::new("locks_input");
            let enter = ui.memory(|m| m.has_focus(input_id))
                && !ui.input(|i| i.modifiers.shift)
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let edit = egui::TextEdit::multiline(&mut app.locks.input)
                .id(input_id)
                .hint_text(r"D:\Repos\projeto\target\release\app.exe")
                .font(theme::mono(12.5))
                .desired_rows(2)
                .desired_width(f32::INFINITY);
            ui.add(edit);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let busy = app.locks.pending.is_some();
                if (widgets::accent_button(ui, &format!("{}  Verificar", icon::MAGNIFYING_GLASS)).clicked() || enter) && !busy {
                    app.locks.check();
                }
                if busy {
                    ui.add(egui::Spinner::new().size(14.0).color(INK_3));
                    ui.label(RichText::new("Verificando…").font(theme::regular(12.5)).color(INK_3));
                }
            });
            if let Some(result) = &app.locks.result {
                ui.add_space(14.0);
                results(ui, &mut app.icons, snap.as_deref(), &protected, result, &mut actions);
            }
        });
    if modal.should_close() {
        close = true;
    }
    if close {
        app.locks.open = false;
    }
    for action in actions {
        match action {
            Action::Show(key) => {
                app.locks.open = false;
                app.view = View::Processes;
                app.select(Some(key));
                app.scroll_to_selected = true;
            }
            // A confirmação aparece por cima; depois de finalizar, a lista é refeita.
            Action::Kill(key) => app.request_kill(key, false),
        }
    }
}

/// Processo da coleta que corresponde ao do Restart Manager (mesmo PID e mesma hora de criação).
fn matching(snap: Option<&Snapshot>, pid: u32, start: u64) -> Option<&ProcInfo> {
    snap?.processes.iter().find(|p| p.pid == pid && p.start_time.abs_diff(start) <= START_TOLERANCE)
}

fn results(
    ui: &mut Ui,
    icons: &mut crate::icons::IconCache,
    snap: Option<&Snapshot>,
    protected: &HashSet<ProcKey>,
    result: &Result<Report, String>,
    actions: &mut Vec<Action>,
) {
    let report = match result {
        Err(e) => {
            ui.label(RichText::new(e).font(theme::regular(13.0)).color(theme::DANGER));
            return;
        }
        Ok(r) => r,
    };
    let checked = if report.files == 1 { "1 arquivo verificado".to_owned() } else { format!("{} arquivos verificados", report.files) };
    if report.lockers.is_empty() {
        ui.label(RichText::new("Nenhum processo está usando.").font(theme::semibold(13.5)).color(INK));
        ui.label(RichText::new(checked).font(theme::regular(12.0)).color(INK_3));
    } else {
        let n = report.lockers.len();
        let title = if n == 1 { "1 processo está usando".to_owned() } else { format!("{n} processos estão usando") };
        widgets::section_label(ui, &title);
        ui.add_space(6.0);
        Frame::NONE.fill(WELL).corner_radius(CornerRadius::same(theme::R_SM)).inner_margin(Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                for l in &report.lockers {
                    let proc = matching(snap, l.pid, l.start);
                    ui.horizontal(|ui| {
                        let texture = icons.get(proc.and_then(|p| p.exe.as_deref()));
                        widgets::process_icon(ui, texture.as_ref(), 20.0);
                        // Botões primeiro, pela direita; o nome fica com o que sobra e corta.
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Some(p) = proc {
                                let safe = !protected.contains(&p.key());
                                let kill = widgets::danger_button(ui, "Finalizar", safe)
                                    .on_disabled_hover_text("Processo crítico do sistema ou o próprio Process Monitor.");
                                if kill.clicked() {
                                    actions.push(Action::Kill(p.key()));
                                }
                                if widgets::ghost_button(ui, "Ver na lista").clicked() {
                                    actions.push(Action::Show(p.key()));
                                }
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                let name = proc.map(|p| p.display_name().to_owned()).unwrap_or_else(|| l.app_name.clone());
                                let meta = match proc {
                                    Some(p) => format!("{} · PID {}", p.name, l.pid),
                                    None => format!("PID {}", l.pid),
                                };
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    ui.add(egui::Label::new(RichText::new(name).font(theme::regular(13.0)).color(INK)).truncate());
                                    ui.add(egui::Label::new(RichText::new(meta).font(theme::regular(11.5)).color(INK_3)).truncate());
                                });
                            });
                        });
                    });
                    ui.add_space(4.0);
                }
            });
        });
        ui.add_space(4.0);
        ui.label(RichText::new(checked).font(theme::regular(12.0)).color(INK_3));
    }
    if report.truncated {
        ui.label(
            RichText::new("A pasta tem mais de 10 000 arquivos: só os primeiros foram verificados.")
                .font(theme::regular(12.0))
                .color(INK_2),
        );
    }
}

/// Aviso por cima da janela enquanto arquivos são arrastados sobre ela.
pub fn drop_overlay(ctx: &egui::Context) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let rect = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("drop_overlay")));
    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_black_alpha(190));
    painter.rect_stroke(
        rect.shrink(16.0),
        CornerRadius::same(R_LG),
        Stroke::new(1.5, theme::ACCENT),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center() - vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        icon::FILE_LOCK,
        theme::regular(34.0),
        theme::ACCENT,
    );
    painter.text(
        rect.center() + vec2(0.0, 22.0),
        egui::Align2::CENTER_CENTER,
        "Solte para ver quem está usando",
        theme::semibold(16.0),
        INK,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caminhos_um_por_linha_sem_aspas() {
        let paths = parse_paths("\"C:\\a b\\x.exe\"\r\n\n  D:\\pasta  \n");
        assert_eq!(paths, vec![PathBuf::from(r"C:\a b\x.exe"), PathBuf::from(r"D:\pasta")]);
        assert!(parse_paths("  \n ").is_empty());
    }

    #[test]
    fn soltar_arquivo_abre_e_verifica_em_segundo_plano() {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::env::temp_dir().join(format!("process-monitor-solto-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();

        let mut state = State::new(egui::Context::default());
        state.open_with(std::slice::from_ref(&file));
        assert!(state.open);
        assert_eq!(state.input, file.display().to_string());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while state.result.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            state.poll();
        }
        let report = state.result.take().expect("resultado").expect("consulta");
        assert!(report.lockers.iter().any(|l| l.pid == std::process::id()));
        assert!(state.pending.is_none());
        drop(held);
        let _ = std::fs::remove_file(&file);
    }
}
