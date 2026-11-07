//! How much of your limits is used, as the provider says it: kept as it
//! last said each meter, shown in the bar, said in a line when one gets
//! close, and asked of a provider that can be asked, in the background.

use std::time::{Duration, Instant, SystemTime};

use rein_llm::{Client, Meter, Reading, Standing};

use crate::format::local_time;
use crate::ui::style::Style;

/// How often a provider is asked at most: usage endpoints turn away who
/// asks too often (oh-my-pi's users got hundreds of 429s a day).
const EVERY: Duration = Duration::from_secs(60);

/// How long an answer may take before it's given up on.
const PATIENCE: Duration = Duration::from_secs(10);

/// A meter as a client draws it.
pub(crate) struct Cell {
    /// What it measures: `5h`, or the whole of what's not a share.
    pub(crate) label: String,
    /// How much of it is used, in percent, where there's a limit.
    pub(crate) used: Option<u8>,
    pub(crate) said: String,
    /// When it starts over, once that matters.
    pub(crate) resets: Option<String>,
    pub(crate) tone: Standing,
}

/// What rein knows of how much of your limits is used.
#[derive(Default)]
pub(crate) struct Meters {
    known: Vec<Meter>,
    /// How each was last said to stand, so a warning comes once.
    said: Vec<(String, Standing)>,
    /// Asking the provider: dropped, it stops.
    asking: tokio::task::JoinSet<rein_llm::Result<Vec<Meter>>>,
    asked: Option<Instant>,
}

impl Meters {
    /// `newer` read in, at `now`: the lines to say of each that stands
    /// worse than it was last said to.
    pub(crate) fn read(&mut self, newer: Vec<Meter>, now: SystemTime) -> Vec<String> {
        let mut lines = Vec::new();
        for meter in &newer {
            let tone = tone(meter);
            let said = self.said.iter_mut().find(|(id, _)| *id == meter.id);
            let before = said.as_ref().map_or(Standing::Fine, |(_, said)| *said);
            if tone > before {
                lines.push(warning(meter, now));
            }
            match said {
                Some((_, said)) => *said = tone,
                None => self.said.push((meter.id.clone(), tone)),
            }
        }
        rein_llm::meter::merge(&mut self.known, newer);
        lines
    }

    /// Forgets everything: they were another provider's, or another sign-in's.
    pub(crate) fn forget(&mut self) {
        *self = Meters::default();
    }

    /// Asks `client` how much is used, for `model`, in the background:
    /// unless it can't be asked, or was asked less than a minute ago.
    pub(crate) fn ask(&mut self, client: &Client, model: &str) {
        if !client.asks_meters() || !self.asking.is_empty() || self.asked.is_some_and(|at| at.elapsed() < EVERY) {
            return;
        }
        self.asked = Some(Instant::now());
        let (client, model) = (client.clone(), model.to_owned());
        self.asking
            .spawn(async move { tokio::time::timeout(PATIENCE, client.meters(&model)).await.unwrap_or_else(|_| Err(rein_llm::Error::Incomplete("no answer in time".into()))) });
    }

    /// The provider's answer, once it comes; never, while nothing's asked.
    /// One that didn't come leaves what was known: an old figure beats none.
    pub(crate) async fn answered(&mut self) -> Vec<Meter> {
        loop {
            match self.asking.join_next().await {
                Some(Ok(Ok(meters))) => return meters,
                Some(_) => {}
                None => std::future::pending().await,
            }
        }
    }

    /// What the bar shows, at `now`: the shortest and the longest of the
    /// plan's windows, one that's only for some models once it's close,
    /// and what isn't a window, a budget or what's spent. What's past its
    /// reset isn't shown: it said what it did before it started over.
    pub(crate) fn bar(&self, s: Style, now: SystemTime) -> Vec<String> {
        self.shown(now).into_iter().map(|meter| shown_in_bar(meter, s, now)).collect()
    }

    /// What's shown, for a client to draw: what the bar has, as data.
    pub(crate) fn cells(&self, now: SystemTime) -> Vec<Cell> {
        let cell = |meter: &Meter| {
            let (label, said) = match meter.reading {
                Reading::Used(percent) => (meter.label(), format!("{percent}%")),
                _ => (figure(meter), String::new()),
            };
            let tone = tone(meter);
            Cell { label, used: meter.reading.used(), said, resets: (tone != Standing::Fine).then(|| meter.resets.map(|at| when(at, now))).flatten(), tone }
        };
        self.shown(now).into_iter().map(cell).collect()
    }

    fn shown(&self, now: SystemTime) -> Vec<&Meter> {
        let live: Vec<&Meter> = self.known.iter().filter(|meter| meter.resets.is_none_or(|at| at > now)).collect();
        let windows: Vec<&Meter> = live.iter().copied().filter(|meter| matches!(meter.reading, Reading::Used(_)) && meter.only.is_none()).collect();
        let mut shown: Vec<&Meter> = Vec::new();
        for meter in [windows.iter().min_by_key(|meter| meter.span), windows.iter().max_by_key(|meter| meter.span)].into_iter().flatten() {
            if !shown.iter().any(|known| known.id == meter.id) {
                shown.push(meter);
            }
        }
        for meter in live {
            let window = matches!(meter.reading, Reading::Used(_));
            if (!window || meter.only.is_some() && tone(meter) > Standing::Fine) && !shown.iter().any(|known| known.id == meter.id) {
                shown.push(meter);
            }
        }
        shown
    }

    /// Every meter in full, for `/status`.
    pub(crate) fn status(&self, now: SystemTime) -> String {
        let all: Vec<String> = self
            .known
            .iter()
            .filter(|meter| meter.resets.is_none_or(|at| at > now))
            .map(|meter| match meter.resets {
                Some(at) => format!("{} (resets {})", figure(meter), when(at, now)),
                None => figure(meter),
            })
            .collect();
        if all.is_empty() { "none said".to_owned() } else { all.join(" · ") }
    }
}

/// How a meter is shown to stand: as the provider says, and closer still
/// when most of it's used, whatever the provider says.
fn tone(meter: &Meter) -> Standing {
    let by_reading = match meter.reading.used() {
        Some(90..) => Standing::Reached,
        Some(75..) => Standing::Warned,
        _ => Standing::Fine,
    };
    meter.standing.max(by_reading)
}

/// The meter as said in a few characters: `5h 28%`, `$4.20 left`.
fn figure(meter: &Meter) -> String {
    match meter.reading {
        Reading::Used(percent) => format!("{} {percent}%", meter.label()),
        Reading::Left { dollars, .. } => format!("${dollars:.2} left"),
        Reading::Spent(dollars) if meter.span == Some(Duration::from_secs(86_400)) => format!("${dollars:.2} today"),
        Reading::Spent(dollars) => format!("${dollars:.2} spent"),
        Reading::Requests { made, cap } => format!("{} {made}/{cap}", meter.only.clone().unwrap_or_else(|| meter.label())),
    }
}

/// The meter in the bar: grey while it's fine, yellow once it's close, red
/// once it's all but used, with when it starts over.
fn shown_in_bar(meter: &Meter, s: Style, now: SystemTime) -> String {
    let figure = figure(meter);
    let resets = meter.resets.map(|at| format!(" →{}", when(at, now))).unwrap_or_default();
    match tone(meter) {
        Standing::Fine => s.dim(&figure),
        Standing::Warned => s.yellow(&format!("{figure}{resets}")),
        Standing::Reached => s.red(&format!("{figure}{resets}")),
    }
}

/// The line said when `meter` gets close or is reached.
fn warning(meter: &Meter, now: SystemTime) -> String {
    let resets = meter.resets.map(|at| format!(" · resets {}", when(at, now))).unwrap_or_default();
    let reached = tone(meter) == Standing::Reached && meter.standing == Standing::Reached;
    let said = match meter.reading {
        Reading::Used(_) if reached => format!("your {} limit is reached", meter.label()),
        Reading::Used(percent) => format!("{percent}% of your {} limit is used", meter.label()),
        Reading::Left { dollars, cap } => format!("${dollars:.2} left of ${cap:.2}"),
        Reading::Requests { made, cap } => format!("{made} of {cap} {} requests used", meter.only.as_deref().unwrap_or("allowed")),
        Reading::Spent(dollars) => format!("${dollars:.2} spent"),
    };
    format!("{said}{resets}")
}

/// When `at` is, as near as it needs saying: `21:00` today, `Fri 17:20`
/// within the week, `Nov 12` further off.
fn when(at: SystemTime, now: SystemTime) -> String {
    let (then, today) = (local_time(at), local_time(now));
    let ahead = at.duration_since(now).unwrap_or_default();
    if then.date() == today.date() {
        then.clock()
    } else if ahead < Duration::from_secs(6 * 86_400) {
        format!("{} {}", then.weekday(), then.clock())
    } else {
        then.month_day()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3600;
    const DAY: u64 = 24 * HOUR;

    fn window(id: &str, span: u64, used: u8, standing: Standing) -> Meter {
        Meter { id: id.into(), only: None, span: Some(Duration::from_secs(span)), resets: None, reading: Reading::Used(used), standing }
    }

    fn bar(meters: &Meters, now: SystemTime) -> Vec<String> {
        meters.bar(Style::plain(), now)
    }

    #[test]
    fn the_bar_shows_the_shortest_and_longest_window_and_a_models_own_once_its_close() {
        let now = SystemTime::now();
        let mut meters = Meters::default();
        let opus = |used, standing| Meter { only: Some("Opus".into()), ..window("seven_day_opus", 7 * DAY, used, standing) };
        meters.read(vec![window("five_hour", 5 * HOUR, 28, Standing::Fine), window("seven_day", 7 * DAY, 40, Standing::Fine), opus(50, Standing::Fine)], now);
        assert_eq!(bar(&meters, now), ["5h 28%", "7d 40%"]);
        meters.read(vec![opus(80, Standing::Warned)], now);
        assert_eq!(bar(&meters, now), ["5h 28%", "7d 40%", "7d Opus 80%"]);
        let mut one = Meters::default();
        one.read(vec![window("five_hour", 5 * HOUR, 3, Standing::Fine)], now);
        assert_eq!(bar(&one, now), ["5h 3%"], "one window, once");
        assert!(bar(&Meters::default(), now).is_empty(), "a provider that says nothing shows nothing");
    }

    #[test]
    fn a_window_past_its_reset_goes_from_the_bar() {
        let now = SystemTime::now();
        let mut meters = Meters::default();
        let gone = Meter { resets: Some(now - Duration::from_secs(1)), ..window("five_hour", 5 * HOUR, 99, Standing::Warned) };
        let later = Meter { resets: Some(now + Duration::from_secs(HOUR)), ..window("seven_day", 7 * DAY, 10, Standing::Fine) };
        meters.read(vec![gone, later], now);
        assert_eq!(bar(&meters, now), ["7d 10%"], "never an old 99%");
    }

    #[test]
    fn what_isnt_a_window_shows_too_and_one_close_says_when_it_starts_over() {
        let now = SystemTime::now();
        let mut meters = Meters::default();
        let budget = Meter {
            id: "budget".into(),
            only: None,
            span: None,
            resets: Some(now + Duration::from_secs(90)),
            reading: Reading::Left { dollars: 1.5, cap: 10.0 },
            standing: Standing::Warned,
        };
        let spent = Meter { id: "spent_today".into(), only: None, span: Some(Duration::from_secs(DAY)), resets: None, reading: Reading::Spent(0.4242), standing: Standing::Fine };
        let free = Meter {
            id: "free".into(),
            only: Some("free".into()),
            span: Some(Duration::from_secs(DAY)),
            resets: None,
            reading: Reading::Requests { made: 12, cap: 50 },
            standing: Standing::Fine,
        };
        meters.read(vec![budget, spent, free], now);
        let resets = when(now + Duration::from_secs(90), now);
        assert_eq!(bar(&meters, now), [format!("$1.50 left →{resets}"), "$0.42 today".into(), "free 12/50".into()]);
    }

    #[test]
    fn a_meter_is_coloured_by_how_close_it_is_whatever_the_provider_says() {
        assert_eq!(tone(&window("w", HOUR, 50, Standing::Fine)), Standing::Fine);
        assert_eq!(tone(&window("w", HOUR, 50, Standing::Warned)), Standing::Warned, "the provider's warning");
        assert_eq!(tone(&window("w", HOUR, 76, Standing::Fine)), Standing::Warned, "most of it used");
        assert_eq!(tone(&window("w", HOUR, 92, Standing::Warned)), Standing::Reached, "all but used");
        let now = SystemTime::now();
        let mut meters = Meters::default();
        meters.read(vec![window("w", 5 * HOUR, 92, Standing::Warned)], now);
        assert_eq!(meters.bar(Style::colored(), now), ["\x1b[31m5h 92%\x1b[0m"]);
    }

    #[test]
    fn a_warning_is_said_once_as_it_gets_closer_and_again_after_a_reset() {
        let now = SystemTime::now();
        let mut meters = Meters::default();
        assert!(meters.read(vec![window("seven_day", 7 * DAY, 40, Standing::Fine)], now).is_empty());
        assert_eq!(meters.read(vec![window("seven_day", 7 * DAY, 75, Standing::Warned)], now), ["75% of your 7d limit is used"]);
        assert!(meters.read(vec![window("seven_day", 7 * DAY, 77, Standing::Warned)], now).is_empty(), "once, not with every step");
        assert_eq!(meters.read(vec![window("seven_day", 7 * DAY, 100, Standing::Reached)], now), ["your 7d limit is reached"]);
        assert!(meters.read(vec![window("seven_day", 7 * DAY, 2, Standing::Fine)], now).is_empty(), "started over");
        assert_eq!(meters.read(vec![window("seven_day", 7 * DAY, 80, Standing::Warned)], now).len(), 1, "and said again when it gets close again");
        assert_eq!(meters.read(vec![window("five_hour", 5 * HOUR, 95, Standing::Fine)], now), ["95% of your 5h limit is used"], "all but used, not reached till the provider says");
    }

    #[test]
    fn a_reset_is_said_as_near_as_it_needs() {
        let now = SystemTime::now();
        let today = local_time(now);
        // An hour on may be tomorrow, near midnight; either way it reads as a time.
        let soon = when(now + Duration::from_secs(HOUR), now);
        assert!(soon.len() == 5 || soon.len() == 9, "{soon}");
        assert!(when(now + Duration::from_secs(3 * DAY), now).ends_with(&local_time(now + Duration::from_secs(3 * DAY)).clock()));
        assert_eq!(when(now + Duration::from_secs(20 * DAY), now), local_time(now + Duration::from_secs(20 * DAY)).month_day());
        assert_eq!(when(now, now), today.clock());
    }

    #[test]
    fn everything_known_is_in_the_status_with_when_it_starts_over() {
        let now = SystemTime::now();
        let mut meters = Meters::default();
        assert_eq!(meters.status(now), "none said");
        let later = now + Duration::from_secs(HOUR);
        meters.read(vec![Meter { resets: Some(later), ..window("five_hour", 5 * HOUR, 28, Standing::Fine) }, window("seven_day", 7 * DAY, 75, Standing::Warned)], now);
        assert_eq!(meters.status(now), format!("5h 28% (resets {}) · 7d 75%", when(later, now)));
        meters.forget();
        assert_eq!(meters.status(now), "none said");
    }
}
