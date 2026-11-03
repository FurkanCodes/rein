//! rein's wordmark, kept at the top of the screen, with what it's using
//! beside it, in the strip's greens:
//!
//! ```text
//! █▀▀▄ █▀▀▀ █ █▄  █   gpt-6.1-sol · ChatGPT plan · v0.1.0
//! █▀█  █▀▀  █ █ ▀▄█   ~/projects/rein · main
//! ▀  ▀ ▀▀▀▀ ▀ ▀   ▀   AGENTS.md, CLAUDE.md
//! ```
//!
//! Where what it says doesn't fit beside it, under it; in a window too
//! narrow for the wordmark, only what it says.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::peers::strip::Colors;
use crate::ui::style::{Style, fit, width};

/// What the logo says, once it's up: filled in as rein finds it out.
#[derive(Debug, Clone, Default)]
pub(crate) struct Facts {
    /// The model, as shown.
    pub(crate) model: Option<String>,
    /// What pays for it, in a few words: `ChatGPT plan`.
    pub(crate) pays: Option<String>,
    /// The folder, `~` for home.
    pub(crate) place: String,
    pub(crate) branch: Option<String>,
    /// The instruction files read, as shown.
    pub(crate) instructions: Vec<String>,
}

impl Facts {
    /// What the logo says, a line each: what runs it and which rein,
    /// where, and by what instructions. Only what's known.
    fn details(&self) -> Vec<String> {
        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        let running: Vec<&str> = self.model.as_deref().into_iter().chain(self.pays.as_deref()).chain([version.as_str()]).collect();
        let place: Vec<&str> = [self.place.as_str()].into_iter().chain(self.branch.as_deref()).collect();
        [running.join(" · "), place.join(" · "), self.instructions.join(", ")].into_iter().filter(|line| !line.is_empty()).collect()
    }
}

/// The logo's facts, while it's shown.
static SHOWN: Mutex<Option<Facts>> = Mutex::new(None);

fn shown() -> MutexGuard<'static, Option<Facts>> {
    SHOWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Puts the logo at the top of the screen, for a conversation in `root`.
pub(crate) fn show(root: &Path, model: Option<&str>) {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let facts = Facts { model: model.map(model_name), place: home_relative(root, home.as_deref()), branch: crate::peers::git_branch(root), ..Facts::default() };
    *shown() = Some(facts);
    publish();
}

/// Changes what the logo says. `false` if it isn't shown, so whoever
/// wanted it said can say it another way.
pub(crate) fn update(change: impl FnOnce(&mut Facts)) -> bool {
    let mut shown = shown();
    let Some(facts) = shown.as_mut() else { return false };
    change(facts);
    drop(shown);
    publish();
    true
}

/// The logo is up.
pub(crate) fn is_shown() -> bool {
    shown().is_some()
}

/// `gpt-6.1-sol` of `openai/gpt-6.1-sol`.
pub(crate) fn model_name(spec: &str) -> String {
    spec.rsplit_once('/').map_or(spec, |(_, name)| name).to_owned()
}

/// What pays, in a few words: `ChatGPT plan` of `Using ChatGPT plan
/// (manage usage: …)`, `OPENAI_API_KEY` of `Using OPENAI_API_KEY with …`.
pub(crate) fn pays_briefly(pays: &str) -> String {
    let pays = pays.strip_prefix("Using ").unwrap_or(pays);
    let pays = pays.split(" (").next().unwrap_or(pays);
    pays.split(" with ").next().unwrap_or(pays).to_owned()
}

fn publish() {
    let Some(facts) = shown().clone() else { return };
    let style = Style::stderr();
    let colors = Colors::here(style.on());
    crate::ui::screen::set_header(move |columns, ruled| lines(&facts, columns, ruled, colors, |text| style.dim(text)));
}

/// The wordmark: letters five pixels tall, in three rows of half blocks
/// (`▀` a cell's top pixel, `▄` its bottom, `█` both), the least that
/// gives an E its three bars with a gap between each.
const MARK: [&str; 3] = ["█▀▀▄ █▀▀▀ █ █▄  █", "█▀█  █▀▀  █ █ ▀▄█", "▀  ▀ ▀▀▀▀ ▀ ▀   ▀"];
/// Between the wordmark and what's beside it.
const GAP: usize = 3;
/// The least room beside the wordmark worth putting anything in.
const ROOM: usize = 24;

/// `text`, at most `most` columns: its end, after `…`, if it's longer,
/// from the start of a folder's name where it has one.
fn tail(text: &str, most: usize) -> String {
    if width(text) <= most {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let keep = most.saturating_sub(1).min(chars.len());
    let kept: String = chars[chars.len() - keep..].iter().collect();
    // From a folder's start, not partway into its name.
    match kept.find('/') {
        Some(slash) => format!("…{}", &kept[slash..]),
        None => format!("…{kept}"),
    }
}

/// The greens the wordmark and its line run through, left to right: the
/// strip's working green into a cooler one.
const FROM: (u8, u8, u8) = (70, 220, 120);
const TO: (u8, u8, u8) = (40, 165, 200);

/// The colour `at` of the way along `span` columns.
fn along(at: usize, span: usize) -> (u8, u8, u8) {
    let t = if span <= 1 { 0.0 } else { at as f64 / (span - 1) as f64 };
    let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
    (mix(FROM.0, TO.0), mix(FROM.1, TO.1), mix(FROM.2, TO.2))
}

/// `text`, each column a step further along the greens, over `span`.
fn tinted(text: &str, span: usize, colors: Colors) -> String {
    if colors == Colors::None {
        return text.to_owned();
    }
    text.chars().enumerate().map(|(x, c)| if c == ' ' { " ".to_owned() } else { colors.paint(along(x, span), true, &c.to_string()) }).collect()
}

/// The logo's lines, `columns` wide at most: the wordmark (none in a
/// window too narrow for it), what `facts` says beside it (or under it,
/// where that doesn't fit), and a line under all if `ruled`. `dim` greys
/// what it says.
pub(crate) fn lines(facts: &Facts, columns: usize, ruled: bool, colors: Colors, dim: impl Fn(&str) -> String) -> Vec<String> {
    let mark: &[&str] = if columns > width(MARK[0]) { &MARK } else { &[] };
    let details = facts.details();
    let wide = mark.first().map_or(0, |row| width(row));
    let room = columns.saturating_sub(1 + wide + GAP);
    let beside = details.len() <= mark.len() && room >= ROOM;
    // The first line keeps its start (the model), the others their end.
    let most = if beside { room } else { columns.saturating_sub(1) };
    let details: Vec<String> = details.iter().enumerate().map(|(n, d)| if n == 0 { fit(d, most) } else { tail(d, most) }).collect();
    let widest = details.iter().map(|d| width(d)).max().unwrap_or(0);
    let mut out = Vec::new();
    for (row, part) in mark.iter().enumerate() {
        let mut line = format!(" {}", tinted(part, wide, colors));
        if beside && let Some(detail) = details.get(row) {
            line.push_str(&" ".repeat(wide - width(part) + GAP));
            line.push_str(&dim(detail));
        }
        out.push(line.trim_end().to_owned());
    }
    if !beside {
        out.extend(details.iter().map(|d| format!(" {}", dim(d))));
    }
    let span = if beside { 1 + wide + GAP + widest } else { 1 + wide.max(widest) }.min(columns);
    if ruled {
        out.push(tinted(&"━".repeat(span), span, colors));
    }
    out
}

/// `path` with your home folder as `~`.
pub(crate) fn home_relative(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            model: Some("gpt-6.1-sol".into()),
            pays: Some("ChatGPT plan".into()),
            place: "~/projects/rein".into(),
            branch: Some("main".into()),
            instructions: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
        }
    }

    fn plain(facts: &Facts, columns: usize) -> Vec<String> {
        lines(facts, columns, true, Colors::None, str::to_owned)
    }

    const V: &str = env!("CARGO_PKG_VERSION");

    #[test]
    fn the_wordmark_has_what_rein_is_using_beside_it() {
        let shown = plain(&facts(), 100);
        assert_eq!(shown[0], format!(" █▀▀▄ █▀▀▀ █ █▄  █   gpt-6.1-sol · ChatGPT plan · v{V}"));
        assert_eq!(shown[1], " █▀█  █▀▀  █ █ ▀▄█   ~/projects/rein · main");
        assert_eq!(shown[2], " ▀  ▀ ▀▀▀▀ ▀ ▀   ▀   AGENTS.md, CLAUDE.md");
        assert_eq!(shown[3], "━".repeat(1 + 17 + 3 + format!("gpt-6.1-sol · ChatGPT plan · v{V}").chars().count()), "the line as wide as the logo");
        assert_eq!(shown.len(), 4, "three rows and the line");
    }

    #[test]
    fn where_it_doesnt_fit_beside_it_goes_under() {
        let shown = plain(&facts(), 40);
        assert_eq!(shown[..3], [" █▀▀▄ █▀▀▀ █ █▄  █", " █▀█  █▀▀  █ █ ▀▄█", " ▀  ▀ ▀▀▀▀ ▀ ▀   ▀"]);
        assert_eq!(shown[3..6], [format!(" gpt-6.1-sol · ChatGPT plan · v{V}"), " ~/projects/rein · main".to_owned(), " AGENTS.md, CLAUDE.md".to_owned()]);
        assert!(shown.iter().all(|line| width(line) <= 40), "{shown:?}");
    }

    #[test]
    fn a_window_too_narrow_for_the_wordmark_has_only_what_it_says() {
        let shown = plain(&facts(), 16);
        assert!(shown[0].starts_with(" gpt-6.1-sol"), "{shown:?}");
        assert!(!shown.iter().any(|line| line.contains('█')), "no wordmark: {shown:?}");
        assert!(shown.iter().all(|line| width(line) <= 16), "{shown:?}");
    }

    #[test]
    fn in_colour_it_runs_through_the_greens() {
        let shown = lines(&facts(), 100, true, Colors::True, str::to_owned);
        assert!(shown[0].starts_with(" \x1b[1;38;2;70;220;120m█"), "starts green: {:?}", shown[0]);
        assert!(shown[0].contains("\x1b[1;38;2;40;165;200m█\x1b[0m   "), "ends cooler: {:?}", shown[0]);
        assert_eq!(width(&shown[1]), width(&plain(&facts(), 100)[1]));
    }

    #[test]
    fn a_long_folder_keeps_its_end_and_the_first_line_its_model() {
        let long = Facts { place: "/private/var/folders/x/T/rein-screen-1-logo/project".into(), pays: Some("a very long way of paying for it all, really".into()), ..facts() };
        let shown = plain(&long, 70);
        assert!(shown[0].ends_with("   gpt-6.1-sol · a very long way of paying for it a…"), "{:?}", shown[0]);
        assert!(shown[1].ends_with("   …/folders/x/T/rein-screen-1-logo/project · main"), "{:?}", shown[1]);
        assert!(shown.iter().all(|line| width(line) <= 70), "{shown:?}");
    }

    #[test]
    fn it_says_only_what_it_knows_a_line_each() {
        let facts =
            Facts { model: Some(model_name("openai/gpt-6.1-sol")), pays: Some(pays_briefly("Using ChatGPT plan (manage usage: https://chatgpt.com/settings/usage)")), ..facts() };
        assert_eq!(facts.details(), [format!("gpt-6.1-sol · ChatGPT plan · v{V}"), "~/projects/rein · main".to_owned(), "AGENTS.md, CLAUDE.md".to_owned()]);
        let bare = Facts { place: "/srv/app".into(), ..Facts::default() };
        assert_eq!(bare.details(), [format!("v{V}"), "/srv/app".to_owned()], "only what's known");
        assert_eq!(pays_briefly("Using OPENAI_API_KEY with http://127.0.0.1:1/v1"), "OPENAI_API_KEY");
        assert_eq!(pays_briefly("Using your saved OpenRouter key"), "your saved OpenRouter key");
    }

    #[test]
    fn your_home_is_a_tilde() {
        let home = std::path::Path::new("/Users/me");
        assert_eq!(home_relative(std::path::Path::new("/Users/me/projects/rein"), Some(home)), "~/projects/rein");
        assert_eq!(home_relative(home, Some(home)), "~");
        assert_eq!(home_relative(std::path::Path::new("/srv/app"), Some(home)), "/srv/app");
    }
}
