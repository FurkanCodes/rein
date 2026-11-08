//! What rein did while its tab was behind another, said over the box when
//! you come back: switching to a tab catches you up on it, so you read
//! the work in pieces as it goes rather than all of it at the end.
//!
//! ```text
//!   ↳ while you were away · 4m · 12 steps, 1 failed · done 1m ago
//!     changed src/api.rs, src/main.rs and 2 more
//!     cargo check · no new errors · 0.4s
//! ```
//!
//! Only the terminal saying its window went behind another starts it:
//! where it doesn't say, nobody knows when you left.

use std::time::Instant;

use super::style::{Style, fit};
use crate::peers::strip::short;

/// The most changed files named; the rest are counted.
const NAMED: usize = 3;

/// What happened that's worth catching up on.
pub enum Saw {
    /// A step ran: worked, failed, or neither (declined, say).
    Step { ok: Option<bool> },
    /// A file changed, by a step or by a command it ran.
    Changed(String),
    /// A check ran after edits: what its line says, and whether it found
    /// errors the turn brought in.
    Checked { said: String, broke: bool },
    /// The turn ended, done or not.
    Ended { done: bool },
}

/// What's happened since the tab went behind another.
#[derive(Default)]
pub struct Away {
    /// When you left; `None` while you're here.
    left: Option<Instant>,
    steps: usize,
    failed: usize,
    /// Each once, in the order they changed.
    changed: Vec<String>,
    check: Option<(String, bool)>,
    /// How the last turn ended, and when.
    ended: Option<(bool, Instant)>,
}

impl Away {
    /// The window went behind another: what happens from now is noted.
    /// Leaving again before coming back keeps what's noted so far.
    pub fn leave(&mut self, now: Instant) {
        if self.left.is_none() {
            *self = Away { left: Some(now), ..Away::default() };
        }
    }

    /// Notes `saw`, if you're away.
    pub fn saw(&mut self, saw: Saw) {
        if self.left.is_none() {
            return;
        }
        match saw {
            Saw::Step { ok } => {
                self.steps += 1;
                if ok == Some(false) {
                    self.failed += 1;
                }
            }
            Saw::Changed(path) => {
                if !self.changed.contains(&path) {
                    self.changed.push(path);
                }
            }
            Saw::Checked { said, broke } => self.check = Some((said, broke)),
            Saw::Ended { done } => self.ended = Some((done, Instant::now())),
        }
    }

    /// You're back: what happened while you were away, if anything did,
    /// and a question waiting since `asked`, if one is. Nothing's noted
    /// after this until you leave again.
    pub fn back(&mut self, now: Instant, asked: Option<Instant>) -> Option<Caught> {
        let left = self.left.take()?;
        let away = std::mem::take(self);
        if away.steps == 0 && away.ended.is_none() && away.check.is_none() {
            return None;
        }
        let ago = |at: Instant| short(now.saturating_duration_since(at).as_secs());
        Some(Caught {
            away: ago(left),
            steps: away.steps,
            failed: away.failed,
            changed: away.changed,
            check: away.check,
            ended: away.ended.map(|(done, at)| (done, ago(at))),
            waiting: asked.map(ago),
        })
    }
}

/// What happened while you were away, as it was when you came back.
#[derive(Debug, PartialEq, Eq)]
pub struct Caught {
    away: String,
    steps: usize,
    failed: usize,
    changed: Vec<String>,
    check: Option<(String, bool)>,
    ended: Option<(bool, String)>,
    waiting: Option<String>,
}

impl Caught {
    /// Its lines, each fitted to `columns`, after `pad`.
    pub fn lines(&self, pad: &str, columns: usize, s: Style) -> Vec<String> {
        let room = columns.saturating_sub(pad.len() + 1);
        let mut head = vec![format!("while you were away · {}", self.away)];
        match (self.steps, self.failed) {
            (0, _) => {}
            (1, 0) => head.push("1 step".to_owned()),
            (n, 0) => head.push(format!("{n} steps")),
            (n, failed) => head.push(format!("{n} step{}, {failed} failed", if n == 1 { "" } else { "s" })),
        }
        match &self.ended {
            Some((true, ago)) => head.push(format!("done {ago} ago")),
            Some((false, ago)) => head.push(format!("stopped {ago} ago")),
            None => {}
        }
        if let Some(waiting) = &self.waiting {
            head.push(format!("waiting for you · {waiting}"));
        }
        let head = head.join(" · ");
        let mut lines = vec![format!("{pad}{} {}", s.blue("↳"), if self.failed > 0 { s.red(&fit(&head, room)) } else { s.dim(&fit(&head, room)) })];
        if !self.changed.is_empty() {
            lines.push(format!("{pad}  {}", s.dim(&fit(&format!("changed {}", named(&self.changed)), room.saturating_sub(1)))));
        }
        if let Some((said, broke)) = &self.check {
            let said = fit(said, room.saturating_sub(1));
            lines.push(format!("{pad}  {}", if *broke { s.red(&said) } else { s.dim(&said) }));
        }
        lines
    }
}

/// `a.rs`, `a.rs and b.rs`, `a.rs, b.rs, c.rs and 2 more`.
fn named(paths: &[String]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        many if many.len() <= NAMED => format!("{} and {}", many[..many.len() - 1].join(", "), many[many.len() - 1]),
        many => format!("{} and {} more", many[..NAMED].join(", "), many.len() - NAMED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn plain(caught: &Caught) -> Vec<String> {
        caught.lines("", 200, Style::plain()).iter().map(|line| crate::ui::style::strip(line)).collect()
    }

    #[test]
    fn what_happened_while_you_were_away_is_said_once_you_are_back() {
        let start = Instant::now();
        let mut away = Away::default();
        away.saw(Saw::Step { ok: Some(true) });
        away.leave(start);
        for ok in [Some(true), Some(false), None] {
            away.saw(Saw::Step { ok });
        }
        for path in ["a.rs", "b.rs", "a.rs", "c.rs", "d.rs", "e.rs"] {
            away.saw(Saw::Changed(path.to_owned()));
        }
        away.saw(Saw::Checked { said: "cargo check · 2 new errors · 0.6s".to_owned(), broke: true });
        let caught = away.back(start + Duration::from_secs(240), Some(start + Duration::from_secs(180))).expect("something happened");
        assert_eq!(
            plain(&caught),
            ["↳ while you were away · 4m · 3 steps, 1 failed · waiting for you · 1m", "  changed a.rs, b.rs, c.rs and 2 more", "  cargo check · 2 new errors · 0.6s"],
            "the step before you left isn't counted, and a.rs is named once"
        );
        // Back, nothing more is noted until you leave again.
        away.saw(Saw::Step { ok: Some(true) });
        assert_eq!(away.back(start, None), None);
    }

    #[test]
    fn nothing_is_said_when_nothing_happened() {
        let mut away = Away::default();
        assert_eq!(away.back(Instant::now(), None), None, "you never left");
        away.leave(Instant::now());
        assert_eq!(away.back(Instant::now(), Some(Instant::now())), None, "a question you left waiting is no news");
    }

    #[test]
    fn leaving_twice_keeps_what_was_noted() {
        let start = Instant::now();
        let mut away = Away::default();
        away.leave(start);
        away.saw(Saw::Step { ok: Some(true) });
        away.leave(start + Duration::from_secs(5));
        away.saw(Saw::Ended { done: true });
        let caught = away.back(start + Duration::from_secs(90), None).unwrap();
        assert!(plain(&caught)[0].starts_with("↳ while you were away · 1m · 1 step · done "), "{caught:?}");
    }

    #[test]
    fn a_few_files_are_named_and_the_rest_counted() {
        let paths = |n: usize| (1..=n).map(|i| format!("{i}.rs")).collect::<Vec<_>>();
        assert_eq!(named(&paths(1)), "1.rs");
        assert_eq!(named(&paths(2)), "1.rs and 2.rs");
        assert_eq!(named(&paths(3)), "1.rs, 2.rs and 3.rs");
        assert_eq!(named(&paths(5)), "1.rs, 2.rs, 3.rs and 2 more");
    }
}
