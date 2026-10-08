//! Tokens visuais e configuração do egui.
//!
//! Direção: instrumento de telemetria em grafite. As superfícies sobem poucos pontos
//! de luminosidade a cada nível, as bordas são linhas finas translúcidas e a cor fica
//! reservada para dado: cada recurso tem a sua, e a carga aparece como calor âmbar.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle};

// Superfícies (grafite, do fundo para cima). O poço fica abaixo do fundo: campos recebem conteúdo.
pub const WELL: Color32 = Color32::from_rgb(0x0A, 0x0B, 0x0D);
pub const GRAPHITE_0: Color32 = Color32::from_rgb(0x0E, 0x10, 0x13);
pub const GRAPHITE_1: Color32 = Color32::from_rgb(0x14, 0x17, 0x1B);
pub const GRAPHITE_2: Color32 = Color32::from_rgb(0x1A, 0x1E, 0x23);
pub const GRAPHITE_3: Color32 = Color32::from_rgb(0x22, 0x27, 0x2D);

// Linhas finas: somem quando não se procura por elas.
pub const HAIRLINE: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);
pub const HAIRLINE_STRONG: Color32 = Color32::from_rgba_premultiplied(32, 32, 32, 32);

// Texto em quatro níveis.
pub const INK: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEB);
pub const INK_2: Color32 = Color32::from_rgb(0xA8, 0xAE, 0xB7);
pub const INK_3: Color32 = Color32::from_rgb(0x76, 0x7D, 0x87);
pub const INK_4: Color32 = Color32::from_rgb(0x4E, 0x54, 0x5C);

// Uma cor por recurso, a mesma em toda a interface.
pub const CPU: Color32 = Color32::from_rgb(0x4C, 0xC2, 0xFF);
pub const MEMORY: Color32 = Color32::from_rgb(0xB4, 0x8C, 0xFF);
pub const DISK: Color32 = Color32::from_rgb(0xA3, 0xE3, 0x5A);
pub const NETWORK: Color32 = Color32::from_rgb(0x2D, 0xD4, 0xBF);
pub const GPU: Color32 = Color32::from_rgb(0xFF, 0x7E, 0xB6);

/// Calor: quanto mais carga, mais âmbar.
pub const HEAT: Color32 = Color32::from_rgb(0xFF, 0xB3, 0x47);
pub const DANGER: Color32 = Color32::from_rgb(0xF2, 0x54, 0x5B);
pub const OK: Color32 = Color32::from_rgb(0x7B, 0xD8, 0x8F);

pub const R_SM: u8 = 6;
pub const R_MD: u8 = 10;
pub const R_LG: u8 = 14;

pub const SEMIBOLD: &str = "semibold";

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.text_styles = [
            (TextStyle::Small, regular(11.5)),
            (TextStyle::Body, regular(13.5)),
            (TextStyle::Button, regular(13.5)),
            (TextStyle::Monospace, mono(12.5)),
            (TextStyle::Heading, semibold(20.0)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 6.0);
        s.button_padding = egui::vec2(10.0, 4.0);
        s.interact_size = egui::vec2(40.0, 28.0);
        s.window_margin = Margin::same(16);
        s.menu_margin = Margin::same(6);
        s.icon_spacing = 6.0;

        style.interaction.selectable_labels = false;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = GRAPHITE_0;
        v.window_fill = GRAPHITE_2;
        v.window_stroke = Stroke::new(1.0, HAIRLINE_STRONG);
        v.window_corner_radius = CornerRadius::same(R_LG);
        v.menu_corner_radius = CornerRadius::same(R_MD);
        v.window_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(110) };
        v.popup_shadow = Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(90) };
        v.extreme_bg_color = WELL;
        v.faint_bg_color = GRAPHITE_1;
        v.code_bg_color = WELL;
        v.warn_fg_color = HEAT;
        v.error_fg_color = DANGER;
        v.hyperlink_color = CPU;
        v.selection.bg_fill = CPU.gamma_multiply(0.18);
        v.selection.stroke = Stroke::new(1.0, CPU);
        v.striped = false;

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = GRAPHITE_1;
        w.noninteractive.weak_bg_fill = GRAPHITE_1;
        w.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, INK);
        w.noninteractive.corner_radius = CornerRadius::same(R_SM);

        w.inactive.bg_fill = GRAPHITE_2;
        w.inactive.weak_bg_fill = GRAPHITE_2;
        w.inactive.bg_stroke = Stroke::new(1.0, HAIRLINE);
        w.inactive.fg_stroke = Stroke::new(1.0, INK);
        w.inactive.corner_radius = CornerRadius::same(R_SM);

        // Também é o fundo da linha sob o mouse na tabela.
        w.hovered.bg_fill = GRAPHITE_2;
        w.hovered.weak_bg_fill = GRAPHITE_3;
        w.hovered.bg_stroke = Stroke::new(1.0, HAIRLINE_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.hovered.corner_radius = CornerRadius::same(R_SM);
        w.hovered.expansion = 0.0;

        w.active.bg_fill = GRAPHITE_3;
        w.active.weak_bg_fill = GRAPHITE_3;
        w.active.bg_stroke = Stroke::new(1.0, HAIRLINE_STRONG);
        w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        w.active.corner_radius = CornerRadius::same(R_SM);
        w.active.expansion = 0.0;

        w.open = w.active;
    });
}

/// Segoe UI e Cascadia Mono vêm do próprio Windows. Se faltarem, o egui usa as fontes
/// embutidas dele, e os ícones Phosphor entram como fallback das duas famílias.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono_defaults = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();

    let load = |fonts: &mut FontDefinitions, key: &str, files: &[&str]| -> bool {
        for file in files {
            if let Ok(bytes) = std::fs::read(format!(r"C:\Windows\Fonts\{file}")) {
                fonts.font_data.insert(key.to_owned(), FontData::from_owned(bytes).into());
                return true;
            }
        }
        false
    };

    let has_regular = load(&mut fonts, "segoe", &["segoeui.ttf"]);
    let has_semibold = load(&mut fonts, "segoe-sb", &["seguisb.ttf", "segoeuib.ttf"]);
    let has_mono = load(&mut fonts, "cascadia", &["CascadiaMono.ttf", "consola.ttf"]);

    let mut proportional = defaults.clone();
    if has_regular {
        proportional.insert(0, "segoe".to_owned());
    }
    fonts.families.insert(FontFamily::Proportional, proportional.clone());

    let mut semibold = proportional;
    if has_semibold {
        semibold.insert(0, "segoe-sb".to_owned());
    }
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold);

    let mut monospace = mono_defaults;
    if has_mono {
        monospace.insert(0, "cascadia".to_owned());
    }
    fonts.families.insert(FontFamily::Monospace, monospace);

    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    // add_to_fonts só cobre a família proporcional; a seminegrito também mostra ícones.
    if let Some(family) = fonts.families.get_mut(&FontFamily::Name(SEMIBOLD.into())) {
        family.push("phosphor".to_owned());
    }
    fonts
}

/// Cor de fundo de uma célula conforme a carga (0–1). Abaixo de 2% não pinta nada.
pub fn heat_fill(intensity: f32) -> Color32 {
    let t = intensity.clamp(0.0, 1.0);
    if t < 0.03 {
        return Color32::TRANSPARENT;
    }
    // Linear e começando baixo: cargas pequenas mal aparecem, só as grandes chamam atenção.
    let alpha = 0.04 + 0.44 * t;
    let color = if t > 0.75 { lerp(HEAT, DANGER, (t - 0.75) / 0.25) } else { HEAT };
    color.gamma_multiply(alpha)
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}
