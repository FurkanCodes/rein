//! How the reins look while they work, in any terminal: the tab's title,
//! which every terminal shows, and a strip across the top of rein's own
//! screen, which rein draws itself, in colour.
//!
//! ```text
//!   api ⠹ 41s    ↳ tests ✓ done 12s    web ◆ needs you             health
//! ─────────────━━━━━━━━━━━━━━━━━━━━──────────────────────────────────────
//! ```
//!
//! Every tab lists the reins in the same order, so changing tabs moves
//! nothing: only which one is lit, this tab's own, with the heavy part of
//! the line under it.
//!
//! Everything here is drawn from what each rein says of itself and a frame
//! count, eight frames a second: what moves is worked out, not stored.

use super::{State, Status};
use crate::ui::style::{fit, width};

/// The spinner, a frame at a time.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Frames a second.
pub(crate) const FPS: u64 = 8;

/// What colours the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Colors {
    None,
    /// The 256 of xterm's palette.
    Palette,
    /// Any colour: `COLORTERM` says `truecolor` or `24bit`.
    True,
}

impl Colors {
    pub(crate) fn here(on: bool) -> Colors {
        if !on {
            return Colors::None;
        }
        match std::env::var("COLORTERM").as_deref() {
            Ok("truecolor" | "24bit") => Colors::True,
            _ => Colors::Palette,
        }
    }

    /// `text` in the colour `(r, g, b)`, bold if `bold`.
    pub(crate) fn paint(self, color: (u8, u8, u8), bold: bool, text: &str) -> String {
        self.paint_on(color, None, bold, text)
    }

    /// `text` in the colour `(r, g, b)`, on `back` if there's one.
    fn paint_on(self, (r, g, b): (u8, u8, u8), back: Option<(u8, u8, u8)>, bold: bool, text: &str) -> String {
        let weight = if bold { "1;" } else { "" };
        let behind = match (self, back) {
            (Colors::True, Some((r, g, b))) => format!("48;2;{r};{g};{b};"),
            (Colors::Palette, Some((r, g, b))) => format!("48;5;{};", palette(r, g, b)),
            _ => String::new(),
        };
        match self {
            Colors::None => text.to_owned(),
            Colors::True => format!("\x1b[{weight}{behind}38;2;{r};{g};{b}m{text}\x1b[0m"),
            Colors::Palette => format!("\x1b[{weight}{behind}38;5;{}m{text}\x1b[0m", palette(r, g, b)),
        }
    }
}

/// The nearest of xterm's 6×6×6 colour cube.
fn palette(r: u8, g: u8, b: u8) -> u8 {
    let level = |c: u8| (u16::from(c) * 5 / 255) as u8;
    16 + 36 * level(r) + 6 * level(g) + level(b)
}

const ORANGE: (u8, u8, u8) = (255, 165, 0);
const BLUE: (u8, u8, u8) = (74, 158, 255);
const GREEN: (u8, u8, u8) = (63, 185, 80);
const GREY: (u8, u8, u8) = (128, 128, 128);

/// A state's colour: orange working, blue needing you, green done.
const fn color(state: State) -> (u8, u8, u8) {
    match state {
        State::Working => ORANGE,
        State::Waiting => BLUE,
        State::Done => GREEN,
        State::Idle => GREY,
    }
}

/// `12s`, `4m`, `2h`.
pub(crate) fn short(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// What a rein's state says, after its name, at `frame`, `now` seconds
/// since the epoch: a spinner and the time while it works.
fn state_words(status: &Status, frame: u64, now: u64) -> String {
    match status.state {
        State::Working => format!("{} {}", SPINNER[(frame as usize) % SPINNER.len()], short(now.saturating_sub(status.since))),
        State::Waiting => "◆ needs you".to_owned(),
        State::Done => match status.took {
            Some(took) => format!("✓ done {}", short(took)),
            None => "✓ done".to_owned(),
        },
        State::Idle => "○".to_owned(),
    }
}

/// The tab's title at `frame`: `⠹ api · 41s`, `◆ api · needs you`,
/// `✓ api · done 12s`, `○ api`. A turning spinner marks a tab at work
/// among the others, in a terminal that shows nothing else.
pub(crate) fn title(status: &Status, frame: u64, now: u64) -> String {
    let name = &status.name;
    match status.state {
        State::Working => format!("{} {name} · {}", SPINNER[(frame as usize) % SPINNER.len()], short(now.saturating_sub(status.since))),
        State::Waiting => format!("◆ {name} · needs you"),
        State::Done => match status.took {
            Some(took) => format!("✓ {name} · done {}", short(took)),
            None => format!("✓ {name} · done"),
        },
        State::Idle => format!("○ {name}"),
    }
}

/// Behind this tab's own rein in the strip.
const LIT: (u8, u8, u8) = (62, 68, 82);

/// The same order in every tab: reins by name, each followed by its own
/// sub-agents. Index 0 is `me`, the others follow it in `others`' order.
fn order(me: &Status, others: &[Status]) -> Vec<usize> {
    let all: Vec<&Status> = std::iter::once(me).chain(others).collect();
    let key = |s: &Status| match &s.parent {
        Some(parent) => (parent.clone(), 1, s.name.clone(), s.workspace.clone()),
        None => (s.name.clone(), 0, String::new(), s.workspace.clone()),
    };
    let mut indices: Vec<usize> = (0..all.len()).collect();
    indices.sort_by_key(|&i| key(all[i]));
    indices
}

/// The strip: every rein in the same order whichever tab shows it, each
/// in its state's colour, this tab's own lit, and under them a line whose
/// colour says how they all are, heavy under this tab's own, with a band
/// of light running along it while any of them works.
pub(crate) fn lines(me: &Status, others: &[Status], frame: u64, now: u64, colors: Colors, columns: usize) -> Vec<String> {
    let all: Vec<&Status> = std::iter::once(me).chain(others).collect();
    let mut row = String::from(" ");
    let mut mine = 0..0;
    for (n, i) in order(me, others).into_iter().enumerate() {
        let status = all[i];
        if n > 0 {
            row.push_str("  ");
        }
        let words = state_words(status, frame, now);
        // A rein that needs you pulses, slower than the spinner turns.
        let bright = status.state != State::Waiting || (frame / (FPS / 2)).is_multiple_of(2);
        let (r, g, b) = color(status.state);
        let tint = if bright { (r, g, b) } else { (r / 2, g / 2, b / 2) };
        // A sub-agent shows whose it is with ↳.
        let label = if status.parent.is_some() { format!("↳ {}", status.name) } else { status.name.clone() };
        let (back, ink) = if i == 0 { (Some(LIT), (245, 245, 245)) } else { (None, (170, 170, 170)) };
        let start = width(&row);
        row.push_str(&colors.paint_on(ink, back, i == 0, &format!(" {label} ")));
        row.push_str(&colors.paint_on(tint, back, status.state == State::Waiting, &format!("{words} ")));
        if i == 0 {
            mine = start..width(&row);
        }
    }
    let workspace = colors.paint(GREY, false, &me.workspace);
    let room = columns.saturating_sub(width(&row) + width(&workspace) + 1);
    let top = if room > 0 { format!("{row}{}{workspace} ", " ".repeat(room)) } else { fit(&row, columns) };
    vec![top, rule(me, others, &mine, frame, colors, columns)]
}

/// The line under the strip, `columns` wide, heavy under `mine` and thin
/// elsewhere. Its colour is the most pressing of all the reins' states:
/// amber while one needs you, green while one works, with a band of light
/// sweeping along it, then a calm green once done, or grey.
fn rule(me: &Status, others: &[Status], mine: &std::ops::Range<usize>, frame: u64, colors: Colors, columns: usize) -> String {
    let states: Vec<State> = std::iter::once(me.state).chain(others.iter().map(|o| o.state)).collect();
    let (base, moving) = if states.contains(&State::Waiting) {
        ((255, 176, 32), false)
    } else if states.contains(&State::Working) {
        ((40, 200, 90), true)
    } else if states.contains(&State::Done) {
        ((40, 140, 70), false)
    } else {
        ((70, 70, 70), false)
    };
    let glyph = |x: usize| if mine.contains(&x) { "━" } else { "─" };
    if colors == Colors::None {
        return (0..columns).map(glyph).collect();
    }
    // Waiting pulses as a whole; working has a band of light go by.
    let pulse = if states.contains(&State::Waiting) && !(frame / (FPS / 2)).is_multiple_of(2) { 0.55 } else { 1.0 };
    let span = columns.max(1) as f64;
    let head = (frame as f64 * 3.0) % (span + 24.0) - 12.0;
    (0..columns)
        .map(|x| {
            let lift = if moving { (1.0 - ((x as f64 - head).abs() / 12.0)).max(0.0) } else { 0.0 };
            // Under this tab's own, full strength; elsewhere, dimmer.
            let strength = if mine.contains(&x) { 1.0 } else { 0.6 };
            let shade = |c: u8| ((f64::from(c) * pulse * strength) + (255.0 - f64::from(c) * strength) * lift * 0.8).clamp(0.0, 255.0) as u8;
            colors.paint((shade(base.0), shade(base.1), shade(base.2)), false, glyph(x))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, state: State, since: u64) -> Status {
        Status {
            name: name.into(),
            workspace: "health".into(),
            root: "/p".into(),
            model: "m".into(),
            state,
            doing: None,
            since,
            took: None,
            tokens: 0,
            editing: Vec::new(),
            question: None,
            recent: Vec::new(),
            parent: None,
        }
    }

    #[test]
    fn the_title_turns_while_it_works_and_says_what_it_came_to() {
        let working = status("api", State::Working, 100);
        assert_eq!(title(&working, 0, 141), "⠋ api · 41s");
        assert_eq!(title(&working, 2, 141), "⠹ api · 41s", "the spinner moves on with the frames");
        assert_eq!(title(&working, 0, 400), "⠋ api · 5m");
        assert_eq!(title(&status("api", State::Waiting, 0), 3, 9), "◆ api · needs you");
        let done = Status { took: Some(12), ..status("api", State::Done, 0) };
        assert_eq!(title(&done, 0, 0), "✓ api · done 12s");
        assert_eq!(title(&status("api", State::Idle, 0), 0, 0), "○ api");
    }

    #[test]
    fn the_strip_shows_every_rein_and_fills_the_width() {
        let me = status("api", State::Working, 100);
        let others = [status("web", State::Waiting, 0), Status { took: Some(12), ..status("review", State::Done, 0) }];
        let lines = lines(&me, &others, 2, 141, Colors::None, 80);
        assert!(lines[0].starts_with("  api ⠹ 41s    review ✓ done 12s    web ◆ needs you   "), "{}", lines[0]);
        assert!(lines[0].ends_with(" health ") && width(&lines[0]) == 80, "the workspace at the right edge: {:?}", lines[0]);
        assert_eq!(width(&lines[1]), 80);
        // In colour, the line is as wide, and while one works it moves.
        let a = lines_at(&me, &others, 0);
        let b = lines_at(&me, &others, 4);
        assert_eq!(width(&a[1]), 80);
        assert_ne!(a[1], b[1], "the line pulses while one needs you");
        let calm = [status("web", State::Working, 0)];
        assert_ne!(lines_at(&status("api", State::Idle, 0), &calm, 0)[1], lines_at(&status("api", State::Idle, 0), &calm, 5)[1], "a band runs along it while one works");
        assert_eq!(lines_at(&status("api", State::Idle, 0), &[], 0)[1], lines_at(&status("api", State::Idle, 0), &[], 5)[1], "still, when nothing happens");
    }

    #[test]
    fn every_tab_lists_the_reins_in_the_same_order_and_only_the_lit_one_changes() {
        let lead = status("lead", State::Working, 100);
        let tests = Status { parent: Some("lead".into()), ..status("tests", State::Done, 0) };
        let api = status("api", State::Idle, 0);
        let from_lead = lines(&lead, &[tests.clone(), api.clone()], 2, 141, Colors::None, 80);
        let from_tests = lines(&tests, &[api.clone(), lead.clone()], 2, 141, Colors::None, 80);
        assert_eq!(from_lead[0], from_tests[0], "the same row from either tab");
        assert!(from_lead[0].starts_with("  api ○    lead ⠹ 41s    ↳ tests ✓ done  "), "a sub-agent after its own: {:?}", from_lead[0]);
        // The heavy part of the line is under the tab's own.
        let heavy = |line: &str| line.chars().enumerate().filter(|(_, c)| *c == '━').map(|(x, _)| x).collect::<Vec<_>>();
        let at = |row: &str, name: &str| row.find(name).map(|byte| row[..byte].chars().count()).unwrap_or(0);
        assert_eq!(heavy(&from_lead[1]).first(), Some(&(at(&from_lead[0], "lead") - 1)));
        assert_eq!(heavy(&from_tests[1]).first(), Some(&(at(&from_tests[0], "↳ tests") - 1)));
        assert_eq!(heavy(&from_lead[1]).len(), " lead ⠹ 41s ".chars().count());
        // In colour, only this tab's own is lit.
        let lit = lines(&lead, &[tests, api], 2, 141, Colors::True, 80);
        assert_eq!(lit[0].matches("48;2;").count(), 2, "one lit cell, its name and its state: {:?}", lit[0]);
    }

    fn lines_at(me: &Status, others: &[Status], frame: u64) -> Vec<String> {
        lines(me, others, frame, 141, Colors::True, 80)
    }

    #[test]
    fn colours_come_as_the_terminal_takes_them() {
        assert_eq!(Colors::True.paint(ORANGE, false, "x"), "\x1b[38;2;255;165;0mx\x1b[0m");
        assert_eq!(Colors::Palette.paint((255, 0, 0), true, "x"), "\x1b[1;38;5;196mx\x1b[0m");
        assert_eq!(Colors::None.paint(ORANGE, true, "x"), "x");
    }
}
