//! The window's look: the KMPLIFY tokens the web dashboard draws with
//! (`src/dashboard/assets/app.css`), light and dark, following the system,
//! and Inter, the KMPLIFY brand face.
//!
//! egui's own default fonts are not compiled in (the `default_fonts`
//! feature is off): they carry the Ubuntu Font Licence, which this
//! repository does not ship. Inter is the only face, for proportional and
//! monospace text alike.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Theme, Visuals,
};

/// Inter Regular and SemiBold, Latin, as TrueType: instances of the
/// variable `inter-latin-wght.woff2` the web dashboard ships (wght 400 and
/// 600), converted to the format egui reads. SIL Open Font License 1.1,
/// `fonts/OFL.txt`; Inter has no Reserved Font Name.
pub const INTER_REGULAR: &[u8] = include_bytes!("fonts/Inter-Regular-latin.ttf");
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("fonts/Inter-SemiBold-latin.ttf");
pub const OFL: &str = include_str!("fonts/OFL.txt");

/// The family strong text is set in.
pub fn semibold() -> FontFamily {
    FontFamily::Name("inter-semibold".into())
}

/// Inter for every family, and nothing else.
pub fn fonts() -> FontDefinitions {
    let mut f = FontDefinitions::empty();
    f.font_data.insert(
        "inter".into(),
        std::sync::Arc::new(FontData::from_static(INTER_REGULAR)),
    );
    f.font_data.insert(
        "inter-semibold".into(),
        std::sync::Arc::new(FontData::from_static(INTER_SEMIBOLD)),
    );
    f.families
        .insert(FontFamily::Proportional, vec!["inter".into()]);
    f.families
        .insert(FontFamily::Monospace, vec!["inter".into()]);
    f.families
        .insert(semibold(), vec!["inter-semibold".into(), "inter".into()]);
    f
}

/// One scheme's tokens, named as in app.css.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub ink: Color32,
    pub head: Color32,
    pub muted: Color32,
    pub line: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_text: Color32,
    pub ok: Color32,
    pub bad: Color32,
    pub warn: Color32,
    pub track: Color32,
    pub code_bg: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const LIGHT: Palette = Palette {
    bg: rgb(0xf8fafc),
    panel: rgb(0xffffff),
    card: rgb(0xffffff),
    ink: rgb(0x334155),
    head: rgb(0x1e293b),
    muted: rgb(0x64748b),
    line: rgb(0xe2e8f0),
    accent: rgb(0x2563eb),
    accent_hover: rgb(0x1d4ed8),
    accent_text: rgb(0x2563eb),
    ok: rgb(0x047857),
    bad: rgb(0xc2410c),
    warn: rgb(0xa35200),
    track: rgb(0xe2e8f0),
    code_bg: rgb(0xf1f5f9),
};

pub const DARK: Palette = Palette {
    bg: rgb(0x0a1014),
    panel: rgb(0x0a1014),
    card: rgb(0x0d161c),
    ink: rgb(0xa0abb6),
    head: rgb(0xe1e5ea),
    muted: rgb(0x8796a1),
    line: rgb(0x242f38),
    accent: rgb(0x2563eb),
    accent_hover: rgb(0x3b82f6),
    accent_text: rgb(0x60a5fa),
    ok: rgb(0x34d399),
    bad: rgb(0xff8a5c),
    warn: rgb(0xffba42),
    track: rgb(0x242f38),
    code_bg: rgb(0x111b22),
};

/// The palette of the scheme on screen now.
pub fn palette(ctx: &egui::Context) -> Palette {
    if ctx.theme() == Theme::Dark {
        DARK
    } else {
        LIGHT
    }
}

pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub const RADIUS: CornerRadius = CornerRadius::same(8);

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.extreme_bg_color = p.code_bg;
    v.faint_bg_color = p.card;
    v.code_bg_color = p.code_bg;
    v.override_text_color = Some(p.ink);
    v.hyperlink_color = p.accent_text;
    v.selection.bg_fill = with_alpha(p.accent, 0x50);
    v.selection.stroke = Stroke::new(1.0, p.accent_text);
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = RADIUS;
    let w = &mut v.widgets;
    for (state, fill, stroke) in [
        (&mut w.noninteractive, p.card, p.line),
        (&mut w.inactive, p.card, p.line),
        (&mut w.hovered, with_alpha(p.muted, 0x22), p.muted),
        (&mut w.active, with_alpha(p.accent, 0x30), p.accent),
        (&mut w.open, p.card, p.line),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0, stroke);
        state.corner_radius = CornerRadius::same(6);
    }
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.ink);
    w.inactive.fg_stroke = Stroke::new(1.0, p.head);
    w.hovered.fg_stroke = Stroke::new(1.0, p.head);
    w.active.fg_stroke = Stroke::new(1.0, p.head);
    v
}

/// Install fonts, both schemes and the spacing, once, at creation. The
/// window follows the system's light or dark setting, like the web page.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.set_theme(egui::ThemePreference::System);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(13.5, FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(13.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(11.5, FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(12.0, FontFamily::Monospace),
        );
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(18.0, semibold()),
        );
    });
}

/// Characters the window's strings use that the Latin subset of Inter
/// lacks, and what is drawn instead, so no box ever stands in for a mark.
pub fn glyph_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2713}' | '\u{2714}' => out.push_str("ok"),
            '\u{2715}' | '\u{2717}' | '\u{2716}' => out.push('x'),
            '\u{2192}' => out.push_str("->"),
            '\u{2190}' => out.push_str("<-"),
            '\u{25cf}' | '\u{25cb}' => out.push('\u{2022}'),
            '\u{2265}' => out.push_str(">="),
            '\u{2264}' => out.push_str("<="),
            '\u{2248}' => out.push('~'),
            '\u{26a0}' => out.push('!'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fonts_are_inter_only() {
        let f = fonts();
        assert_eq!(f.font_data.len(), 2);
        for family in f.families.values() {
            assert!(family.iter().all(|n| n.starts_with("inter")), "{family:?}");
        }
        // TrueType, not WOFF2 (egui cannot read the latter).
        for bytes in [INTER_REGULAR, INTER_SEMIBOLD] {
            assert_eq!(&bytes[..4], &[0, 1, 0, 0]);
        }
        assert!(OFL.contains("SIL OPEN FONT LICENSE"));
    }

    #[test]
    fn marks_outside_the_subset_are_spelled_out() {
        assert_eq!(glyph_safe("\u{2713} done \u{2192} next"), "ok done -> next");
        assert_eq!(glyph_safe("plain · text…"), "plain · text…");
    }
}
