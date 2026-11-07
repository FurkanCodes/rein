//! What runs without asking, in words: the mode, and the sandbox, which
//! is yours to turn on or off apart from it. And whether new conversations
//! start with what `/mode` or `/sandbox` picked, kept in your settings if
//! you say so.

use crate::input::Question;
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;
use crate::{CliResult, config_dir};
use rein_agent::permissions::{Mode, Sandboxing};
use rein_agent::sandbox::Sandbox;

/// What marks a mode with nothing to hold a command back.
const DANGER: &str = "⚠";

/// Whether commands run unasked with nothing to hold them: `trust`, where
/// the model can go past the sandbox too, and `auto` with none on. Marked
/// wherever such a mode is named, as rein starts in one.
pub(crate) fn dangerous(mode: Mode, sandboxing: Sandboxing) -> bool {
    mode == Mode::Trust || (mode == Mode::Auto && sandboxing == Sandboxing::Off)
}

/// `text`, about `mode`, marked if that's dangerous.
fn marked(mode: Mode, sandboxing: Sandboxing, text: &str) -> String {
    if dangerous(mode, sandboxing) { format!("{DANGER} {text}") } else { text.to_owned() }
}

/// `mode`'s name, as the bar and `/mode` show it.
pub(crate) fn shown_name(mode: Mode, sandboxing: Sandboxing) -> String {
    marked(mode, sandboxing, mode.as_str())
}

/// [`description`], as it's said on screen.
pub(crate) fn said(mode: Mode, sandboxing: Sandboxing) -> String {
    marked(mode, sandboxing, description(mode, sandboxing))
}

/// What `mode` lets through, `name: what`, as it is with the sandbox
/// on, off, or on with none here.
pub(crate) fn description(mode: Mode, sandboxing: Sandboxing) -> &'static str {
    use Sandboxing::{Missing, Off, On};
    match (mode, sandboxing) {
        (Mode::Ask, On) => "ask: file changes ask first, and so do commands that do more than look in the sandbox",
        (Mode::Ask, Off | Missing) => "ask: file changes and commands ask first",
        (Mode::Edits, On) => "edits: file changes in this folder and commands in the sandbox don't ask; going past it does",
        (Mode::Edits, Off | Missing) => "edits: file changes in this folder don't ask; commands do",
        (Mode::Auto, On) => "auto: nothing asks; commands stay in the sandbox, and what would go past it is refused",
        (Mode::Auto, Off) => "auto: nothing asks, and every command runs, without the sandbox; what ask rules name is refused",
        (Mode::Auto, Missing) => "auto: nothing asks; file changes in this folder go ahead, and only the commands your rules allow run, as there's no sandbox here",
        (Mode::ReadOnly, On) => "read-only: commands only look, in the sandbox, and nothing changes",
        (Mode::ReadOnly, Off | Missing) => "read-only: nothing changes, and commands that only look ask first",
        (Mode::Trust, On) => {
            "trust: everything runs without asking, commands in the sandbox within what each says it needs, but what ask rules name, like deleting, asks, and deny rules stop what they name"
        }
        (Mode::Trust, Off) => "trust: everything runs without asking, but what ask rules name, like deleting, asks, and deny rules stop what they name",
        (Mode::Trust, Missing) => {
            "trust: everything runs without asking but what ask rules name, like deleting, and commands meant for the sandbox, which isn't here; deny rules stop what they name"
        }
    }
}

/// What `mode` lets through, in a sentence for a list that has room for a
/// line under each name. With the sandbox on and nowhere to run, it's said
/// in full: that's the case a short line would get wrong.
pub(crate) fn brief(mode: Mode, sandboxing: Sandboxing) -> &'static str {
    use Sandboxing::{Missing, Off, On};
    match (mode, sandboxing) {
        (_, Missing) => description(mode, sandboxing).split_once(": ").map_or("", |(_, about)| about),
        (Mode::Trust, On) => "Everything runs, in the sandbox unless a command needs to leave it.",
        (Mode::Ask, On) => "Asks before changing files, or writing from a command. Looks run.",
        (Mode::Edits, On) => "Changes files. Asks before a command needs the network or to leave the sandbox.",
        (Mode::Auto, On) => "Never asks. What would go past the walls is refused.",
        (Mode::ReadOnly, On) => "No file changes. Commands that only look run; the rest are refused.",
        (Mode::Trust, Off) => "Everything runs, except what your rules hold back.",
        (Mode::Ask, Off) => "Asks before file changes and commands.",
        (Mode::Edits, Off) => "Changes files in this folder. Asks before commands.",
        (Mode::Auto, Off) => "Never asks. Every command runs, except what an ask rule names.",
        (Mode::ReadOnly, Off) => "No file changes. Commands that only look ask first; the rest are refused.",
    }
}

/// What's said even where a switch shows the sandbox: on, with nothing to
/// run in, the switch alone would have you think commands have walls.
pub(crate) fn walls_missing(sandboxing: Sandboxing, sandbox: &Sandbox) -> Option<String> {
    (sandboxing == Sandboxing::Missing).then(|| sandbox_now(sandboxing, sandbox))
}

/// The sandbox now, in words, as `/sandbox` sets it.
pub(crate) fn sandbox_now(sandboxing: Sandboxing, sandbox: &Sandbox) -> String {
    match sandboxing {
        Sandboxing::On => format!("sandbox on: commands run in it, within what each says it needs ({})", sandbox.describe()),
        Sandboxing::Off => "sandbox off: commands run as you'd run them, and the mode decides what asks".to_owned(),
        Sandboxing::Missing => format!("sandbox on, but {}: commands ask before they run", sandbox.describe()),
    }
}

/// The same, for `/status`.
pub(crate) fn sandbox_status(sandboxing: Sandboxing, sandbox: &Sandbox) -> String {
    match sandboxing {
        Sandboxing::On => format!("on: {}", sandbox.describe()),
        Sandboxing::Off => "off: commands run as you'd run them · /sandbox turns it on".to_owned(),
        Sandboxing::Missing => format!("on, but {}: commands ask before they run", sandbox.describe()),
    }
}

/// After `/mode`: whether new conversations start in `mode` too, asked
/// when it isn't the one they start in already, and kept in your settings
/// if you say so.
pub(crate) async fn keep_mode(mode: Mode, terminal: &mut Terminal) -> CliResult {
    let kept = |settings: &crate::settings::Settings| settings.in_file("mode").and_then(|name| name.parse::<Mode>().ok()).unwrap_or_default();
    let keep = Keep {
        key: "mode",
        value: mode.as_str(),
        env: "REIN_MODE",
        title: format!("Start in {mode} from now on?"),
        now: |kept| format!("New conversations start in {kept} now."),
        yes: format!("Yes, start in {mode}"),
        done: format!("new conversations start in {mode}"),
    };
    keep_setting(keep, |settings| kept(settings).as_str().to_owned(), terminal).await
}

/// After `/sandbox`: whether new conversations start with it `on` too,
/// asked and kept the same way.
pub(crate) async fn keep_sandbox(on: bool, terminal: &mut Terminal) -> CliResult {
    let word = if on { "on" } else { "off" };
    let keep = Keep {
        key: "sandbox",
        value: word,
        env: "REIN_SANDBOX",
        title: format!("Keep the sandbox {word} from now on?"),
        now: |kept| format!("New conversations start with it {kept} now."),
        yes: format!("Yes, start with it {word}"),
        done: format!("new conversations start with the sandbox {word}"),
    };
    // Off unless the file says on, as when rein starts.
    let kept = |settings: &crate::settings::Settings| if settings.in_file("sandbox") == Some("on") { "on" } else { "off" }.to_owned();
    keep_setting(keep, kept, terminal).await
}

/// A setting a command changed, and how to ask whether to keep it.
struct Keep {
    key: &'static str,
    /// As the settings file has it.
    value: &'static str,
    /// The variable that wins over the file.
    env: &'static str,
    title: String,
    /// What new conversations start with now, said with what's kept.
    now: fn(&str) -> String,
    yes: String,
    done: String,
}

/// Asks whether to keep `keep` for new conversations, unless they start
/// with it already (`kept` says what they start with), and keeps it in
/// your settings if you say so.
async fn keep_setting(keep: Keep, kept: impl Fn(&crate::settings::Settings) -> String, terminal: &mut Terminal) -> CliResult {
    let path = config_dir()?.join(crate::settings::FILE);
    let settings = crate::settings::Settings::load(&path)?;
    let kept = kept(&settings);
    if kept == keep.value {
        return Ok(());
    }
    let question = Question {
        title: keep.title,
        detail: Some(format!("{} Yes keeps {} = {} in {}.", (keep.now)(&kept), keep.key, keep.value, crate::rules::shown(&path))),
        command: None,
        // No first: Enter out of habit keeps nothing.
        answers: vec![("n", "No, just this conversation".to_owned()), ("y", keep.yes)],
    };
    if !terminal.yes(question).await {
        return Ok(());
    }
    crate::settings::set(&path, keep.key, keep.value)?;
    let pad = " ".repeat(GUTTER);
    say!("{pad}{} {}", terminal.err.green("✓"), keep.done);
    // Said, as it would otherwise look like it didn't take.
    if std::env::var(keep.env).is_ok_and(|value| !value.is_empty()) {
        say!("{pad}{}", terminal.err.dim(&format!("({} in your environment still wins over the settings)", keep.env)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_said_briefly_is_a_sentence_and_with_no_sandbox_here_its_said_whole() {
        for mode in Mode::ALL {
            for sandboxing in [Sandboxing::On, Sandboxing::Off] {
                let said = brief(mode, sandboxing);
                assert!(said.ends_with('.') && said.chars().next().is_some_and(char::is_uppercase), "{mode} {sandboxing:?}: {said}");
            }
            let whole = description(mode, Sandboxing::Missing);
            assert!(whole.ends_with(brief(mode, Sandboxing::Missing)), "{mode}: the case a short line would get wrong");
        }
    }

    #[test]
    fn a_sandbox_that_isnt_here_is_said_though_its_switch_shows_on() {
        use rein_agent::sandbox::{Missing, Places};
        let sandbox = Sandbox::missing(Places::new(std::path::Path::new("/tmp"), None, None), Missing::NotHere);
        let said = walls_missing(Sandboxing::Missing, &sandbox).expect("a switch alone would say you have walls");
        assert!(said.contains("commands ask before they run"), "{said}");
        assert_eq!(walls_missing(Sandboxing::On, &sandbox), None);
        assert_eq!(walls_missing(Sandboxing::Off, &sandbox), None);
    }

    #[test]
    fn only_a_mode_with_nothing_to_hold_a_command_back_is_marked() {
        use Sandboxing::{Missing, Off, On};
        let mut marked = Vec::new();
        for mode in Mode::ALL {
            for sandboxing in [On, Off, Missing] {
                if dangerous(mode, sandboxing) {
                    marked.push((mode, sandboxing));
                }
            }
        }
        assert_eq!(marked, [(Mode::Trust, On), (Mode::Trust, Off), (Mode::Trust, Missing), (Mode::Auto, Off)]);
        assert_eq!(shown_name(Mode::Trust, On), "⚠ trust");
        assert_eq!(shown_name(Mode::Ask, Off), "ask");
        assert!(said(Mode::Auto, Off).starts_with("⚠ auto: nothing asks"), "{}", said(Mode::Auto, Off));
        assert!(said(Mode::Auto, On).starts_with("auto: nothing asks"), "{}", said(Mode::Auto, On));
    }
}
