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
