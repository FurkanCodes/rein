//! Colours worked out from the terminal's own background: the band behind
//! your messages, and the tints behind the lines a file change adds and
//! takes away. Blended from the background, they suit a dark theme and a
//! light one alike, as Codex's band does.
//!
//! rein asks the terminal for its background as the full screen comes up
//! ([`ASK`], `OSC 11`), and the answer comes in with the keys. Until it
//! comes, or where it never does, nothing is tinted: your message stays
//! bold, and a change's lines are green and red, as they were.

use std::sync::atomic::{AtomicU32, Ordering};

use super::style::Style;
use crate::peers::strip::Colors;

pub type Rgb = (u8, u8, u8);

/// Asks the terminal for its background colour. It answers
/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` and BEL or `ESC \`, among the keys.
pub const ASK: &str = "\x1b]11;?\x1b\\";

/// The terminal's background, once it said: `1 << 24 | rgb`; 0 until then.
static BACKGROUND: AtomicU32 = AtomicU32::new(0);

pub fn set_background((r, g, b): Rgb) {
    BACKGROUND.store(1 << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b), Ordering::Relaxed);
}

pub fn background() -> Option<Rgb> {
    let packed = BACKGROUND.load(Ordering::Relaxed);
    let byte = |shift: u32| u8::try_from(packed >> shift & 0xff).unwrap_or(0);
    (packed != 0).then(|| (byte(16), byte(8), byte(0)))
}

/// What a tint is behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    /// Your message.
    Band,
    /// A line a change adds.
    Added,
    /// A line a change takes away.
    Removed,
}

const GREEN: Rgb = (63, 185, 80);
const RED: Rgb = (224, 108, 117);

/// The colour `tint` is on `background`: a little lighter than a dark
/// background, a little darker than a light one, and for a change, a
/// little green or red.
pub fn color(tint: Tint, background: Rgb) -> Rgb {
    let dark = luminance(background) < 128.0;
    match tint {
        Tint::Band if dark => blend((255, 255, 255), background, 0.12),
        Tint::Band => blend((0, 0, 0), background, 0.06),
        Tint::Added => blend(GREEN, background, if dark { 0.2 } else { 0.18 }),
        Tint::Removed => blend(RED, background, if dark { 0.2 } else { 0.18 }),
    }
}

/// The code that puts `tint` behind what follows, if colours are on and
/// the terminal said its background.
pub fn behind(tint: Tint, style: Style) -> Option<String> {
    let background = background().filter(|_| style.on())?;
    let (r, g, b) = color(tint, background);
    Some(match Colors::here(true) {
        Colors::True => format!("\x1b[48;2;{r};{g};{b}m"),
        _ => format!("\x1b[48;5;{}m", nearest((r, g, b))),
    })
}

/// `text` on `tint` to `width` columns: what it holds, then spaces to the
/// edge, with the tint put back after every reset inside it. Plain text
/// where there's no tint.
pub fn fill(text: &str, tint: Tint, style: Style, width: usize) -> String {
    let Some(on) = behind(tint, style) else { return text.to_owned() };
    let rest = width.saturating_sub(super::style::width(text));
    format!("{on}{}{}\x1b[0m", text.replace("\x1b[0m", &format!("\x1b[0m{on}")), " ".repeat(rest))
}

/// The terminal's answer to [`ASK`], without `ESC ]` and its end:
/// `11;rgb:1515/1717/1c1c`. Each part has 1 to 4 hex digits.
pub fn parse_answer(answer: &[u8]) -> Option<Rgb> {
    let text = std::str::from_utf8(answer).ok()?;
    let rgb = text.strip_prefix("11;")?.strip_prefix("rgb:")?;
    let mut parts = rgb.split('/').map(|part| {
        let value = u32::from_str_radix(part, 16).ok().filter(|_| (1..=4).contains(&part.len()))?;
        let most = (1u32 << (4 * part.len())) - 1;
        u8::try_from(value * 255 / most).ok()
    });
    let rgb = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(rgb)
}

fn luminance((r, g, b): Rgb) -> f32 {
    0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
}

/// `top` over `bottom`, `alpha` of the way.
fn blend(top: Rgb, bottom: Rgb, alpha: f32) -> Rgb {
    let mix = |t: u8, b: u8| (f32::from(t) * alpha + f32::from(b) * (1.0 - alpha)).round().clamp(0.0, 255.0) as u8;
    (mix(top.0, bottom.0), mix(top.1, bottom.1), mix(top.2, bottom.2))
}

/// The nearest of xterm's 256 colours, its greys included: a tint is
/// nearly grey, and the 6×6×6 cube alone would make it black.
fn nearest(rgb: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let distance = |(r, g, b): Rgb| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(r, rgb.0) + d(g, rgb.1) + d(b, rgb.2)
    };
    let level = |c: u8| LEVELS.iter().enumerate().min_by_key(|(_, l)| (i32::from(**l) - i32::from(c)).abs()).map_or(0, |(i, _)| i);
    let (r, g, b) = (level(rgb.0), level(rgb.1), level(rgb.2));
    let cube = (16 + 36 * r + 6 * g + b, (LEVELS[r], LEVELS[g], LEVELS[b]));
    let greys = (0..24u8).map(|i| (232 + usize::from(i), (8 + 10 * i, 8 + 10 * i, 8 + 10 * i)));
    let (index, _) = std::iter::once(cube).chain(greys).min_by_key(|(_, color)| distance(*color)).unwrap_or(cube);
    u8::try_from(index).unwrap_or(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_terminals_answer() {
        assert_eq!(parse_answer(b"11;rgb:1515/1717/1c1c"), Some((21, 23, 28)));
        assert_eq!(parse_answer(b"11;rgb:ff/80/00"), Some((255, 128, 0)), "two hex digits a part");
        assert_eq!(parse_answer(b"11;rgb:f/8/0"), Some((255, 136, 0)), "one");
        assert_eq!(parse_answer(b"10;rgb:ffff/ffff/ffff"), None, "the foreground, not asked for");
        assert_eq!(parse_answer(b"11;rgb:ffff/ffff"), None);
        assert_eq!(parse_answer(b"11;rgb:ffff/ffff/ffff/ffff"), None);
        assert_eq!(parse_answer(b"11;rgb:fffff/0/0"), None);
    }

    #[test]
    fn a_tint_is_lighter_on_dark_and_darker_on_light() {
        assert_eq!(color(Tint::Band, (21, 23, 28)), (49, 51, 55));
        assert_eq!(color(Tint::Band, (255, 255, 255)), (240, 240, 240));
        let (r, g, _) = color(Tint::Added, (21, 23, 28));
        assert!(g > r && g < 70, "a dark green: {r} {g}");
        let (r, g, _) = color(Tint::Removed, (255, 255, 255));
        assert!(r > g && g > 200, "a light red: {r} {g}");
    }

    #[test]
    fn the_nearest_of_256_colours_keeps_greys_grey() {
        assert_eq!(nearest((49, 51, 55)), 236, "#303030, not black");
        assert_eq!(nearest((255, 0, 0)), 196);
        assert_eq!(nearest((0, 0, 0)), 16);
    }

    #[test]
    fn fills_to_the_edge_and_keeps_the_tint_after_a_reset() {
        set_background((21, 23, 28));
        let filled = fill("you \x1b[90mx\x1b[0m y", Tint::Band, Style::colored(), 12);
        assert!(filled.ends_with("     \x1b[0m"), "{filled:?}");
        assert_eq!(filled.matches("48;").count(), 2, "set again after the reset: {filled:?}");
        assert_eq!(super::super::style::width(&filled), 12);
        assert_eq!(fill("x", Tint::Band, Style::plain(), 12), "x", "no colours, no band");
    }
}
