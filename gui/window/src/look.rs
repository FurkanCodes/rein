//! rein's look: every colour the design names, light and dark, and what the
//! toolkit's own parts take from them. Nothing else in the window says a
//! colour, so changing one here changes it everywhere.

use gpui_kit::base::{TextViewDefaults, TextViewStyle};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::*;

use crate::parts::Monospace;

/// The tokens of `project/rein.css` on the design canvas, by the same names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Look {
    pub(crate) desk: Hsla,
    pub(crate) bg: Hsla,
    pub(crate) pane_2: Hsla,
    pub(crate) raised: Hsla,
    pub(crate) lit: Hsla,
    pub(crate) hover: Hsla,
    pub(crate) press: Hsla,
    pub(crate) float: Hsla,
    pub(crate) line: Hsla,
    pub(crate) line_2: Hsla,
    pub(crate) ring: Hsla,
    pub(crate) lip: Hsla,
    pub(crate) ink: Hsla,
    pub(crate) ink_2: Hsla,
    pub(crate) ink_3: Hsla,
    pub(crate) teal: Hsla,
    pub(crate) teal_text: Hsla,
    pub(crate) teal_tint: Hsla,
    pub(crate) teal_fill: Hsla,
    pub(crate) teal_hover: Hsla,
    pub(crate) teal_lip: Hsla,
    pub(crate) teal_sheen: Hsla,
    pub(crate) teal_seam: Hsla,
    pub(crate) on_teal: Hsla,
    pub(crate) amber: Hsla,
    pub(crate) amber_text: Hsla,
    pub(crate) amber_tint: Hsla,
    pub(crate) amber_line: Hsla,
    pub(crate) red: Hsla,
    pub(crate) band: Hsla,
    pub(crate) focus: Hsla,
    pub(crate) focus_line: Hsla,
    pub(crate) focus_halo: Hsla,
}

impl Global for Look {}

/// The two ends of the rein's gradient, which is the logo's.
pub(crate) const THREAD_FROM: u32 = 0x46dc78;
pub(crate) const THREAD_TO: u32 = 0x28a5c8;

fn solid(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn tint(hex: u32, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..solid(hex) }
}

impl Look {
    fn light() -> Look {
        const SHADE: u32 = 0x162622;
        const TEAL: u32 = 0x0e8f7e;
        const AMBER: u32 = 0xd97706;
        Look {
            desk: solid(0xedf0ee),
            bg: solid(0xffffff),
            pane_2: solid(0xfcfdfc),
            raised: solid(0xf5f6f6),
            lit: solid(0xe8ecea),
            hover: tint(SHADE, 0.055),
            press: tint(SHADE, 0.095),
            float: solid(0xffffff),
            line: tint(SHADE, 0.1),
            line_2: tint(SHADE, 0.18),
            ring: tint(SHADE, 0.15),
            lip: tint(SHADE, 0.24),
            ink: solid(0x191d1c),
            ink_2: solid(0x434b49),
            ink_3: solid(0x646c69),
            teal: solid(TEAL),
            teal_text: solid(0x0b7a6c),
            teal_tint: tint(TEAL, 0.1),
            teal_fill: solid(0x0f7f70),
            teal_hover: solid(0x0c6e61),
            teal_lip: solid(0x0a5a4f),
            teal_sheen: tint(0xffffff, 0.16),
            teal_seam: tint(0xffffff, 0.16),
            on_teal: solid(0xffffff),
            amber: solid(AMBER),
            amber_text: solid(0xa85605),
            amber_tint: tint(AMBER, 0.08),
            amber_line: tint(AMBER, 0.3),
            red: solid(0xc62f2f),
            band: solid(0xf4f6f5),
            focus: solid(TEAL),
            focus_line: tint(TEAL, 0.46),
            focus_halo: tint(TEAL, 0.08),
        }
    }

    fn dark() -> Look {
        const SHADE: u32 = 0xe2f0ec;
        const TEAL: u32 = 0x4fd1b5;
        const AMBER: u32 = 0xf2a33a;
        Look {
            desk: solid(0x101413),
            bg: solid(0x181c1b),
            pane_2: solid(0x161a19),
            raised: solid(0x212725),
            lit: solid(0x2b3230),
            hover: tint(SHADE, 0.07),
            press: tint(SHADE, 0.12),
            float: solid(0x232927),
            line: tint(SHADE, 0.1),
            line_2: tint(SHADE, 0.19),
            ring: tint(SHADE, 0.16),
            lip: tint(0x000000, 0.6),
            ink: solid(0xe9edeb),
            ink_2: solid(0xbcc4c1),
            ink_3: solid(0x939c99),
            teal: solid(TEAL),
            teal_text: solid(0x63dbc2),
            teal_tint: tint(TEAL, 0.14),
            teal_fill: solid(TEAL),
            teal_hover: solid(0x6bdcc4),
            teal_lip: solid(0x2f8f7c),
            teal_sheen: tint(0xffffff, 0.14),
            teal_seam: tint(0x06231d, 0.2),
            on_teal: solid(0x06231d),
            amber: solid(AMBER),
            amber_text: solid(0xf5b45a),
            amber_tint: tint(AMBER, 0.1),
            amber_line: tint(AMBER, 0.32),
            red: solid(0xf07a7a),
            band: solid(0x1f2423),
            focus: solid(TEAL),
            focus_line: tint(TEAL, 0.48),
            focus_halo: tint(TEAL, 0.09),
        }
    }

    pub(crate) fn of(cx: &App) -> Look {
        *cx.global::<Look>()
    }
}

/// Which look to wear: your Mac's, or the one a flag pinned for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Wearing {
    #[default]
    System,
    Light,
    Dark,
}

impl Wearing {
    pub(crate) fn dark(self, appearance: WindowAppearance) -> bool {
        match self {
            Wearing::System => matches!(appearance, WindowAppearance::Dark | WindowAppearance::VibrantDark),
            Wearing::Light => false,
            Wearing::Dark => true,
        }
    }
}

/// Puts on the light or the dark look, and has the toolkit's parts follow.
pub(crate) fn wear(dark: bool, cx: &mut App) {
    let look = if dark { Look::dark() } else { Look::light() };
    // This loads the toolkit's own colours, so ours go on in an update after it.
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    Theme::update(cx, |theme| {
        theme.background = look.bg;
        theme.foreground = look.ink;
        theme.muted = look.raised;
        theme.muted_foreground = look.ink_3;
        theme.border = look.line;
        theme.input = look.ring;
        theme.ring = look.focus;
        theme.caret = look.teal;
        theme.selection = look.teal_tint;
        theme.accent = look.hover;
        theme.accent_foreground = look.ink;
        theme.popover = look.float;
        theme.popover_foreground = look.ink;
        theme.primary = look.teal_fill;
        theme.primary_hover = look.teal_hover;
        theme.primary_active = look.teal_fill;
        theme.primary_foreground = look.on_teal;
        theme.radius = px(7.);
        theme.radius_lg = px(10.);
        theme.shadow = false;
        theme.font_size = px(13.);
    });
    let code = StyleRefinement::default().rounded(px(8.)).bg(look.raised);
    let prose = TextViewStyle::default()
        .with_dark(dark)
        .with_foreground(look.ink)
        .with_muted_foreground(look.ink_3)
        .with_link(look.teal_text)
        .with_selection(look.teal_tint)
        .with_code_background(look.raised)
        .with_border(look.line)
        // Rows make the gaps between blocks.
        .with_paragraph_gap(rems(0.))
        .with_code_block(code)
        .with_inline_code(HighlightStyle { background_color: Some(look.raised), ..HighlightStyle::default() });
    // Every window sets a colour at its root, and a quiet row its own.
    TextViewDefaults::new().with_style(prose).with_inherit_text_color(true).install(cx);
    let mono = cx.theme().mono_font_family.clone();
    cx.set_global(Monospace(mono));
    cx.set_global(look);
}
