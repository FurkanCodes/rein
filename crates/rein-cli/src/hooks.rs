//! Your hooks, in the conversation: found as rein starts, said in a line,
//! and `/hooks`, to see each, what it runs on and how it last did, and to
//! turn one off or on. What you choose is kept in rein's folder.

use std::path::Path;
use std::sync::Arc;

use rein_agent::agent::{Agent, Llm};
use rein_agent::hooks::{Chosen, EventName, Hook, Hooks, Source, State};

use crate::CliResult;
use crate::input::Menu;
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// What you chose of hooks, in rein's folder.
const CHOSEN_FILE: &str = "hooks-chosen.json";

/// Your hooks, rein's and Claude Code's, its plugins', and the project at
/// `root`'s, as you chose them in rein's folder `dir`.
pub(crate) fn load(dir: &Path, root: &Path) -> Arc<Hooks> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    Arc::new(Hooks::load(dir, home.as_deref(), root, &chosen(dir)))
}

fn chosen(dir: &Path) -> Chosen {
    std::fs::read(dir.join(CHOSEN_FILE)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Gives `agent` its hooks and runs those on the session's start, from
/// `source`, saying in a line which run.
pub(crate) async fn start(agent: &mut Agent<Llm>, terminal: &mut Terminal, dir: &Path, source: Source) {
    let hooks = load(dir, agent.tools.root());
    if let Some(said) = said(&hooks) {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&said));
    }
    agent.set_hooks(hooks);
    agent.session_started(source, terminal).await;
}

/// The hooks there are, in a line: those that run, and those that don't
/// and why, briefly. `None` with none.
fn said(hooks: &Hooks) -> Option<String> {
    let all = hooks.all();
    if all.is_empty() && hooks.left_out().is_empty() {
        return None;
    }
    // Each by its name once: when those that run do, and why those that
    // don't, as the first of them says.
    let mut names: Vec<String> = Vec::new();
    for hook in all {
        if !names.contains(&hook.name()) {
            names.push(hook.name());
        }
    }
    let mut parts = Vec::new();
    for name in names {
        let mine: Vec<&Hook> = all.iter().filter(|hook| hook.name() == name).collect();
        let mut whens: Vec<String> = mine.iter().filter(|hook| hook.runs()).map(|hook| when(hook)).collect();
        whens.dedup();
        match mine.iter().find(|hook| !hook.runs()) {
            Some(off) if whens.is_empty() => parts.push(format!("{name} off ({})", why_off(&off.state))),
            _ => parts.push(format!("{name} {}", whens.join(" and "))),
        }
    }
    if !hooks.left_out().is_empty() {
        let count = hooks.left_out().len();
        parts.push(format!("{count} rein can't run"));
    }
    Some(format!("Hooks: {} · /hooks", parts.join(", ")))
}

/// When `hook` runs, in a few words.
fn when(hook: &Hook) -> String {
    let what = hook.matcher.as_deref().map_or_else(String::new, |matcher| format!(" {matcher}"));
    let tool = |what: String| if what.is_empty() { " each tool".to_owned() } else { what };
    let event = match hook.event {
        EventName::PreToolUse => return format!("before{}", tool(what)),
        EventName::PostToolUse => return format!("after{}", tool(what)),
        EventName::PostToolUseFailure => return format!("after{} fails", tool(what)),
        EventName::PermissionRequest => "as rein asks you",
        EventName::UserPromptSubmit => "on your messages",
        EventName::SessionStart => "as a session starts",
        EventName::SessionEnd => "as a session ends",
        EventName::Stop => "as a turn ends",
        EventName::StopFailure => "as a turn fails",
        EventName::Notification => "when rein needs you, away",
        EventName::PreCompact => "before a compaction",
        EventName::PostCompact => "after a compaction",
        EventName::SubagentStart => "as a sub-agent starts",
        EventName::SubagentStop => "as a sub-agent ends",
    };
    event.to_owned()
}

fn why_off(state: &State) -> String {
    match state {
        State::On => String::new(),
        State::Off => "you turned it off".into(),
        State::ReinDoesIt(why) => (*why).to_owned(),
        State::NotRun(why) => why.clone(),
    }
}

/// `/hooks`: every hook, where it's set, what it runs on, and how it last
/// did; pick one to turn it off, or on. Those rein can't run are at the
/// end, with why.
pub(crate) async fn command(agent: &mut Agent<Llm>, terminal: &mut Terminal, dir: &Path) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let hooks = agent.hooks();
    if hooks.all().is_empty() && hooks.left_out().is_empty() {
        say!("{pad}No hooks: rein runs those in ~/.config/rein/hooks.json, Claude Code's ~/.claude/settings.json and its plugins'");
        return Ok(());
    }
    // Those that run first.
    let mut order: Vec<usize> = (0..hooks.all().len()).collect();
    order.sort_by_key(|&i| !hooks.all()[i].runs());
    let mut items: Vec<(String, String)> = order
        .iter()
        .map(|&i| &hooks.all()[i])
        .map(|hook| {
            let state = if hook.runs() { "on".to_owned() } else { format!("off: {}", why_off(&hook.state)) };
            let last = hooks.last(hook).map(|last| format!(" · last {} ({} ms)", last.came, last.took.as_millis())).unwrap_or_default();
            (format!("{} {}", hook.name(), when(hook)), format!("{state} · {} · {}{last}", hook.from.describe(), command_line(hook)))
        })
        .collect();
    let left_out = hooks.left_out().to_vec();
    if !left_out.is_empty() {
        items.push(("can't run".into(), format!("{} that rein can't run, and why", left_out.len())));
    }
    let count = hooks.all().len();
    let Some(picked) = terminal.input.choose(Menu { title: "hooks: pick one to turn off or on".into(), items, current: None, at: None }).await else { return Ok(()) };
    if picked >= count {
        for left in &left_out {
            say!("{pad}{}", s.dim(&format!("– {} in {}: {}", left.what, left.from, left.why)));
        }
        return Ok(());
    }
    let hook = agent.hooks().all()[order[picked]].clone();
    let id = hook.id();
    let mut chosen = chosen(dir);
    let on = match &hook.state {
        State::On => {
            chosen.off.push(id.clone());
            chosen.on.retain(|other| *other != id);
            false
        }
        State::Off | State::ReinDoesIt(_) => {
            chosen.off.retain(|other| *other != id);
            if matches!(hook.state, State::ReinDoesIt(_)) {
                chosen.on.push(id.clone());
            }
            true
        }
        State::NotRun(why) => {
            say!("{pad}{}", s.dim(&format!("{} doesn't run: {why}", hook.name())));
            return Ok(());
        }
    };
    rein_llm::store::write_json_private(&dir.join(CHOSEN_FILE), &chosen).map_err(|e| format!("can't write {}: {e}", dir.join(CHOSEN_FILE).display()))?;
    agent.set_hooks(load(dir, agent.tools.root()));
    say!("{pad}{} {} is {} now", s.green("✓"), hook.name(), if on { "on" } else { "off" });
    Ok(())
}

/// What `hook` runs, as written.
fn command_line(hook: &Hook) -> String {
    match &hook.command.args {
        Some(args) => format!("{} {}", hook.command.line, args.join(" ")),
        None => hook.command.line.clone(),
    }
}
