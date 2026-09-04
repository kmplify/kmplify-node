//! The window's look: the Tailux "cinder" dark scheme the rest of KMPLIFY
//! draws in, with the marketplace's primary blue, and one colour per
//! measurement that stays the same everywhere it appears — chart, ring,
//! legend — so a reader learns four colours once.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};

// Surfaces, darkest to lightest (cinder 900 → 450).
pub const BG: Color32 = Color32::from_rgb(0x0e, 0x0f, 0x11);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x16, 0x1a);
pub const CARD: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x21);
pub const CARD_RAISED: Color32 = Color32::from_rgb(0x23, 0x24, 0x29);
pub const BORDER: Color32 = Color32::from_rgb(0x2a, 0x2c, 0x32);
pub const BORDER_STRONG: Color32 = Color32::from_rgb(0x38, 0x3a, 0x41);

// Text.
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe7, 0xeb);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x83, 0x87, 0x94);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x4c, 0x4f, 0x57);

// Brand and feedback (Tailwind tokens the marketplace theme maps primary to).
pub const PRIMARY: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
pub const PRIMARY_STRONG: Color32 = Color32::from_rgb(0x25, 0x63, 0xeb);
pub const OK: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
pub const WARN: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
pub const ERR: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);

// One colour per measurement. GPU is the brand blue because the card is
// what this product is about; the other three are kept far apart in hue.
pub const GPU: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
pub const VRAM: Color32 = Color32::from_rgb(0x14, 0xb8, 0xa6);
pub const CPU: Color32 = Color32::from_rgb(0xa8, 0x55, 0xf7);
pub const RAM: Color32 = Color32::from_rgb(0xec, 0x48, 0x99);

pub const RADIUS: CornerRadius = CornerRadius::same(10);
pub const RADIUS_SM: CornerRadius = CornerRadius::same(6);

pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// Install the palette on the context. Called once from the creation
/// callback; egui keeps it for the life of the window.
pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = PANEL;
    v.faint_bg_color = CARD;
    v.code_bg_color = CARD_RAISED;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = PRIMARY;
    v.selection.bg_fill = with_alpha(PRIMARY, 0x60);
    v.selection.stroke = Stroke::new(1.0, PRIMARY);
    v.window_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.window_corner_radius = RADIUS;
    v.menu_corner_radius = RADIUS_SM;

    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.weak_bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_MUTED);
    v.widgets.noninteractive.corner_radius = RADIUS_SM;

    v.widgets.inactive.bg_fill = CARD_RAISED;
    v.widgets.inactive.weak_bg_fill = CARD_RAISED;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.corner_radius = RADIUS_SM;

    v.widgets.hovered.bg_fill = BORDER;
    v.widgets.hovered.weak_bg_fill = BORDER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.corner_radius = RADIUS_SM;

    v.widgets.active.bg_fill = PRIMARY_STRONG;
    v.widgets.active.weak_bg_fill = PRIMARY_STRONG;
    v.widgets.active.bg_stroke = Stroke::new(1.0, PRIMARY);
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.corner_radius = RADIUS_SM;

    v.widgets.open.bg_fill = CARD_RAISED;
    v.widgets.open.weak_bg_fill = CARD_RAISED;
    v.widgets.open.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.widgets.open.corner_radius = RADIUS_SM;

    ctx.set_visuals(v);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.window_margin = egui::Margin::same(16);
        style.spacing.slider_width = 220.0;
    });
}

/// A card: the surface every node, job and settings group sits on.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(RADIUS)
        .inner_margin(egui::Margin::same(14))
}

/// A quieter card for finished work in the jobs column.
pub fn card_low() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(RADIUS_SM)
        .inner_margin(egui::Margin::same(10))
}

/// A small rounded label: LOCAL, a job state, an engine.
pub fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    let frame = egui::Frame::new()
        .fill(with_alpha(color, 0x2a))
        .stroke(Stroke::new(1.0, with_alpha(color, 0x80)))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 1));
    frame.show(ui, |ui| {
        ui.label(egui::RichText::new(text).size(10.5).color(color).strong());
    });
}

pub fn muted(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .color(TEXT_MUTED)
        .size(12.0)
}

pub fn dim(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).color(TEXT_DIM).size(12.0)
}

pub fn heading(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .size(15.0)
        .strong()
        .color(TEXT)
}
