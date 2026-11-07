//! The small parts every screen shares: the one button, and what's drawn
//! rather than written: gauges, the strip of ticks, the working mark.

use std::f32::consts::TAU;

use gpui_kit::base::Button;
use gpui_kit::component::Selectable;
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Reach, StepState, Tick};

use crate::look::{Look, THREAD_FROM, THREAD_TO};

/// What a button is for, which is only its colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    Plain,
    Primary,
    Ghost,
}

/// A button's shape. A cell fills its place in the bar under the box: the
/// rule beside it is its edge, and it doesn't sink, as the bar doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Natural,
    Square,
    Wide,
    Cell,
    CellAtEnd,
}

impl Shape {
    const fn is_cell(self) -> bool {
        matches!(self, Shape::Cell | Shape::CellAtEnd)
    }
}

/// The one button. Variant, size and shape vary its colours and measure, so
/// its height, type, states and motion are written once, here. The toolkit's
/// [`Button`] underneath is unstyled and keeps focus, keys and accessibility.
#[derive(IntoElement)]
pub(crate) struct Press {
    base: Button,
    variant: Variant,
    shape: Shape,
    small: bool,
    on: bool,
    label: Option<SharedString>,
    key: Option<SharedString>,
    width: Option<Pixels>,
    children: Vec<AnyElement>,
}

/// The colours of one variant.
struct Paint {
    fill: Hsla,
    hover: Hsla,
    pressed: Hsla,
    text: Hsla,
    text_over: Hsla,
    key: Hsla,
    key_line: Hsla,
    /// What's drawn around it at rest, and while it's pressed down.
    rest: Vec<BoxShadow>,
    down: Vec<BoxShadow>,
}

fn paint(variant: Variant, look: &Look) -> Paint {
    match variant {
        Variant::Plain => Paint {
            fill: look.bg,
            hover: look.raised,
            pressed: look.lit,
            text: look.ink,
            text_over: look.ink,
            key: look.ink_3,
            key_line: look.line,
            rest: vec![hairline(look.ring), lip(look.lip)],
            down: vec![hairline(look.ring)],
        },
        Variant::Primary => Paint {
            fill: look.teal_fill,
            hover: look.teal_hover,
            pressed: look.teal_fill,
            text: look.on_teal,
            text_over: look.on_teal,
            key: look.on_teal,
            key_line: look.teal_seam,
            rest: vec![sheen(look.teal_sheen), lip(look.teal_lip)],
            down: Vec::new(),
        },
        Variant::Ghost => Paint {
            fill: gpui_kit::transparent_black(),
            hover: look.hover,
            pressed: look.press,
            text: look.ink_2,
            text_over: look.ink,
            key: look.ink_3,
            key_line: gpui_kit::transparent_black(),
            rest: Vec::new(),
            down: Vec::new(),
        },
    }
}

/// The hard edge under a key: never a blur.
pub(crate) fn lip(color: Hsla) -> BoxShadow {
    BoxShadow { color, offset: point(px(0.), px(1.)), blur_radius: px(0.), spread_radius: px(0.), inset: false }
}

/// A hairline just inside something.
pub(crate) fn hairline(color: Hsla) -> BoxShadow {
    BoxShadow { color, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(1.), inset: true }
}

/// A hard ring around something, `width` wide.
pub(crate) fn halo(color: Hsla, width: f32) -> BoxShadow {
    BoxShadow { color, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(width), inset: false }
}

/// The light along a key's top edge.
fn sheen(color: Hsla) -> BoxShadow {
    BoxShadow { color, offset: point(px(0.), px(1.)), blur_radius: px(0.), spread_radius: px(0.), inset: true }
}

impl Press {
    pub(crate) fn new(id: impl Into<ElementId>) -> Press {
        Press { base: Button::new(id), variant: Variant::Plain, shape: Shape::Natural, small: false, on: false, label: None, key: None, width: None, children: Vec::new() }
    }

    pub(crate) const fn primary(mut self) -> Press {
        self.variant = Variant::Primary;
        self
    }

    pub(crate) const fn ghost(mut self) -> Press {
        self.variant = Variant::Ghost;
        self
    }

    pub(crate) const fn small(mut self) -> Press {
        self.small = true;
        self
    }

    pub(crate) const fn square(mut self) -> Press {
        self.shape = Shape::Square;
        self
    }

    pub(crate) const fn wide(mut self) -> Press {
        self.shape = Shape::Wide;
        self
    }

    pub(crate) const fn cell(mut self) -> Press {
        self.variant = Variant::Ghost;
        self.shape = Shape::Cell;
        self
    }

    pub(crate) const fn cell_at_end(mut self) -> Press {
        self.variant = Variant::Ghost;
        self.shape = Shape::CellAtEnd;
        self
    }

    pub(crate) fn label(mut self, label: impl Into<SharedString>) -> Press {
        self.label = Some(label.into());
        self
    }

    /// The key that presses it, in a cell of its own at the button's end.
    pub(crate) fn key(mut self, key: impl Into<SharedString>) -> Press {
        self.key = Some(key.into());
        self
    }

    /// How wide it is, for a set of buttons that share a width.
    pub(crate) const fn width(mut self, width: Pixels) -> Press {
        self.width = Some(width);
        self
    }

    pub(crate) fn on_click(mut self, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Press {
        self.base = self.base.on_click(on_click);
        self
    }

    /// What it's called for a screen reader, where its label doesn't say.
    pub(crate) fn named(mut self, name: impl Into<SharedString>) -> Press {
        self.base = self.base.accessibility_label(name);
        self
    }
}

impl Styled for Press {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl InteractiveElement for Press {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl Selectable for Press {
    fn selected(mut self, selected: bool) -> Self {
        self.on = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.on
    }
}

impl DropdownMenu for Press {}

impl ParentElement for Press {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

/// How tall a cell of the bar is, which is the bar, less the rule above it.
pub(crate) const CELL: f32 = 33.;

/// How wide a key's cell is, so that a set can allow for it.
const fn key_cell(small: bool) -> f32 {
    if small { 24. } else { 26. }
}

impl Press {
    fn pad(&self) -> f32 {
        match (self.shape.is_cell(), self.small) {
            (true, _) => 10.,
            (false, true) => 9.,
            (false, false) => 11.,
        }
    }

    fn height(&self) -> f32 {
        if self.small { 24. } else { 28. }
    }
}

impl RenderOnce for Press {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let look = Look::of(cx);
        let mut colours = paint(self.variant, &look);
        if self.on && self.variant == Variant::Ghost {
            colours.fill = look.hover;
            colours.text = look.ink;
        }
        let (pad, height) = (self.pad(), self.height());
        let (shape, small, has_key) = (self.shape, self.small, self.key.is_some());
        let cell = shape.is_cell();
        let mono = cx.global::<Monospace>().0.clone();
        let Paint { fill, hover, pressed, text, text_over, key, key_line, rest, down } = colours;
        let key_cell = self.key.map(|shown| {
            let size = if small { 11. } else { 11.5 };
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .h_full()
                .ml(px(3.))
                .min_w(px(key_cell(small)))
                .px(px(7.))
                .border_l_1()
                .border_color(key_line)
                .text_size(px(size))
                .text_color(key)
                .child(shown)
        });
        let label = self.label.map(|label| div().when(self.width.is_some(), |it| it.flex_1().text_center()).child(label));
        self.base
            .relative()
            .flex_none()
            .gap(px(6.))
            .overflow_hidden()
            .whitespace_nowrap()
            .font_weight(FontWeight::MEDIUM)
            .text_color(text)
            .bg(fill)
            .when(!cell, |it| it.shadow(rest))
            .text_size(px(if small { 12. } else { 13. }))
            .when(!cell, |it| it.h(px(height)).rounded(px(7.)))
            .when(cell, |it| it.h(px(CELL)).font_family(mono).text_size(px(11.5)).border_color(look.line))
            .when(shape == Shape::Cell, |it| it.border_r_1())
            .when(shape == Shape::CellAtEnd, |it| it.border_l_1())
            .map(|it| match shape {
                Shape::Square => it.w(px(height)).justify_center(),
                Shape::Wide => it.w_full().justify_start().px(px(pad)),
                _ if has_key => it.pl(px(pad)).pr(px(0.)),
                _ => it.px(px(pad)),
            })
            .when_some(self.width, |it, width| it.w(width))
            .hover(move |style| style.bg(hover).text_color(text_over))
            .active(move |style| {
                let style = style.bg(pressed).text_color(text_over);
                if cell { style } else { style.top(px(1.)).shadow(down) }
            })
            .focus_visible(move |style| style.shadow(vec![halo(look.focus, 2.)]))
            .children(label)
            .children(self.children)
            .children(key_cell)
    }
}

/// The monospace family in use, which the toolkit picks by what's installed.
pub(crate) struct Monospace(pub(crate) SharedString);

impl Global for Monospace {}

/// Each button of a set as wide as the widest label needs and no wider: room
/// goes around a label, never between it and its key, and nothing stretches.
pub(crate) fn set_width<'a>(labels: impl Iterator<Item = &'a str>, keys: bool, window: &mut Window) -> Pixels {
    let (size, mut font) = (px(13.), window.text_style().font());
    font.weight = FontWeight::MEDIUM;
    let text_system = window.text_system();
    let widest = labels.fold(px(0.), |widest, label| {
        let run = TextRun { len: label.len(), font: font.clone(), color: black(), background_color: None, underline: None, strikethrough: None };
        widest.max(text_system.shape_line(SharedString::from(label.to_owned()), size, &[run], None).width)
    });
    let room = if keys { 3. + key_cell(false) + 6. } else { 0. };
    widest + px(2. * 11. + 2. + room)
}

/// A gauge, `share` full.
pub(crate) fn gauge(share: f32, width: Option<f32>, fill: Hsla, look: &Look) -> Div {
    let track = div().h(px(4.)).rounded(px(2.)).overflow_hidden().bg(look.line_2).flex_none();
    let track = match width {
        Some(width) => track.w(px(width)),
        None => track.flex_1().min_w_0(),
    };
    track.child(div().h_full().w(relative(share)).bg(fill))
}

/// How many steps the strip shows.
pub(crate) const STRIP: usize = 12;

/// A turn's steps as ticks, the latest at the right, so every row's latest
/// lines up. Short looked, middle ran, tall changed a file; red failed, teal
/// is running now, and an orange dot is waiting for you.
pub(crate) fn strip(ticks: &[Tick], asking: bool, look: &Look) -> Div {
    let tick = |tick: &Tick| {
        let (height, opacity) = match tick.reach {
            Reach::Looked => (4., 0.5),
            Reach::Ran => (7., 0.68),
            Reach::Changed => (11., 0.9),
        };
        let shown = div().flex_none().w(px(2.)).rounded(px(1.));
        match tick.state {
            StepState::Running => shown.h(px(11.)).bg(look.teal),
            StepState::Failed => shown.h(px(height)).bg(look.red),
            StepState::Done | StepState::Stopped => shown.h(px(height)).bg(look.ink_3).opacity(opacity),
        }
    };
    let waiting = asking.then(|| div().flex_none().size(px(6.)).ml(px(1.)).self_center().rounded_full().bg(look.amber));
    div().flex().flex_none().items_end().gap(px(2.)).h(px(11.)).children(ticks.iter().map(tick)).children(waiting)
}

/// The wordmark's pixels as the terminal draws it, each `(x, y, wide, tall)`.
const WORD: [(u8, u8, u8, u8); 16] = [
    (0, 0, 1, 5),
    (1, 0, 2, 1),
    (3, 1, 1, 1),
    (1, 2, 2, 1),
    (2, 3, 1, 1),
    (3, 4, 1, 1),
    (5, 0, 4, 1),
    (5, 1, 1, 3),
    (6, 2, 2, 1),
    (5, 4, 4, 1),
    (10, 0, 1, 5),
    (12, 0, 1, 5),
    (13, 1, 1, 1),
    (14, 2, 1, 1),
    (15, 3, 1, 1),
    (16, 0, 1, 5),
];
/// How many pixels wide and tall it is.
const WORD_SIZE: (u8, u8) = (17, 5);

/// `rein`, in the logo's gradient, `pixel` a side: the one other place the
/// gradient appears, besides the line above a working box.
pub(crate) fn wordmark(pixel: f32) -> impl IntoElement {
    let (from, to) = (rgb(THREAD_FROM), rgb(THREAD_TO));
    let (columns, rows) = (f32::from(WORD_SIZE.0), f32::from(WORD_SIZE.1));
    let paint = move |bounds: Bounds<Pixels>, (), window: &mut Window, _: &mut App| {
        for (x, y, wide, tall) in WORD {
            for column in x..x + wide {
                let along = f32::from(column) / (columns - 1.);
                let mix = |a: f32, b: f32| a + (b - a) * along;
                let colour = Rgba { r: mix(from.r, to.r), g: mix(from.g, to.g), b: mix(from.b, to.b), a: 1. };
                let place = bounds.origin + point(px(f32::from(column) * pixel), px(f32::from(y) * pixel));
                window.paint_quad(fill(Bounds::new(place, size(px(pixel), px(f32::from(tall) * pixel))), colour));
            }
        }
    };
    canvas(|_, _, _| (), paint).flex_none().w(px(columns * pixel)).h(px(rows * pixel))
}

/// The mark for what's working: a ring with a gap, `step` eighths of a turn
/// round. It's drawn, not an icon, so it needs no asset and costs one path.
pub(crate) fn working(size: f32, color: Hsla, step: u8) -> impl IntoElement {
    let stroke = 1.75_f32;
    let paint = move |bounds: Bounds<Pixels>, (), window: &mut Window, _: &mut App| {
        let radius = bounds.size.width / 2. - px(stroke / 2.);
        let at = |angle: f32| bounds.center() + point(radius * angle.cos(), radius * angle.sin());
        let from = f32::from(step) * TAU / 8.;
        let mut path = PathBuilder::stroke(px(stroke));
        path.move_to(at(from));
        path.arc_to(point(radius, radius), px(0.), true, true, at(from + TAU * 0.75));
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    };
    canvas(|_, _, _| (), paint).flex_none().size(px(size))
}
