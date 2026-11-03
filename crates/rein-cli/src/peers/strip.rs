//! How the reins look while they work, in any terminal: the tab's title,
//! which every terminal shows, and a strip across the top of rein's own
//! screen, which rein draws itself, in colour.
//!
//! ```text
//!  ▸ api ⠹ 41s    web ◆ needs you    review ✓ done 12s    tests ○          health
//! ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
//! ```
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
    fn paint(self, (r, g, b): (u8, u8, u8), bold: bool, text: &str) -> String {
        let weight = if bold { "1;" } else { "" };
        match self {
            Colors::None => text.to_owned(),
            Colors::True => format!("\x1b[{weight}38;2;{r};{g};{b}m{text}\x1b[0m"),
            Colors::Palette => format!("\x1b[{weight}38;5;{}m{text}\x1b[0m", palette(r, g, b)),
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
fn short(secs: u64) -> String {
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

/// The strip: this rein, then the others, each in its state's colour, and
/// under them a line whose colour says how they all are, with a band of
/// light running along it while any of them works.
pub(crate) fn lines(me: &Status, others: &[Status], frame: u64, now: u64, colors: Colors, columns: usize) -> Vec<String> {
    let mut cells = Vec::new();
    for (status, mine) in std::iter::once((me, true)).chain(others.iter().map(|o| (o, false))) {
        let words = state_words(status, frame, now);
        // A rein that needs you pulses, slower than the spinner turns.
        let bright = status.state != State::Waiting || (frame / (FPS / 2)).is_multiple_of(2);
        let (r, g, b) = color(status.state);
        let tint = if bright { (r, g, b) } else { (r / 2, g / 2, b / 2) };
        let name = if mine { colors.paint((230, 230, 230), true, &format!("▸ {}", status.name)) } else { colors.paint((200, 200, 200), false, &status.name) };
        cells.push(format!("{name} {}", colors.paint(tint, status.state == State::Waiting, &words)));
    }
    let row = format!(" {}", cells.join("    "));
    let workspace = colors.paint(GREY, false, &me.workspace);
    let room = columns.saturating_sub(width(&row) + width(&workspace) + 1);
    let top = if room > 0 { format!("{row}{}{workspace} ", " ".repeat(room)) } else { fit(&row, columns) };
    vec![top, rule(me, others, frame, colors, columns)]
}

/// The line under the strip, `columns` wide. Its colour is the most
/// pressing of all the reins' states: amber while one needs you, green
/// while one works, with a band of light sweeping along it, then a calm
/// green once done, or grey.
fn rule(me: &Status, others: &[Status], frame: u64, colors: Colors, columns: usize) -> String {
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
    if colors == Colors::None {
        return "━".repeat(columns);
    }
    // Waiting pulses as a whole; working has a band of light go by.
    let pulse = if states.contains(&State::Waiting) && !(frame / (FPS / 2)).is_multiple_of(2) { 0.55 } else { 1.0 };
    let span = columns.max(1) as f64;
    let head = (frame as f64 * 3.0) % (span + 24.0) - 12.0;
    (0..columns)
        .map(|x| {
            let lift = if moving { (1.0 - ((x as f64 - head).abs() / 12.0)).max(0.0) } else { 0.0 };
            let shade = |c: u8| ((f64::from(c) * pulse * 0.75) + (255.0 - f64::from(c) * 0.75) * lift * 0.8).clamp(0.0, 255.0) as u8;
            colors.paint((shade(base.0), shade(base.1), shade(base.2)), false, "━")
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
        assert!(lines[0].starts_with(" ▸ api ⠹ 41s    web ◆ needs you    review ✓ done 12s  "), "{}", lines[0]);
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
