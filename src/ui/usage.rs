//! Tela "IA": quanto das janelas do Claude e do Codex já foi usado, quando renovam, em
//! que ritmo estão acabando e o que está gastando (projeto, sessão e modelo).

use std::collections::HashSet;

use egui::{Align, Color32, CornerRadius, Id, Layout, Rect, RichText, Sense, Ui, pos2, vec2};
use egui_phosphor::regular as icon;

use crate::app::App;
use crate::format;
use crate::theme::{self, ACCENT, INK, INK_2, INK_3, INK_4};
use crate::ui::widgets::{self, ChartStyle, Layer};
use crate::usage::{Consumption, Limits, Pace, Provider, ProviderState, Tokens, Window, now_secs};

pub fn show(app: &mut App, ui: &mut Ui) {
    let usage = app.usage_state.clone();
    let now = now_secs();

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new("Uso de IA").font(theme::semibold(22.0)).color(INK));
            ui.label(
                RichText::new("Limites do Claude e do Codex, quando renovam e o que está gastando")
                    .font(theme::regular(12.5))
                    .color(INK_3),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::ghost_button(ui, &format!("{}  Atualizar", icon::ARROW_CLOCKWISE))
                .on_hover_text("Relê os arquivos agora. A consulta própria continua no máximo a cada 5 min.")
                .clicked()
            {
                app.usage.refresh_now();
            }
        });
    });
    ui.add_space(14.0);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.columns(2, |cols| {
            provider_card(&mut cols[0], Provider::Claude, &usage.claude, now);
            provider_card(&mut cols[1], Provider::Codex, &usage.codex, now);
        });
        ui.add_space(10.0);
        consumption_card(app, ui, &usage, now);
        ui.add_space(10.0);
        footnote(ui, usage.network);
    });
}

fn provider_card(ui: &mut Ui, provider: Provider, state: &ProviderState, now: i64) {
    let glyph = match provider {
        Provider::Claude => icon::SPARKLE,
        Provider::Codex => icon::CODE,
    };
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).font(theme::regular(16.0)).color(ACCENT));
            ui.label(RichText::new(provider.label()).font(theme::semibold(15.0)).color(INK));
            if let Some(plan) = state.limits.as_ref().and_then(|l| l.plan.as_deref()) {
                ui.label(RichText::new(capitalize(plan)).font(theme::regular(12.0)).color(INK_3));
            }
            if let Some(l) = &state.limits {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(source_line(l, now)).font(theme::regular(11.5)).color(INK_3)).on_hover_text(
                        format!("Lido {}.", format::when(l.fetched_at, now)),
                    );
                });
            }
        });
        ui.add_space(8.0);
        match &state.limits {
            Some(limits) => {
                for (i, w) in limits.windows.iter().enumerate() {
                    if i > 0 {
                        ui.add_space(10.0);
                    }
                    let series = state.history.iter().find(|(id, _)| *id == w.id).map(|(_, s)| s);
                    window_row(ui, w, series, now);
                }
            }
            None => {
                ui.add_space(4.0);
            }
        }
        if let Some(note) = &state.note {
            ui.add_space(8.0);
            ui.add(egui::Label::new(RichText::new(note).font(theme::regular(12.0)).color(INK_3)).wrap());
        }
    });
}

fn source_line(l: &Limits, now: i64) -> String {
    let age = now - l.fetched_at;
    let when = if age < 60 { "agora".to_owned() } else { format!("há {}", short_duration(age)) };
    format!("{} · {when}", l.source.label())
}

fn window_row(ui: &mut Ui, w: &Window, series: Option<&crate::sampler::Series>, now: i64) {
    let expired = w.expired(now);
    let main = matches!(w.id.as_str(), crate::usage::FIVE_HOUR | crate::usage::SEVEN_DAY);
    let title = capitalize(&w.label());
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::semibold(if main { 13.0 } else { 12.0 })).color(INK_2));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let value = if expired { "—".to_owned() } else { format::percent(w.used) };
            ui.label(RichText::new(value).font(theme::semibold(if main { 16.0 } else { 13.0 })).color(INK));
        });
    });
    let t = if expired { 0.0 } else { w.used / 100.0 };
    widgets::meter(ui, t, meter_color(t), if main { 8.0 } else { 5.0 });
    ui.add_space(2.0);

    let reset = match w.resets_at {
        Some(r) if r <= now => format!("Renovou {}; esperando uma leitura nova", format::when(r, now)),
        Some(r) => format!("Renova em {}, {}", short_duration(r - now), format::when(r, now)),
        None => "Hora de renovar desconhecida".to_owned(),
    };
    let (pace, color) = match w.pace {
        _ if expired => (String::new(), INK_3),
        Pace::Exhausted => ("esgotado até renovar".to_owned(), theme::DANGER),
        Pace::RunsOutAt(at) => (format!("no ritmo atual, acaba {}", format::when(at, now)), ACCENT),
        Pace::Safe => ("no ritmo atual, renova antes de acabar".to_owned(), INK_3),
        Pace::Unknown => (String::new(), INK_3),
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new(reset).font(theme::regular(11.5)).color(INK_3));
        if !pace.is_empty() {
            ui.label(RichText::new("·").font(theme::regular(11.5)).color(INK_4));
            ui.label(RichText::new(pace).font(theme::regular(11.5)).color(color));
        }
    });
    if main
        && let Some(series) = series.filter(|s| s.values().len() >= 2)
    {
        ui.add_space(2.0);
        let size = vec2(ui.available_width(), 26.0);
        widgets::chart(ui, size, &[Layer::area(series, ACCENT)], 100.0, ChartStyle::SPARK)
            .on_hover_text("Uso da janela nas últimas leituras (até 2 h)");
    }
}

/// Laranja que acende com a carga, como as células de calor.
fn meter_color(t: f32) -> Color32 {
    ACCENT.gamma_multiply(0.45 + 0.55 * t.clamp(0.0, 1.0))
}

fn consumption_card(app: &mut App, ui: &mut Ui, usage: &crate::usage::Usage, now: i64) {
    widgets::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::CHART_BAR).font(theme::regular(16.0)).color(ACCENT));
            ui.label(RichText::new("O que está gastando").font(theme::semibold(15.0)).color(INK));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (week, text) in [(true, "Semana"), (false, "Janela de 5 h")] {
                    if chip(ui, app.usage_week == week, text).clicked() {
                        app.usage_week = week;
                    }
                }
                ui.add_space(10.0);
                for p in [Provider::Codex, Provider::Claude] {
                    if chip(ui, app.usage_provider == p, p.label()).clicked() {
                        app.usage_provider = p;
                    }
                }
            });
        });
        ui.add_space(8.0);
        let state = usage.get(app.usage_provider);
        let period = if app.usage_week { state.week.as_ref() } else { state.recent.as_ref() };
        match (app.usage_provider, period) {
            (_, Some(c)) => ranking(ui, app.usage_provider, c, now),
            (Provider::Claude, None) if state.has_logs || !usage.scanned => {
                muted(ui, "Lendo os registros do Claude Code…");
            }
            (Provider::Claude, None) => muted(ui, "Nenhum registro do Claude Code em ~/.claude/projects."),
            (Provider::Codex, None) => muted(
                ui,
                "O Codex CLI não gravou conversas nesta máquina (~/.codex/sessions). As conversas do app do Codex ficam na \
                 nuvem e não dizem o consumo por projeto.",
            ),
        }
    });
}

fn ranking(ui: &mut Ui, provider: Provider, c: &Consumption, now: i64) {
    let since = if c.aligned {
        format!("Desde {} (início da janela atual)", moment(c.since, now))
    } else {
        format!("Desde {}", moment(c.since, now))
    };
    ui.label(
        RichText::new(format!("{since} · {} respostas · {} tokens novos", c.total.requests, tokens(c.total.fresh())))
            .font(theme::regular(12.0))
            .color(INK_3),
    )
    .on_hover_text(
        "Tokens novos: entrada, saída e cache criado. O cache lido fica de fora da soma: é barato e dominaria o \
         total. A Anthropic e a OpenAI não publicam quanto cada tipo pesa no limite.",
    );
    ui.add_space(6.0);
    if c.projects.is_empty() {
        muted(ui, "Nada neste período.");
        return;
    }
    let expanded_id = Id::new(("usage_expanded", provider.key()));
    let mut expanded: HashSet<String> = ui.data(|d| d.get_temp(expanded_id)).unwrap_or_default();
    let cache_cols = provider == Provider::Claude;
    egui::Grid::new(("usage_ranking", provider.key())).num_columns(6).spacing(vec2(14.0, 6.0)).striped(false).show(
        ui,
        |ui| {
            for h in ["Projeto", "Respostas", "Entrada", if cache_cols { "Cache criado" } else { "" }, "Cache lido", "Saída"] {
                ui.label(RichText::new(h).font(theme::semibold(11.5)).color(INK_3));
            }
            ui.end_row();
            let name_width = 260.0;
            for p in c.projects.iter().take(30) {
                let open = expanded.contains(&p.path);
                let caret = if open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
                let response = truncated(ui, &format!("{caret}  {}", p.name()), name_width, INK, theme::semibold(12.5));
                let models: Vec<String> = p.models.iter().map(|(m, t)| format!("{m}: {} tokens novos", tokens(t.fresh()))).collect();
                let response = response.on_hover_text(format!("{}\n\n{}", p.path, models.join("\n")));
                if response.clicked() {
                    if open {
                        expanded.remove(&p.path);
                    } else {
                        expanded.insert(p.path.clone());
                    }
                }
                numbers(ui, &p.tokens, cache_cols, INK);
                ui.end_row();
                if open {
                    for s in p.sessions.iter().take(20) {
                        let short: String = s.id.chars().take(8).collect();
                        let span = format!("{}–{}", moment(s.first, now), moment(s.last, now));
                        let text = format!("      {short} · {span} · {}", s.model);
                        truncated(ui, &text, name_width, INK_2, theme::regular(12.0)).on_hover_text(format!("Sessão {}", s.id));
                        numbers(ui, &s.tokens, cache_cols, INK_2);
                        ui.end_row();
                    }
                }
            }
        },
    );
    if c.projects.len() > 30 {
        muted(ui, &format!("E mais {} projetos com menos consumo.", c.projects.len() - 30));
    }
    ui.data_mut(|d| d.insert_temp(expanded_id, expanded));
}

fn numbers(ui: &mut Ui, t: &Tokens, cache_cols: bool, color: Color32) {
    let cell = |ui: &mut Ui, text: String| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(text).font(theme::regular(12.5)).color(color));
        });
    };
    cell(ui, t.requests.to_string());
    cell(ui, tokens(t.input));
    if cache_cols {
        let created = tokens(t.cache_create());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(created).font(theme::regular(12.5)).color(color))
                .on_hover_text(format!("5 min: {} · 1 h: {}", tokens(t.cache_create_5m), tokens(t.cache_create_1h)));
        });
    } else {
        ui.label("");
    }
    cell(ui, tokens(t.cache_read));
    cell(ui, tokens(t.output));
}

/// Texto numa linha, cortado com reticências, clicável.
fn truncated(ui: &mut Ui, text: &str, width: f32, color: Color32, font: egui::FontId) -> egui::Response {
    let galley = ui.fonts_mut(|f| {
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(width);
        f.layout_job(job)
    });
    let (rect, response) = ui.allocate_exact_size(vec2(width, galley.size().y), Sense::click());
    ui.painter().galley(rect.left_top(), galley, color);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn chip(ui: &mut Ui, selected: bool, text: &str) -> egui::Response {
    let button = egui::Button::selectable(
        selected,
        RichText::new(text).font(theme::regular(12.0)).color(if selected { INK } else { INK_3 }),
    )
    .corner_radius(CornerRadius::same(theme::R_SM))
    .min_size(vec2(44.0, 24.0));
    ui.add(button)
}

fn muted(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).font(theme::regular(12.5)).color(INK_3)).wrap());
}

fn footnote(ui: &mut Ui, network: bool) {
    let consult = if network {
        "A consulta própria está ligada: no máximo a cada 5 min, e só quando o Codenotch e a statusline não têm leitura recente."
    } else {
        "A consulta própria aos servidores está desligada (Configurações → IA)."
    };
    ui.add(
        egui::Label::new(
            RichText::new(format!(
                "Fontes, da mais barata para a mais cara: o Codenotch, se estiver aberto; a statusline do Claude Code; os \
                 registros do Codex CLI. {consult} Os números de consumo são dos registros locais e não dão a %: os \
                 fornecedores não publicam quantos tokens valem 100%."
            ))
            .font(theme::regular(11.5))
            .color(INK_4),
        )
        .wrap(),
    );
}

/// Milhares e milhões, com vírgula: `845`, `12,3 mil`, `4,56 mi`.
pub fn tokens(n: u64) -> String {
    let v = n as f64;
    if n < 1000 {
        n.to_string()
    } else if v < 1e6 {
        format!("{} mil", decimal(v / 1e3))
    } else if v < 1e9 {
        format!("{} mi", decimal(v / 1e6))
    } else {
        format!("{} bi", decimal(v / 1e9))
    }
}

fn decimal(v: f64) -> String {
    let places = if v >= 100.0 { 0 } else if v >= 10.0 { 1 } else { 2 };
    format!("{v:.places$}").replace('.', ",")
}

/// Duração em no máximo duas unidades, sem segundos acima de um minuto: `2 h 14 min`.
fn short_duration(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    if secs < 60 {
        return format!("{secs} s");
    }
    let (d, h, m) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m:02} min")
    } else {
        format!("{m} min")
    }
}

/// `10:30` hoje, `05/11 10:30` em outro dia.
fn moment(unix: i64, now: i64) -> String {
    use chrono::{Local, TimeZone};
    let (Some(at), Some(today)) = (Local.timestamp_opt(unix, 0).single(), Local.timestamp_opt(now, 0).single()) else {
        return "—".to_owned();
    };
    if at.date_naive() == today.date_naive() { at.format("%H:%M").to_string() } else { at.format("%d/%m %H:%M").to_string() }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Contador curto da barra lateral: o maior uso de cada provedor (`62·18%`).
pub fn badge(usage: &crate::usage::Usage, now: i64) -> (String, bool) {
    let peak = |p: Provider| usage.get(p).limits.as_ref().and_then(|l| l.peak(now));
    let (c, x) = (peak(Provider::Claude), peak(Provider::Codex));
    let hot = c.into_iter().chain(x).any(|v| v >= crate::alerts::USAGE_THRESHOLDS[0]);
    let n = |v: f32| format!("{}", v.round() as u32);
    let text = match (c, x) {
        (Some(c), Some(x)) => format!("{}·{}%", n(c), n(x)),
        (Some(v), None) | (None, Some(v)) => format!("{}%", n(v)),
        (None, None) => String::new(),
    };
    (text, hot)
}

/// Retângulo de um item da barra lateral com metade da largura (Sistema e IA dividem a linha).
pub fn half_rect(row: Rect, right: bool) -> Rect {
    let half = ((row.width() - 4.0) / 2.0).floor() - 1.0;
    let left = if right { row.right() - half } else { row.left() };
    Rect::from_min_size(pos2(left, row.top()), vec2(half, row.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_curtos() {
        assert_eq!(tokens(845), "845");
        assert_eq!(tokens(12_345), "12,3 mil");
        assert_eq!(tokens(4_560_000), "4,56 mi");
        assert_eq!(tokens(250_000_000), "250 mi");
        assert_eq!(short_duration(30), "30 s");
        assert_eq!(short_duration(2 * 3600 + 14 * 60 + 5), "2 h 14 min");
        assert_eq!(short_duration(3 * 86_400 + 3600), "3 d 1 h");
    }

    #[test]
    fn contador_da_barra_lateral() {
        use crate::usage::{FIVE_HOUR, Limits, Source, Usage, Window};
        let mut u = Usage::default();
        assert_eq!(badge(&u, 0), (String::new(), false));
        let l = |used: f32| Limits { windows: vec![Window::new(FIVE_HOUR, used, None, None)], source: Source::Api, fetched_at: 0, plan: None };
        u.claude.limits = Some(l(61.6));
        assert_eq!(badge(&u, 0), ("62%".to_owned(), false));
        u.codex.limits = Some(l(80.0));
        assert_eq!(badge(&u, 0), ("62·80%".to_owned(), true));
    }
}
