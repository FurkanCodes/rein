//! What providers count of your use, as each says it: a plan's 5 hours
//! and week, a key's budget, what a key spent today. One shape for every
//! provider, so the bar shows whatever there is the same way, and a
//! provider that says nothing shows nothing: never a guess.

use std::time::{Duration, SystemTime};

/// One thing a provider counts of your use, as it last said it.
#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    /// The provider's name for it: one said again with the same name is
    /// the same meter, read again.
    pub id: String,
    /// What it's only for, where it isn't for everything: "Opus", "free models".
    pub only: Option<String>,
    /// How long it counts before it starts over; `None` for one that never does.
    pub span: Option<Duration>,
    /// When it next starts over, where the provider says.
    pub resets: Option<SystemTime>,
    pub reading: Reading,
    /// What the provider says of it, or where it says nothing, what the
    /// reading does ([`Standing::of`]).
    pub standing: Standing,
}

/// How much is used, in what the provider counts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    /// A share of the limit, in percent, as plans say it.
    Used(u8),
    /// US dollars left of a cap of so many.
    Left { dollars: f64, cap: f64 },
    /// US dollars spent, with no cap to spend them against.
    Spent(f64),
    /// Requests made, of so many allowed.
    Requests { made: u64, cap: u64 },
}

impl Reading {
    /// How much of its limit is used, in percent: `None` for what's spent
    /// with no limit.
    pub fn used(self) -> Option<u8> {
        let share = |used: f64, cap: f64| if cap > 0.0 { (used / cap * 100.0).round().clamp(0.0, 100.0) as u8 } else { 100 };
        match self {
            Reading::Used(percent) => Some(percent.min(100)),
            Reading::Left { dollars, cap } => Some(share(cap - dollars, cap)),
            Reading::Requests { made, cap } => Some(share(made as f64, cap as f64)),
            Reading::Spent(_) => None,
        }
    }
}

/// How a meter stands. A meter informs, and only the provider's refusal
/// stops anything: a week at 100% may still be served, from credits or
/// overage, so rein never stops on a reading alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Standing {
    Fine,
    /// Getting close: the provider says so, or most of it is used.
    Warned,
    /// All of it used: the provider turns requests away until it resets.
    Reached,
}

/// Where a reading alone warns, as Codex and Claude Code first do.
const WARN_FROM: u8 = 75;

impl Standing {
    /// What `reading` says of itself, for a provider that doesn't say.
    pub fn of(reading: Reading) -> Standing {
        match reading.used() {
            Some(100..) => Standing::Reached,
            Some(used) if used >= WARN_FROM => Standing::Warned,
            _ => Standing::Fine,
        }
    }
}

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

impl Meter {
    /// Its span as people say it: `5h`, `7d`, `mo`; with what it's only
    /// for after it: `7d Opus`. Named by its length, never from a list of
    /// the ones rein knows: a plan's monthly window shows as one too.
    pub fn label(&self) -> String {
        let span = match self.span.map(|span| span.as_secs()) {
            None => String::new(),
            Some(secs) if secs < HOUR => format!("{}m", (secs / 60).max(1)),
            Some(secs) if secs < DAY => format!("{}h", (secs + HOUR / 2) / HOUR),
            Some(secs) if secs < 28 * DAY => format!("{}d", (secs + DAY / 2) / DAY),
            Some(secs) if secs < 365 * DAY => "mo".to_owned(),
            Some(_) => "yr".to_owned(),
        };
        match (&self.only, span.is_empty()) {
            (Some(only), true) => only.clone(),
            (Some(only), false) => format!("{span} {only}"),
            (None, _) => span,
        }
    }
}

/// Where a provider says how much of your limits is used, besides with
/// its answers: something to ask it now and then, with your own key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Asked {
    /// Nowhere else: with its answers, if at all.
    #[default]
    Nowhere,
    /// OpenRouter's `GET /key`: the key's budget or cap, or what it spent
    /// today, and its free requests today.
    OpenRouterKey,
}

/// `meters` with `newer` read into them: a meter said again replaces what
/// it said before, and the others stay.
pub fn merge(meters: &mut Vec<Meter>, newer: Vec<Meter>) {
    for meter in newer {
        match meters.iter_mut().find(|known| known.id == meter.id) {
            Some(known) => *known = meter,
            None => meters.push(meter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter(span: Option<u64>, only: Option<&str>) -> Meter {
        Meter { id: "m".into(), only: only.map(str::to_owned), span: span.map(Duration::from_secs), resets: None, reading: Reading::Used(1), standing: Standing::Fine }
    }

    #[test]
    fn a_meter_is_named_by_how_long_it_counts() {
        assert_eq!(meter(Some(5 * HOUR), None).label(), "5h");
        assert_eq!(meter(Some(7 * DAY), None).label(), "7d");
        assert_eq!(meter(Some(7 * DAY), Some("Opus")).label(), "7d Opus");
        assert_eq!(meter(Some(DAY), None).label(), "1d");
        assert_eq!(meter(Some(30 * DAY), None).label(), "mo", "a monthly plan's window shows too");
        assert_eq!(meter(Some(15 * 60), None).label(), "15m");
        assert_eq!(meter(None, Some("free models")).label(), "free models");
        assert_eq!(meter(None, None).label(), "");
    }

    #[test]
    fn a_reading_says_how_much_is_used_and_where_it_stands() {
        assert_eq!(Reading::Left { dollars: 2.5, cap: 10.0 }.used(), Some(75));
        assert_eq!(Reading::Left { dollars: 0.0, cap: 0.0 }.used(), Some(100), "a cap of nothing is all used");
        assert_eq!(Reading::Requests { made: 50, cap: 50 }.used(), Some(100));
        assert_eq!(Reading::Left { dollars: 12.0, cap: 10.0 }.used(), Some(0), "more left than the cap is none used, not an underflow");
        assert_eq!(Reading::Spent(3.0).used(), None);
        assert_eq!(Standing::of(Reading::Used(74)), Standing::Fine);
        assert_eq!(Standing::of(Reading::Used(75)), Standing::Warned);
        assert_eq!(Standing::of(Reading::Requests { made: 50, cap: 50 }), Standing::Reached);
        assert_eq!(Standing::of(Reading::Spent(1e9)), Standing::Fine, "spending with no cap is never a limit");
    }

    #[test]
    fn a_meter_said_again_replaces_itself_and_the_others_stay() {
        let mut known = vec![Meter { id: "five_hour".into(), ..meter(Some(5 * HOUR), None) }, Meter { id: "seven_day".into(), ..meter(Some(7 * DAY), None) }];
        merge(&mut known, vec![Meter { id: "seven_day".into(), reading: Reading::Used(80), ..meter(Some(7 * DAY), None) }]);
        assert_eq!(known.len(), 2);
        assert_eq!(known[1].reading, Reading::Used(80));
        assert_eq!(known[0].id, "five_hour");
    }
}
