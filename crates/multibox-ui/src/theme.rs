//! The window's colours, in one place.
//!
//! The chrome is neutral: warm paper and ink, with the buttons in ink too.
//! Colour is kept for what carries meaning. One blue marks the caches a
//! worktree holds, and its ghost, outlined in the same blue, marks cache data
//! shared with other worktrees, which takes no room of its own. Green, amber
//! and red say merged, uncommitted and gone, always beside an icon and words.
//!
//! Neighbours in a bar were checked to stay apart for colour-blind readers
//! (at least 22 in OKLab ×100 under protanopia, deuteranopia and tritanopia,
//! 23 or more for full colour vision) and text against both surfaces (4.5:1
//! or better). The ghost alone falls short against the surface, which is why
//! it carries the outline and is never the only way a value is told.

use gpui_kit::component::{ActiveTheme, Theme, ThemeConfig, ThemeMode};
use gpui_kit::{App, Hsla, SharedString, Window, font, px, rgb};
use std::rc::Rc;

struct Tones {
    canvas: u32,
    /// The inspector, a step back from the canvas.
    panel: u32,
    /// Dialogs, menus and tooltips, a step forward.
    raised: u32,
    ink: u32,
    ink2: u32,
    ink3: u32,
    hairline: u32,
    /// The edge of a control, a little firmer than a hairline.
    edge: u32,
    hover: u32,
    selected: u32,
    source: u32,
    cache: u32,
    shared: u32,
    git: u32,
    success: u32,
    warning: u32,
    danger: u32,
}

const LIGHT: Tones = Tones {
    canvas: 0xFCFBF9,
    panel: 0xF5F3EF,
    raised: 0xFFFFFF,
    ink: 0x1B1A18,
    ink2: 0x55524D,
    ink3: 0x716D66,
    hairline: 0xE5E2DC,
    edge: 0xD8D4CD,
    hover: 0xF3F1ED,
    selected: 0xECE9E3,
    source: 0x23211E,
    cache: 0x2F6BDE,
    shared: 0xC3D4F5,
    git: 0x8A857D,
    success: 0x1F8A57,
    warning: 0xB4700A,
    danger: 0xCC3D3D,
};

const DARK: Tones = Tones {
    canvas: 0x151413,
    panel: 0x1B1A18,
    raised: 0x21201E,
    ink: 0xEEECE8,
    ink2: 0xAFABA5,
    ink3: 0x8B8780,
    hairline: 0x2B2927,
    edge: 0x3A3835,
    hover: 0x1E1D1B,
    selected: 0x282624,
    source: 0xE9E6E1,
    cache: 0x5B8DEF,
    shared: 0x283650,
    git: 0x77736C,
    success: 0x4CC38A,
    warning: 0xE5A63A,
    danger: 0xF0625F,
};

fn tones(dark: bool) -> &'static Tones {
    if dark { &DARK } else { &LIGHT }
}

fn hex(color: u32) -> String {
    format!("#{color:06x}")
}

/// The same tones as the component library names them, so that its buttons,
/// dialogs, inputs and tooltips are cut from the same cloth.
fn config(dark: bool) -> ThemeConfig {
    let t = tones(dark);
    let (primary_hover, primary_active, on_danger, overlay) = if dark {
        ("#ffffff", "#d9d6d1", hex(t.canvas), "#00000080")
    } else {
        ("#34322e", "#000000", "#ffffff".to_owned(), "#1b1a1833")
    };
    let colors: serde_json::Map<String, serde_json::Value> = [
        ("background", hex(t.canvas)),
        ("foreground", hex(t.ink)),
        ("border", hex(t.hairline)),
        ("input.border", hex(t.edge)),
        ("ring", hex(t.cache)),
        ("caret", hex(t.ink)),
        ("muted.background", hex(t.hover)),
        ("muted.foreground", hex(t.ink2)),
        ("accent.background", hex(t.selected)),
        ("accent.foreground", hex(t.ink)),
        ("primary.background", hex(t.ink)),
        ("primary.hover.background", primary_hover.to_owned()),
        ("primary.active.background", primary_active.to_owned()),
        ("primary.foreground", hex(t.canvas)),
        ("secondary.background", hex(t.hover)),
        ("secondary.hover.background", hex(t.selected)),
        ("secondary.active.background", hex(t.edge)),
        ("secondary.foreground", hex(t.ink)),
        ("danger.background", hex(t.danger)),
        ("danger.foreground", on_danger),
        ("success.background", hex(t.success)),
        ("success.foreground", hex(t.canvas)),
        ("warning.background", hex(t.warning)),
        ("warning.foreground", hex(t.canvas)),
        ("info.background", hex(t.cache)),
        ("info.foreground", hex(t.canvas)),
        ("link.foreground", hex(t.cache)),
        ("popover.background", hex(t.raised)),
        ("popover.foreground", hex(t.ink)),
        ("progress_bar.background", hex(t.ink)),
        ("list.hover.background", hex(t.hover)),
        ("scrollbar.thumb.background", format!("{}80", hex(t.ink3))),
        ("scrollbar.thumb.hover.background", hex(t.ink3)),
        ("selection.background", format!("{}59", hex(t.cache))),
        ("title_bar.background", hex(t.canvas)),
        ("title_bar.border", hex(t.hairline)),
        ("overlay", overlay.to_owned()),
    ]
    .into_iter()
    .map(|(name, color)| (name.to_owned(), color.into()))
    .collect();
    let value = serde_json::json!({
        "name": if dark { "Multibox Dark" } else { "Multibox Light" },
        "mode": if dark { "dark" } else { "light" },
        "font.size": 14,
        "radius": 6,
        "radius.lg": 10,
        "colors": colors,
    });
    serde_json::from_value(value).expect("the built-in theme is well formed")
}

/// Registers both themes and shows the one matching the system.
pub fn install(window: &mut Window, cx: &mut App) {
    for dark in [false, true] {
        Theme::global_mut(cx).apply_config(&Rc::new(config(dark)));
    }
    Theme::sync_system_appearance(Some(window), cx);
}

pub fn toggle(window: &mut Window, cx: &mut App) {
    let mode = if cx.theme().mode.is_dark() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    Theme::change(mode, Some(window), cx);
}

/// The tones of the current mode, ready to paint with.
#[derive(Clone)]
pub struct Palette {
    pub canvas: Hsla,
    pub panel: Hsla,
    pub ink: Hsla,
    pub ink2: Hsla,
    pub ink3: Hsla,
    pub hairline: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub source: Hsla,
    pub cache: Hsla,
    pub shared: Hsla,
    pub git: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub mono: SharedString,
    /// How far above the bottom of a line box exactly one font size high the
    /// baseline sits, per point of size. See `run` in the window's drawing code.
    pub baseline: f32,
    pub dark: bool,
}

impl Palette {
    pub fn current(cx: &App) -> Palette {
        let theme = cx.theme();
        let dark = theme.mode.is_dark();
        let t = tones(dark);
        let paint = |color: u32| -> Hsla { rgb(color).into() };
        // GPUI centres the glyphs' ascent and descent in the line box, so in a
        // box one size high the baseline is (1 - ascent + descent) / 2 sizes
        // above its bottom. The proportions are the UI font's own.
        let text = cx.text_system();
        let id = text.resolve_font(&font(theme.font_family.clone()));
        let size = px(100.);
        let ascent = (text.ascent(id, size) / size).abs();
        let descent = (text.descent(id, size) / size).abs();
        let baseline = ((1. - ascent + descent) / 2.).max(0.);
        Palette {
            canvas: paint(t.canvas),
            panel: paint(t.panel),
            ink: paint(t.ink),
            ink2: paint(t.ink2),
            ink3: paint(t.ink3),
            hairline: paint(t.hairline),
            hover: paint(t.hover),
            selected: paint(t.selected),
            source: paint(t.source),
            cache: paint(t.cache),
            shared: paint(t.shared),
            git: paint(t.git),
            success: paint(t.success),
            warning: paint(t.warning),
            danger: paint(t.danger),
            mono: theme.mono_font_family.clone(),
            baseline,
            dark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_themes_parse_and_keep_their_mode() {
        assert!(!config(false).mode.is_dark());
        assert!(config(true).mode.is_dark());
        assert_eq!(config(false).font_size, Some(14.0));
    }
}
