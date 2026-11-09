//! Tela "IA": quanto das janelas do Claude e do Codex já foi usado, quando renovam, em
//! que ritmo estão acabando e o que está gastando (projeto, sessão e modelo).

use std::collections::HashSet;

use egui::{Align, Align2, Color32, CornerRadius, Id, Layout, Rect, RichText, Sense, Ui, pos2, vec2};
use egui_phosphor::regular as icon;

use crate::app::App;
use crate::format;
use crate::theme::{self, ACCENT, INK, INK_2, INK_3, INK_4};
use crate::ui::widgets::{self, ChartStyle, Layer};
use crate::usage::{Consumption, Limits, Pace, Provider, ProviderState, Tokens, Window, now_secs, project_name, share};

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
    if !expired
        && let Some(b) = crate::usage::budget(w, now)
        && w.used < 99.95
    {
        budget_line(ui, &b);
    }
    if main
        && let Some(series) = series.filter(|s| s.values().len() >= 2)
    {
        ui.add_space(2.0);
        let size = vec2(ui.available_width(), 26.0);
        widgets::chart(ui, size, &[Layer::area(series, ACCENT)], 100.0, ChartStyle::SPARK)
            .on_hover_text("Uso da janela nas últimas leituras (até 2 h)");
    }
}

/// "Para durar até renovar, use até 12% por dia", com o ritmo atual ao lado.
fn budget_line(ui: &mut Ui, b: &crate::usage::Budget) {
    let unit = b.unit.label();
    let text = if b.last_stretch {
        format!("Resta {} até renovar", format::percent(b.per_unit))
    } else {
        format!("Para durar até renovar, use até {} por {unit}", format::percent(b.per_unit))
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new(text).font(theme::regular(11.5)).color(INK_2));
        if let Some(current) = b.current.filter(|_| !b.last_stretch) {
            // Acima do orçamento (com folga de 5%), o ritmo atual acende.
            let color = if current > b.per_unit * 1.05 { ACCENT } else { INK_3 };
            ui.label(RichText::new("·").font(theme::regular(11.5)).color(INK_4));
            ui.label(RichText::new(format!("ritmo atual {} por {unit}", format::percent(current))).font(theme::regular(11.5)).color(color));
        }
    })
    .response
    .on_hover_text("O que falta da janela dividido pelo tempo até ela renovar. O ritmo atual vem das últimas leituras.");
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
        let provider = app.usage_provider;
        let state = usage.get(provider);
        let period = if app.usage_week { state.week.as_ref() } else { state.recent.as_ref() };
        match (provider, period) {
            (Provider::Claude, _) if !state.installed => muted(ui, "Claude Code não encontrado neste computador."),
            (Provider::Claude, _) if !state.has_logs => {
                muted(ui, "O Claude Code ainda não gravou conversas neste computador (pasta projects).")
            }
            (_, Some(c)) => consumption(ui, provider, c, now),
            (Provider::Claude, None) if !usage.scanned => muted(ui, "Lendo os registros do Claude Code…"),
            (Provider::Claude, None) => muted(ui, "Nada lido ainda."),
            (Provider::Codex, None) if !state.installed => muted(ui, "Codex não encontrado neste computador."),
            (Provider::Codex, None) => muted(
                ui,
                "O Codex CLI não gravou conversas neste computador (pasta sessions do Codex). As conversas do app do Codex \
                 ficam na nuvem e não dizem o consumo por projeto.",
            ),
        }
    });
}

fn consumption(ui: &mut Ui, provider: Provider, c: &Consumption, now: i64) {
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
    if c.total.requests == 0 {
        ui.add_space(6.0);
        muted(ui, "Nada neste período.");
        return;
    }
    ui.add_space(12.0);
    ui.columns(2, |cols| {
        kinds(&mut cols[0], c);
        models(&mut cols[1], c);
    });
    ui.add_space(14.0);
    top_sessions(ui, c, now);
    ui.add_space(14.0);
    projects(ui, provider, c, now);
}

/// Cores das três origens: o laranja da marca em três intensidades (nenhuma cor de recurso).
const KIND_COLORS: [Color32; 3] = [ACCENT, Color32::from_rgb(0xB0, 0x3A, 0x14), Color32::from_rgb(0x6A, 0x2A, 0x14)];

fn kinds(ui: &mut Ui, c: &Consumption) {
    section(ui, "Quem gastou");
    let total = c.total.fresh();
    // Uma barra só, dividida pelas três origens.
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(4), theme::WELL);
    let mut x = rect.left();
    for ((_, t), color) in c.kinds.iter().zip(KIND_COLORS) {
        let w = rect.width() * share(t.fresh(), total) / 100.0;
        if w >= 0.5 {
            let seg = Rect::from_min_max(pos2(x, rect.top()), pos2((x + w).min(rect.right()), rect.bottom()));
            ui.painter().rect_filled(seg, CornerRadius::same(4), color);
            x += w;
        }
    }
    ui.add_space(6.0);
    let tip = [
        "Conversas abertas por uma pessoa: o terminal, o app de desktop ou a extensão do editor.",
        "claude -p, SDK (TypeScript, Python) e scripts que chamam o Claude Code sem ninguém digitando. No Codex, o codex exec.",
        "Agentes que outra sessão abriu para uma tarefa (Task, revisão). Contam aqui, não na sessão que os chamou.",
    ];
    for (((kind, t), color), tip) in c.kinds.iter().zip(KIND_COLORS).zip(tip) {
        value_row(ui, color, kind.label(), t, total).on_hover_text(tip);
    }
}

fn models(ui: &mut Ui, c: &Consumption) {
    section(ui, "Peso de cada modelo");
    let total = c.total.fresh();
    for (i, (family, t)) in c.models.iter().take(6).enumerate() {
        let color = ACCENT.gamma_multiply((1.0 - i as f32 * 0.16).max(0.3));
        value_row(ui, color, family, t, total);
        widgets::meter(ui, share(t.fresh(), total) / 100.0, color, 4.0);
        ui.add_space(2.0);
    }
    if c.models.len() > 6 {
        muted(ui, &format!("E mais {} modelos.", c.models.len() - 6));
    }
}

/// Bolinha, nome à esquerda; tokens e % à direita, alinhados.
fn value_row(ui: &mut Ui, color: Color32, label: &str, t: &Tokens, total: u64) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    let p = ui.painter();
    p.circle_filled(pos2(rect.left() + 4.0, rect.center().y), 4.0, color);
    let pct = format!("{:>5}", format::percent(share(t.fresh(), total)));
    let right = p.text(pos2(rect.right(), rect.center().y), Align2::RIGHT_CENTER, pct, theme::semibold(12.5), INK);
    let tok = p.text(
        pos2(right.left() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{} tokens", tokens(t.fresh())),
        theme::regular(12.0),
        INK_3,
    );
    let name_rect = Rect::from_min_max(pos2(rect.left() + 14.0, rect.top()), pos2(tok.left() - 8.0, rect.bottom()));
    paint_truncated(ui, name_rect, label, theme::regular(12.5), INK_2);
    response
}

fn top_sessions(ui: &mut Ui, c: &Consumption, now: i64) {
    section(ui, "Sessões mais pesadas");
    let total = c.total.fresh();
    for (i, t) in c.top.iter().enumerate() {
        let s = &t.session;
        let row = ui.horizontal(|ui| {
            ui.set_min_height(26.0);
            ui.label(RichText::new(format!("{}", i + 1)).font(theme::semibold(12.5)).color(INK_4));
            let button_w = 30.0;
            let text_w = (ui.available_width() - button_w - 8.0).max(80.0);
            let (rect, _) = ui.allocate_exact_size(vec2(text_w, 26.0), Sense::hover());
            let span = format!(
                "{}–{} ({})",
                moment(s.first, now),
                moment(s.last, now),
                short_duration(s.last.saturating_sub(s.first))
            );
            let right = format!("{} tokens · {}", tokens(s.tokens.fresh()), format::percent(share(s.tokens.fresh(), total)));
            let p = ui.painter();
            let r = p.text(pos2(rect.right(), rect.center().y), Align2::RIGHT_CENTER, right, theme::regular(12.0), INK_2);
            let mid = format!("{span} · {}", crate::usage::model_family(&s.model));
            let m = p.text(pos2(r.left() - 14.0, rect.center().y), Align2::RIGHT_CENTER, mid, theme::regular(12.0), INK_3);
            let name = Rect::from_min_max(rect.left_top(), pos2(m.left() - 10.0, rect.bottom()));
            paint_truncated(ui, name, project_name(&t.project), theme::semibold(12.5), INK);
            let (tip, enabled) = if t.folder_exists {
                (format!("Abrir a pasta do projeto\n{}", t.project), true)
            } else {
                (format!("A pasta não existe mais neste computador\n{}", t.project), false)
            };
            let open = ui
                .add_enabled(enabled, egui::Button::new(RichText::new(icon::FOLDER_OPEN).font(theme::regular(14.0))).min_size(vec2(button_w, 24.0)))
                .on_hover_text(&tip)
                .on_disabled_hover_text(&tip);
            if open.clicked() {
                crate::platform::shell_open(&t.project);
            }
        });
        row.response.on_hover_text(format!("Sessão {}\nModelo: {}", s.id, s.model));
    }
}

/// Larguras da tabela: o nome fica com o que sobra; os números, colunas fixas, mais
/// estreitas em janela pequena.
fn columns_for(width: f32) -> (f32, f32) {
    let num = if width >= 760.0 { 92.0 } else { 76.0 };
    let name = (width - num * 5.0).max(120.0);
    (name, num)
}

fn projects(ui: &mut Ui, provider: Provider, c: &Consumption, now: i64) {
    section(ui, "Por projeto");
    let expanded_id = Id::new(("usage_expanded", provider.key()));
    let mut expanded: HashSet<String> = ui.data(|d| d.get_temp(expanded_id)).unwrap_or_default();
    let cache_cols = provider == Provider::Claude;
    let (name_w, num_w) = columns_for(ui.available_width());
    let headers = ["Respostas", "Entrada", if cache_cols { "Cache criado" } else { "" }, "Cache lido", "Saída"];
    table_row(ui, name_w, num_w, ("Projeto", theme::semibold(11.5), INK_3), &headers.map(|h| (h.to_owned(), None)), INK_3, false);
    crate::app::hairline(ui);
    for p in c.projects.iter().take(30) {
        let open = expanded.contains(&p.path);
        let caret = if open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
        let name = format!("{caret}  {}", p.name());
        let response = table_row(ui, name_w, num_w, (&name, theme::semibold(12.5), INK), &cells(&p.tokens, cache_cols), INK, true);
        let models: Vec<String> = p.models.iter().map(|(m, t)| format!("{m}: {} tokens novos", tokens(t.fresh()))).collect();
        if response.on_hover_text(format!("{}\n\n{}", p.path, models.join("\n"))).clicked() {
            if open {
                expanded.remove(&p.path);
            } else {
                expanded.insert(p.path.clone());
            }
        }
        if open {
            for s in p.sessions.iter().take(20) {
                let short: String = s.id.chars().take(8).collect();
                let text = format!("      {short} · {}–{} · {}", moment(s.first, now), moment(s.last, now), crate::usage::model_family(&s.model));
                table_row(ui, name_w, num_w, (&text, theme::regular(12.0), INK_2), &cells(&s.tokens, cache_cols), INK_2, false)
                    .on_hover_text(format!("Sessão {}\nModelo: {}", s.id, s.model));
            }
        }
    }
    if c.projects.len() > 30 {
        muted(ui, &format!("E mais {} projetos com menos consumo.", c.projects.len() - 30));
    }
    ui.data_mut(|d| d.insert_temp(expanded_id, expanded));
}

/// Textos das colunas numéricas, com a dica do cache criado (5 min e 1 h).
fn cells(t: &Tokens, cache_cols: bool) -> [(String, Option<String>); 5] {
    let created = if cache_cols {
        (tokens(t.cache_create()), Some(format!("Cache de 5 min: {} · de 1 h: {}", tokens(t.cache_create_5m), tokens(t.cache_create_1h))))
    } else {
        ("—".to_owned(), None)
    };
    [(t.requests.to_string(), None), (tokens(t.input), None), created, (tokens(t.cache_read), None), (tokens(t.output), None)]
}

/// Uma linha da tabela em colunas fixas: o nome à esquerda, cortado com reticências, e
/// cada número alinhado à direita da coluna dele (o cabeçalho também), para as colunas
/// ficarem retas.
fn table_row(
    ui: &mut Ui,
    name_w: f32,
    num_w: f32,
    name: (&str, egui::FontId, Color32),
    numbers: &[(String, Option<String>); 5],
    color: Color32,
    clickable: bool,
) -> egui::Response {
    let height = 24.0;
    let sense = if clickable { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(name_w + num_w * 5.0, height), sense);
    if clickable && response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R_SM), theme::GRAPHITE_2);
    }
    let name_rect = Rect::from_min_size(rect.left_top() + vec2(4.0, 0.0), vec2(name_w - 12.0, height));
    paint_truncated(ui, name_rect, name.0, name.1, name.2);
    let header = color == INK_3;
    let font = if header { theme::semibold(11.5) } else { theme::regular(12.5) };
    for (i, (text, tip)) in numbers.iter().enumerate() {
        let cell = Rect::from_min_size(pos2(rect.left() + name_w + num_w * i as f32, rect.top()), vec2(num_w, height));
        ui.painter().text(pos2(cell.right() - 6.0, cell.center().y), Align2::RIGHT_CENTER, text, font.clone(), color);
        if let Some(tip) = tip {
            ui.interact(cell, ui.id().with(("usage_cell", rect.top() as i64, i)), Sense::hover()).on_hover_text(tip);
        }
    }
    if clickable { response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { response }
}

/// Texto numa linha dentro de `rect`, cortado com reticências.
fn paint_truncated(ui: &Ui, rect: Rect, text: &str, font: egui::FontId, color: Color32) {
    if rect.width() < 8.0 {
        return;
    }
    let galley = ui.fonts_mut(|f| {
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width());
        f.layout_job(job)
    });
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(pos2(rect.left(), y), galley, color);
}

fn section(ui: &mut Ui, text: &str) {
    widgets::section_label(ui, text);
    ui.add_space(4.0);
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
