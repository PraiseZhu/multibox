//! The window's colours, in one place.
//!
//! The chrome is neutral: white cards on a warm grey ground in light mode,
//! graphite in dark, with the buttons in ink. Colour is kept for what carries
//! meaning. One lapis blue, deeper and quieter than the usual interface blue,
//! marks the caches a
//! worktree holds, and its ghost, outlined in the same blue, marks cache data
//! shared with other worktrees, which takes no room of its own. Green, amber
//! and red say merged, uncommitted and gone, always beside an icon and words.
//!
//! Neighbours in a bar were checked to stay apart for colour-blind readers
//! (at least 20 in OKLab ×100 under protanopia, deuteranopia and tritanopia,
//! as for full colour vision) and text against the ground and the cards
//! (4.5:1 or better). The ghost alone falls short against the surface, which is why
//! it carries the outline and is never the only way a value is told.

use gpui_kit::component::{ActiveTheme, Theme, ThemeConfig, ThemeMode};
use gpui_kit::{App, Hsla, SharedString, Window, font, px, rgb};
use std::rc::Rc;

struct Tones {
    /// The ground the cards sit on.
    canvas: u32,
    /// Cards, the top bar and the inspector.
    card: u32,
    /// The inspector.
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
    canvas: 0xF5F4F1,
    card: 0xFFFFFF,
    panel: 0xFFFFFF,
    raised: 0xFFFFFF,
    ink: 0x1A1918,
    ink2: 0x53514D,
    ink3: 0x6C6A65,
    hairline: 0xE8E6E1,
    edge: 0xD9D6D0,
    hover: 0xF5F4F1,
    selected: 0xEFEEEA,
    source: 0x22211F,
    cache: 0x2C5C9E,
    shared: 0xCBD8EA,
    git: 0x9B978F,
    success: 0x22855A,
    warning: 0xA9690C,
    danger: 0xC43F3B,
};

const DARK: Tones = Tones {
    canvas: 0x121213,
    card: 0x1A1A1C,
    panel: 0x1A1A1C,
    raised: 0x222225,
    ink: 0xEDEDEB,
    ink2: 0xAAA9A5,
    ink3: 0x86857F,
    hairline: 0x29292C,
    edge: 0x38383C,
    hover: 0x1F1F22,
    selected: 0x252528,
    source: 0xDCDBD7,
    cache: 0x6F9AD6,
    shared: 0x24324A,
    git: 0x6B6A66,
    success: 0x4FBF8B,
    warning: 0xDDA341,
    danger: 0xEC6A62,
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
        ("#ffffff", "#d6d6d3", hex(t.canvas), "#00000080")
    } else {
        ("#33322f", "#000000", "#ffffff".to_owned(), "#1a191833")
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
        ("title_bar.background", hex(t.card)),
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
    pub card: Hsla,
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
            card: paint(t.card),
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
