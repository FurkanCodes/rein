//! The project's check after edits, in the conversation: found from the
//! project's files as rein starts, and asked about once, as it runs the
//! project's build scripts; `/check` to see it, turn it off or on, or run
//! yours. What you choose is kept in rein's folder, never the project's,
//! by the exact command: a different one asks again.

use std::collections::BTreeMap;
use std::path::Path;

use rein_agent::agent::{Agent, Llm};
use rein_agent::check::{Check, detect};
use serde::{Deserialize, Serialize};

use crate::CliResult;
use crate::input::Question;
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// What you chose for each project's check, in rein's folder.
const FILE: &str = "checks.json";

/// What the `check` setting says: find the project's (the default), none
/// anywhere, or yours everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Setting {
    #[default]
    Find,
    Off,
    Yours(String),
}

impl Setting {
    /// The setting's value, as the file or `$REIN_CHECK` gives it.
    pub(crate) fn of(value: Option<&str>) -> Setting {
        match value.map(str::trim) {
            None | Some("" | "on") => Setting::Find,
            Some("off") => Setting::Off,
            Some(command) => Setting::Yours(command.to_owned()),
        }
    }
}

/// What you chose for one project: its command, whether it runs, and
/// whether it's yours rather than one rein found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    command: String,
    on: bool,
    #[serde(default)]
    yours: bool,
}

fn key(root: &Path) -> String {
    root.display().to_string()
}

fn all(dir: &Path) -> BTreeMap<String, Kept> {
    rein_llm::store::read_json(&dir.join(FILE)).ok().flatten().unwrap_or_default()
}

fn keep(dir: &Path, root: &Path, kept: Kept) -> CliResult {
    let mut all = all(dir);
    all.insert(key(root), kept);
    let path = dir.join(FILE);
    rein_llm::store::write_json_private(&path, &all).map_err(|e| format!("can't keep that in {}: {}", path.display(), crate::describe(e)))
}

/// Gives `agent` the project's check, as `setting` says and you chose,
/// asking about one rein found when there's someone to `ask` and you
/// haven't said: a task run on its own, or a sub-agent, runs only what you
/// said yes to. Says in a line what checks.
pub(crate) async fn start(agent: &mut Agent<Llm>, terminal: &mut Terminal, dir: &Path, setting: &Setting, ask: bool) {
    let root = agent.tools.root().to_owned();
    let check = match setting {
        Setting::Off => None,
        Setting::Yours(command) => Some(Check::yours(command)),
        Setting::Find => chosen(dir, &root, terminal, ask).await,
    };
    if let Some(check) = &check {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&format!("Check after edits: {} · /check", about(check))));
    }
    agent.set_check(check);
}

/// The check you chose for the project at `root`, or the one rein found,
/// once you've said yes to it, asked now if you haven't and can be.
async fn chosen(dir: &Path, root: &Path, terminal: &mut Terminal, ask: bool) -> Option<Check> {
    let kept = all(dir).remove(&key(root));
    if let Some(kept) = &kept
        && kept.yours
    {
        return kept.on.then(|| Check::yours(&kept.command));
    }
    let found = detect(root)?;
    match kept {
        Some(kept) if kept.command == found.command => kept.on.then_some(found),
        _ if ask => {
            let yes = say_yes(&found, terminal).await;
            if let Err(e) = keep(dir, root, Kept { command: found.command.clone(), on: yes, yours: false }) {
                say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ {e}")));
            }
            yes.then_some(found)
        }
        _ => None,
    }
}

async fn say_yes(check: &Check, terminal: &mut Terminal) -> bool {
    let from = check.found_from.map(|file| format!(", found from {file}")).unwrap_or_default();
    let question = Question {
        title: format!("Check the project with {} after edits?", check.name()),
        detail: Some(format!(
            "After a step changes files, rein runs `{}`{from}, and tells the model what its changes broke, there and then. It runs the \
             project's own code (build scripts), in the sandbox when it's on. /check turns it off or on.",
            check.command
        )),
        command: None,
        // No first: what's in a cloned repository runs only once you choose.
        answers: vec![("n", "No, not in this project".to_owned()), ("y", "Yes, check after edits".to_owned())],
    };
    terminal.yes(question).await
}

/// `cargo check (found from Cargo.toml)`, or `make lint (yours)`.
fn about(check: &Check) -> String {
    match check.found_from {
        Some(file) => format!("{} (found from {file})", check.name()),
        None => format!("`{}` (yours)", check.command),
    }
}

/// `/check`: what checks, and how to change it; `/check on`, `/check off`,
/// or `/check <command>` for yours, kept for the project.
pub(crate) fn command(given: Option<&str>, agent: &mut Agent<Llm>, terminal: &mut Terminal, dir: &Path) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let root = agent.tools.root().to_owned();
    match given.map(str::trim) {
        None | Some("") => {
            let said = match agent.checking().check() {
                Some(check) if agent.checking().paused() => format!("Paused: {}. /check on runs it again", about(check)),
                Some(check) => format!("After edits, rein runs `{}`{}", check.command, check.found_from.map(|file| format!(", found from {file}")).unwrap_or_default()),
                None => match detect(&root) {
                    Some(found) => format!("Off. /check on runs {} after edits", about(&found)),
                    None => "Off, and rein doesn't know this project's check: /check <command> runs yours after edits".to_owned(),
                },
            };
            say!("{pad}{said}");
            say!("{pad}{}", s.dim("/check off, /check on, or /check <command>; check = off in your settings turns it off everywhere"));
        }
        Some("on") => {
            let check = match agent.checking().check() {
                Some(check) => check.clone(),
                None => detect(&root).ok_or("rein doesn't know this project's check: /check <command> runs yours")?,
            };
            keep(dir, &root, Kept { command: check.command.clone(), on: true, yours: check.found_from.is_none() })?;
            say!("{pad}{} after edits, rein runs {}", s.green("✓"), about(&check));
            agent.set_check(Some(check));
        }
        Some("off") => {
            let command = agent.checking().check().map(|check| check.command.clone()).or_else(|| detect(&root).map(|found| found.command));
            if let Some(command) = command {
                let yours = agent.checking().check().is_some_and(|check| check.found_from.is_none());
                keep(dir, &root, Kept { command, on: false, yours })?;
            }
            agent.set_check(None);
            say!("{pad}{} rein doesn't check this project after edits; /check on does again", s.green("✓"));
        }
        Some(command) => {
            let check = Check::yours(command);
            keep(dir, &root, Kept { command: check.command.clone(), on: true, yours: true })?;
            say!("{pad}{} after edits, rein runs `{}`, {}", s.green("✓"), check.command, reads(&check));
            agent.set_check(Some(check));
        }
    }
    Ok(())
}

/// How rein reads what a command of yours says.
fn reads(check: &Check) -> &'static str {
    match check.reads {
        rein_agent::check::Reads::ExitCode => "read by its exit code",
        _ => "its errors read as its program writes them",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_finds_turns_off_or_runs_yours() {
        assert_eq!(Setting::of(None), Setting::Find);
        assert_eq!(Setting::of(Some("on")), Setting::Find);
        assert_eq!(Setting::of(Some("off")), Setting::Off);
        assert_eq!(Setting::of(Some("make lint")), Setting::Yours("make lint".into()));
    }
}
