//! The palette and the metrics, in one place.
//!
//! Both come from a design done against this window specifically (the
//! artboards are 1:1 with the implementation, so the numbers below are the
//! numbers there). Kept as data applied to `egui::Style` rather than sprinkled
//! through the drawing code: egui styles globally, and a colour picked at a
//! call site is a colour nobody can find later.

use egui::{Color32, CornerRadius, Stroke, Style, TextStyle, Visuals};

/// Design metrics, in points. egui works in points and the window is not
/// scaled, so these are the pixels in the artboards.
pub mod metric {
    pub const FONT_HEADING: f32 = 19.0;
    pub const FONT_BODY: f32 = 14.0;
    pub const FONT_SMALL: f32 = 12.0;
    pub const WIDGET_HEIGHT: f32 = 28.0;
    pub const PROGRESS_HEIGHT: f32 = 20.0;
    /// Between one bordered group and the next.
    pub const GAP_GROUPS: f32 = 14.0;
    /// Between rows inside a group.
    pub const GAP_IN_GROUP: f32 = 8.0;
    /// Between items on the same row.
    pub const GAP_ROW: f32 = 8.0;
    /// Between stacked notice lines, which are denser than ordinary rows.
    pub const GAP_NOTICES: f32 = 6.0;
    pub const WINDOW_MARGIN: f32 = 14.0;
    pub const GROUP_MARGIN: f32 = 10.0;
    pub const CORNER_RADIUS: u8 = 5;
    pub const CHECKBOX: f32 = 15.0;
    /// The top band holding the name and, one day, the dog -- see
    /// `crate::app::draw_header`.
    pub const HEADER_HEIGHT: f32 = 42.0;
    pub const SPRITE: [f32; 2] = [168.0, 42.0];
    pub const WINDOW_INITIAL: [f32; 2] = [440.0, 634.0];
    pub const WINDOW_MINIMUM: [f32; 2] = [400.0, 554.0];
}

/// One palette, in the roles the design names, so light and dark differ only
/// in their values and never in which colour does what.
pub struct Palette {
    pub window: Color32,
    pub panel: Color32,
    /// Text fields and the unfilled part of a progress bar.
    pub field: Color32,
    pub text: Color32,
    pub text_weak: Color32,
    pub text_disabled: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub warn: Color32,
    pub error: Color32,
    pub error_panel: Color32,
    pub border: Color32,
    /// Borders of things that can be clicked, which need more contrast than
    /// a plain divider.
    pub border_interactive: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const LIGHT: Palette = Palette {
    window: rgb(0xF2F1EE),
    panel: rgb(0xFBFAF8),
    field: rgb(0xE7E5E0),
    text: rgb(0x1E1D1B),
    text_weak: rgb(0x6E6B66),
    // The design specifies #A6A29B and states 3:1 for disabled text. That
    // value measures 2.44:1 against the panel it sits on, so it is darkened
    // to the lightest value that actually reaches 3:1 (3.04:1). Smallest
    // change that makes the design's own promise true; the dark theme's
    // #6A6E71 already passes at 3.10:1 and is untouched.
    text_disabled: rgb(0x94908A),
    accent: rgb(0x2B6E62),
    on_accent: rgb(0xFBFAF8),
    warn: rgb(0x8A5A00),
    error: rgb(0xA3281E),
    error_panel: rgb(0xF7E9E7),
    border: rgb(0xD8D5CF),
    border_interactive: rgb(0xA9A5A0),
};

pub const DARK: Palette = Palette {
    window: rgb(0x17181A),
    panel: rgb(0x202225),
    field: rgb(0x101113),
    text: rgb(0xE8E6E2),
    text_weak: rgb(0x97938C),
    text_disabled: rgb(0x6A6E71),
    accent: rgb(0x6FC3B3),
    on_accent: rgb(0x10201D),
    warn: rgb(0xE0A83C),
    error: rgb(0xE5786C),
    error_panel: rgb(0x2A1D1C),
    border: rgb(0x34373B),
    border_interactive: rgb(0x4E5257),
};

/// Nudges a colour toward white or black, for the hover and pressed states
/// the design specifies as +4% and -4% of luminance.
fn shift(color: Color32, percent: i16) -> Color32 {
    let adjust = |c: u8| -> u8 {
        let delta = (255.0 * percent as f32 / 100.0) as i16;
        (c as i16 + delta).clamp(0, 255) as u8
    };
    Color32::from_rgb(adjust(color.r()), adjust(color.g()), adjust(color.b()))
}

pub fn palette_for(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = palette_for(dark);
    let mut style = Style {
        visuals: if dark {
            Visuals::dark()
        } else {
            Visuals::light()
        },
        ..Default::default()
    };

    style.visuals.window_fill = p.window;
    style.visuals.panel_fill = p.window;
    style.visuals.extreme_bg_color = p.field;
    style.visuals.override_text_color = Some(p.text);
    style.visuals.warn_fg_color = p.warn;
    // egui would otherwise derive weak text by fading the body colour; the
    // design specifies its own value, which is what the contrast test checks.
    style.visuals.weak_text_color = Some(p.text_weak);
    // Disabled widgets are faded toward this rather than toward the panel, so
    // the design's disabled colour is what actually appears.
    style.visuals.widgets.noninteractive.weak_bg_fill = p.panel;
    style.visuals.error_fg_color = p.error;
    style.visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    style.visuals.selection.bg_fill = p.accent.linear_multiply(0.35);
    style.visuals.selection.stroke = Stroke::new(1.0_f32, p.text);
    style.visuals.hyperlink_color = p.accent;

    let radius = CornerRadius::same(metric::CORNER_RADIUS);
    style.visuals.window_corner_radius = radius;
    style.visuals.menu_corner_radius = radius;

    // Ordinary widgets, in the four states egui distinguishes. `inactive` is
    // the resting state of something clickable; `noninteractive` is a label
    // or a frame, which gets the plain divider border rather than the
    // stronger interactive one.
    for (widgets, fill, stroke_colour, text) in [
        (
            &mut style.visuals.widgets.noninteractive,
            p.panel,
            p.border,
            p.text,
        ),
        (
            &mut style.visuals.widgets.inactive,
            p.panel,
            p.border_interactive,
            p.text,
        ),
        (
            &mut style.visuals.widgets.hovered,
            shift(p.panel, 4),
            p.border_interactive,
            p.text,
        ),
        (
            &mut style.visuals.widgets.active,
            shift(p.panel, -4),
            p.border_interactive,
            p.text,
        ),
        (
            &mut style.visuals.widgets.open,
            p.panel,
            p.border_interactive,
            p.text,
        ),
    ] {
        widgets.bg_fill = fill;
        widgets.weak_bg_fill = fill;
        widgets.bg_stroke = Stroke::new(1.0_f32, stroke_colour);
        widgets.fg_stroke = Stroke::new(1.0_f32, text);
        widgets.corner_radius = radius;
    }

    style.spacing.item_spacing = egui::vec2(metric::GAP_ROW, metric::GAP_IN_GROUP);
    style.spacing.interact_size = egui::vec2(40.0, metric::WIDGET_HEIGHT);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.icon_width = metric::CHECKBOX;
    style.spacing.icon_width_inner = metric::CHECKBOX - 6.0;
    style.spacing.window_margin = egui::Margin::same(metric::WINDOW_MARGIN as i8);
    style.spacing.menu_margin = egui::Margin::same(metric::GROUP_MARGIN as i8);

    style.text_styles = [
        (
            TextStyle::Heading,
            egui::FontId::proportional(metric::FONT_HEADING),
        ),
        (
            TextStyle::Body,
            egui::FontId::proportional(metric::FONT_BODY),
        ),
        (
            TextStyle::Button,
            egui::FontId::proportional(metric::FONT_BODY),
        ),
        (
            TextStyle::Small,
            egui::FontId::proportional(metric::FONT_SMALL),
        ),
        // The fourth text style the design asks for. Free in egui, and it is
        // what makes /dev/disk4 distinguishable from /dev/diskl -- which the
        // confirmation dialog depends on, since the guard is exact-match.
        (
            TextStyle::Monospace,
            egui::FontId::monospace(metric::FONT_SMALL),
        ),
    ]
    .into();

    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-ASCII character the interface draws, so the check below can
    /// hold them against the fonts eframe actually ships. A missing glyph
    /// does not fail loudly -- it renders as a blank or a tofu box, on one
    /// platform and not the other, which is exactly the sort of thing that
    /// escapes review.
    ///
    /// `✓` (U+2713) and `▾` (U+25BE) are deliberately absent: neither has a
    /// glyph in the bundled fonts, and both were in the design. `✔` (U+2714)
    /// replaces the first; the second is not needed at all, because egui's
    /// `ComboBox` draws its own arrow.
    const GLYPHS_USED: &str = "⟳✔⚠·…—";

    /// Relative luminance, per WCAG, for the contrast check below.
    fn luminance(c: Color32) -> f32 {
        let channel = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The design states 4.5:1 for every text colour except the disabled one,
    /// which is deliberately 3:1 and never the only signal. Checked rather
    /// than trusted: this is the kind of promise that quietly stops being
    /// true when someone tweaks one hex value.
    #[test]
    fn every_text_colour_meets_the_contrast_the_design_promises() {
        for (name, p) in [("light", &LIGHT), ("dark", &DARK)] {
            for (role, colour, background) in [
                ("text", p.text, p.window),
                ("text on panel", p.text, p.panel),
                ("text_weak", p.text_weak, p.panel),
                ("warn", p.warn, p.window),
                ("error", p.error, p.window),
                ("error on its panel", p.error, p.error_panel),
                ("on_accent", p.on_accent, p.accent),
            ] {
                let ratio = contrast(colour, background);
                assert!(
                    ratio >= 4.5,
                    "{name}/{role}: {ratio:.2}:1 is below the 4.5:1 the design promises"
                );
            }
            let disabled = contrast(p.text_disabled, p.panel);
            assert!(
                disabled >= 3.0,
                "{name}/disabled: {disabled:.2}:1 is below 3:1"
            );
        }
    }

    /// The cross-platform requirement, checked rather than assumed: every
    /// glyph the interface draws must be renderable with the fonts eframe
    /// already bundles, so the design needs **no font imported and no font
    /// licence beyond what is already vendored by epaint_default_fonts**
    /// (Ubuntu-Light under UFL, Hack under MIT, Noto Emoji under OFL, and
    /// the emoji-icon font under MIT -- all permissive, all compatible with
    /// this project's MIT OR Apache-2.0).
    ///
    /// Without this, a symbol that resolves on macOS's font stack and not on
    /// a minimal Linux install shows up as an empty box for the user and
    /// never for the developer.
    #[test]
    fn every_glyph_the_ui_draws_is_renderable_with_the_bundled_fonts() {
        let ctx = egui::Context::default();
        // Force the default font set to be built, exactly as at runtime.
        ctx.set_fonts(egui::FontDefinitions::default());
        let _ = ctx.run(Default::default(), |_| {});

        let mut missing = Vec::new();
        for style in [
            TextStyle::Body,
            TextStyle::Small,
            TextStyle::Button,
            TextStyle::Monospace,
        ] {
            let font_id = style.resolve(&ctx.style());
            for glyph in GLYPHS_USED.chars() {
                if !ctx.fonts_mut(|f| f.has_glyph(&font_id, glyph)) {
                    missing.push(format!("{glyph:?} in {style:?}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these would render as blank boxes: {}",
            missing.join(", ")
        );
    }

    #[test]
    fn hover_and_pressed_move_in_opposite_directions() {
        let base = LIGHT.panel;
        assert!(luminance(shift(base, 4)) > luminance(base));
        assert!(luminance(shift(base, -4)) < luminance(base));
    }

    /// Shifting a colour that is already at an extreme must not wrap around.
    #[test]
    fn shifting_clamps_rather_than_wraps() {
        assert_eq!(shift(Color32::WHITE, 10), Color32::WHITE);
        assert_eq!(shift(Color32::BLACK, -10), Color32::BLACK);
    }
}
