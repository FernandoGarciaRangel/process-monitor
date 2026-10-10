//! Peças visuais reaproveitadas pelas telas.

use egui::{
    Align, Align2, Color32, CornerRadius, Frame, Key, Layout, Margin, Mesh, Pos2, Rect, Response, RichText, Sense,
    Shape, Stroke, TextureHandle, Ui, Vec2, pos2, vec2,
};

use crate::sampler::{HISTORY_LEN, Series};
use crate::theme::{self, GRAPHITE_1, HAIRLINE, INK, INK_2, INK_3, INK_4, R_MD, R_SM, WELL};

/// Uma série num gráfico. `fill` pinta a área com degradê; sem ele, só a linha.
pub struct Layer<'a> {
    pub series: &'a Series,
    pub color: Color32,
    pub fill: bool,
}

impl<'a> Layer<'a> {
    pub fn area(series: &'a Series, color: Color32) -> Self {
        Self { series, color, fill: true }
    }

    pub fn line(series: &'a Series, color: Color32) -> Self {
        Self { series, color, fill: false }
    }
}

#[derive(Clone, Copy)]
pub struct ChartStyle {
    pub grid: bool,
    /// Ponto luminoso no valor mais recente: a assinatura dos gráficos.
    pub head: bool,
    pub line_width: f32,
}

impl ChartStyle {
    pub const SPARK: Self = Self { grid: false, head: true, line_width: 1.4 };
    pub const FULL: Self = Self { grid: true, head: true, line_width: 1.8 };
}

/// Reserva o espaço e desenha o gráfico.
pub fn chart(ui: &mut Ui, size: Vec2, layers: &[Layer], max: f32, style: ChartStyle) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    paint_chart(ui, rect, layers, max, style);
    response
}

/// Desenha o gráfico num retângulo já reservado. O tempo corre da direita para a
/// esquerda e o histórico incompleto encosta à direita, como no Gerenciador de Tarefas.
pub fn paint_chart(ui: &Ui, rect: Rect, layers: &[Layer], max: f32, style: ChartStyle) {
    let painter = ui.painter_at(rect.expand(1.0));
    if style.grid {
        for i in 1..4 {
            let y = rect.top() + rect.height() * i as f32 / 4.0;
            painter.hline(rect.x_range(), y, Stroke::new(1.0, HAIRLINE));
        }
    }

    // Folga no topo e à direita para o ponto de cabeça não ser cortado.
    let pad = if style.head { 5.0 } else { 1.0 };
    let plot = Rect::from_min_max(rect.min + vec2(0.0, pad), rect.max - vec2(pad, 0.5));
    let max = max.max(f32::EPSILON);
    let step = plot.width() / (HISTORY_LEN - 1) as f32;

    for layer in layers {
        let n = layer.series.values().len();
        if n == 0 {
            continue;
        }
        let points: Vec<Pos2> = layer
            .series
            .values()
            .enumerate()
            .map(|(i, v)| {
                let x = plot.right() - (n - 1 - i) as f32 * step;
                let y = plot.bottom() - (v / max).clamp(0.0, 1.0) * plot.height();
                pos2(x, y)
            })
            .collect();

        if layer.fill && points.len() >= 2 {
            let mut mesh = Mesh::default();
            let top = layer.color.gamma_multiply(0.30);
            let bottom = layer.color.gamma_multiply(0.0);
            for p in &points {
                mesh.colored_vertex(*p, top);
                mesh.colored_vertex(pos2(p.x, plot.bottom()), bottom);
            }
            for i in 0..(points.len() as u32 - 1) {
                let (a, b, c, d) = (i * 2, i * 2 + 1, i * 2 + 2, i * 2 + 3);
                mesh.add_triangle(a, b, c);
                mesh.add_triangle(b, d, c);
            }
            painter.add(Shape::mesh(mesh));
        }
        if points.len() >= 2 {
            painter.line(points.clone(), Stroke::new(style.line_width, layer.color));
        }
        if style.head
            && let Some(last) = points.last()
        {
            painter.circle_filled(*last, 6.0, layer.color.gamma_multiply(0.16));
            painter.circle_filled(*last, 2.6, layer.color);
        }
    }
}

/// Teto "redondo" para a escala de um gráfico sem limite natural (taxas de I/O).
pub fn nice_max(value: f32, floor: f32) -> f32 {
    let v = value.max(floor);
    let magnitude = 10f32.powf(v.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if v <= m * magnitude {
            return m * magnitude;
        }
    }
    10.0 * magnitude
}

/// Superfície elevada um nível: cartões e blocos de conteúdo.
pub fn card() -> Frame {
    Frame::NONE
        .fill(GRAPHITE_1)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(R_MD))
        .inner_margin(Margin::same(14))
}

/// Rótulo de seção: pequeno, espaçado e apagado, para não competir com os valores.
pub fn section_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).font(theme::semibold(10.5)).color(INK_3).extra_letter_spacing(0.6));
}

/// Estatística: rótulo apagado em cima, valor em seminegrito embaixo.
pub fn stat(ui: &mut Ui, label: &str, value: impl Into<String>) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(label).font(theme::regular(11.5)).color(INK_3));
        ui.label(RichText::new(value.into()).font(theme::semibold(15.0)).color(INK));
    });
}

/// Linha chave/valor dos detalhes. O valor quebra linha e pode ser selecionado.
pub fn field(ui: &mut Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).font(theme::regular(12.0)).color(INK_3));
    long_text(ui, value, theme::regular(12.5), INK);
    ui.end_row();
}

/// Texto que pode quebrar em qualquer caractere. Caminhos e linhas de comando têm
/// trechos longos sem espaço; quebrando só em palavras, um argumento comprido
/// alargaria o painel em vez de descer para a linha seguinte.
pub fn long_text(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32) {
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap.break_anywhere = true;
    ui.add(egui::Label::new(job).wrap().selectable(true));
}

/// Barra de proporção (uso de disco, VRAM, memória).
pub fn meter(ui: &mut Ui, fraction: f32, color: Color32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let painter = ui.painter();
    let radius = CornerRadius::same((height / 2.0) as u8);
    painter.rect_filled(rect, radius, theme::WELL);
    let w = rect.width() * fraction.clamp(0.0, 1.0);
    if w > 0.5 {
        let fill = Rect::from_min_size(rect.min, vec2(w.max(height), rect.height()));
        painter.rect_filled(fill, radius, color);
    }
}

/// Bolinha de legenda + texto.
pub fn legend(ui: &mut Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(RichText::new(text).font(theme::regular(12.0)).color(INK_2));
    });
}

/// Ícone do executável no tamanho pedido; sem ícone, um glifo neutro do mesmo tamanho.
pub fn process_icon(ui: &mut Ui, texture: Option<&TextureHandle>, size: f32) {
    match texture {
        Some(tex) => {
            ui.add(egui::Image::from_texture((tex.id(), vec2(size, size))).fit_to_exact_size(vec2(size, size)));
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                egui_phosphor::regular::APP_WINDOW,
                theme::regular(size * 0.9),
                INK_4,
            );
        }
    }
}

/// Botão destrutivo: vermelho só no texto e na borda até o hover, para não gritar em repouso.
pub fn danger_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let fill = theme::DANGER.gamma_multiply(0.14);
    let button = egui::Button::new(RichText::new(text).color(theme::DANGER).font(theme::semibold(13.0)))
        .fill(fill)
        .stroke(Stroke::new(1.0, theme::DANGER.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(R_SM))
        .min_size(vec2(0.0, 32.0));
    ui.add_enabled(enabled, button)
}

/// Botão da ação principal, em laranja.
pub fn accent_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(Color32::BLACK).font(theme::semibold(13.0)))
            .fill(theme::ACCENT)
            .corner_radius(CornerRadius::same(R_SM))
            .min_size(vec2(0.0, 32.0)),
    )
}

/// Interruptor liga/desliga. `changed()` na resposta quando o clique troca o valor.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, ""));
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let painter = ui.painter();
    let radius = CornerRadius::same(10);
    painter.rect_filled(rect, radius, theme::lerp(theme::GRAPHITE_3, theme::ACCENT, t));
    if t < 1.0 {
        painter.rect_stroke(rect, radius, Stroke::new(1.0, theme::HAIRLINE_STRONG), egui::StrokeKind::Inside);
    }
    let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
    painter.circle_filled(pos2(x, rect.center().y), 7.0, theme::lerp(INK_2, Color32::WHITE, t));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Botão secundário discreto.
pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(INK_2).font(theme::regular(13.0)))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, theme::HAIRLINE_STRONG))
            .corner_radius(CornerRadius::same(R_SM))
            .min_size(vec2(0.0, 32.0)),
    )
}

/// Botão só de ícone, com dica.
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon).font(theme::regular(14.0)).color(INK_2))
            .fill(Color32::TRANSPARENT)
            .frame_when_inactive(false)
            .corner_radius(CornerRadius::same(R_SM))
            .min_size(vec2(26.0, 26.0)),
    )
    .on_hover_text(tooltip)
}

/// Campo de busca com lupa e botão de limpar. Ctrl+F foca o campo e Esc limpa.
pub fn search_box(ui: &mut Ui, text: &mut String, id_salt: &str, hint: &str, tooltip: &str) {
    // Desconta a margem interna (2 × 10): sem isso, com pouco espaço, o campo invadia o
    // título à esquerda.
    let width = (ui.available_width() - 28.0).clamp(160.0, 340.0);
    Frame::NONE
        .fill(WELL)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(R_SM))
        .inner_margin(Margin::symmetric(10, 0))
        .show(ui, |ui| {
            ui.set_width(width);
            // Esquerda para a direita explícito: `horizontal` herdaria a direção da barra,
            // que é montada da direita para a esquerda, e a lupa e o × trocariam de lado.
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.set_height(30.0);
                ui.label(RichText::new(egui_phosphor::regular::MAGNIFYING_GLASS).font(theme::regular(14.0)).color(INK_3));
                let clear_width = if text.is_empty() { 0.0 } else { 26.0 };
                let edit = egui::TextEdit::singleline(text)
                    .id_salt(id_salt)
                    .hint_text(RichText::new(hint).color(INK_4))
                    .frame(Frame::NONE)
                    .font(theme::regular(13.0))
                    .desired_width(ui.available_width() - clear_width);
                let response = ui.add(edit);
                if ui.input(|i| i.modifiers.command && i.key_pressed(Key::F)) {
                    response.request_focus();
                }
                if response.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    text.clear();
                }
                if !text.is_empty() && icon_button(ui, egui_phosphor::regular::X, "Limpar busca").clicked() {
                    text.clear();
                }
            });
        })
        .response
        .on_hover_text(tooltip);
}

/// Texto alinhado à direita na célula, como os números da tabela.
pub fn right_text(ui: &mut Ui, text: &str, color: Color32) {
    let rect = ui.max_rect();
    ui.painter().text(pos2(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, text, theme::regular(12.5), color);
}

/// Pinta o fundo de calor no retângulo da célula e escreve o valor alinhado à direita.
pub fn heat_cell(ui: &mut Ui, text: &str, intensity: f32, strong: bool) {
    let rect = ui.max_rect();
    let fill = theme::heat_fill(intensity);
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect.shrink2(vec2(1.0, 1.0)), CornerRadius::same(3), fill);
    }
    let color = if strong || intensity > 0.5 { INK } else { INK_2 };
    ui.painter().text(
        pos2(rect.right() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        text,
        theme::regular(12.5),
        color,
    );
}
