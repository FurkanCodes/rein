//! What a settings page shows and changes: your settings file, read out
//! with what new conversations start with, and what this conversation
//! found (your rules and the project's, skills, hooks, memory, MCP servers).
//! A key set here is written to the file, as `/mode` keeps one, and checked
//! the way rein reads it as it starts, so what's written starts.

use rein_agent::agent::{Agent, Llm};
use rein_agent::permissions::{Kind, Mode, Whose};
use rein_llm::Effort;
use serde_json::{Value, json};

use crate::args::AskArgs;
use crate::settings::{self, Settings};

/// What the window does with `look`, the window's own setting.
const LOOKS: [&str; 3] = ["light", "dark", "follow"];

/// What new conversations start with, as `flags` and your settings say,
/// the file's own words beside, and the choices each takes.
pub(super) fn page(flags: &[String], found: &Value) -> Result<Value, String> {
    let dir = crate::config_dir()?;
    let path = dir.join(settings::FILE);
    let file = Settings::load(&path)?;
    let args = AskArgs::parse(flags.to_vec(), &file)?;
    let keys: Vec<Value> = settings::KEYS
        .iter()
        .map(|(key, env)| {
            // An environment variable that's set wins over the file.
            let over = std::env::var(env).ok().filter(|value| !value.is_empty()).map(|_| *env);
            json!({ "key": key, "file": file.in_file(key), "over": over })
        })
        .collect();
    let starts = json!({
        "model": args.model,
        "effort": args.effort.map(|effort| effort.as_str()),
        "search": args.search.to_string(),
        "mode": args.mode.as_str(),
        "sandbox": on(args.sandbox),
        "bell": on(args.bell),
        "memory": on(args.memory),
        "peers": file.get("peers").unwrap_or_else(|| "accept".to_owned()),
        "max_steps": args.max_steps.map_or_else(|| "off".to_owned(), |steps| steps.to_string()),
        "compact_at": file.get("compact_at").unwrap_or_else(|| rein_agent::compact::DEFAULT_LIMIT.to_string()),
        "check": file.get("check"),
        "look": file.get("look").unwrap_or_else(|| "follow".to_owned()),
    });
    let choices = json!({
        "effort": Effort::ALL.iter().map(|effort| effort.as_str()).collect::<Vec<_>>(),
        "mode": Mode::ALL.iter().map(|mode| json!({ "id": mode.as_str(), "about": crate::modes::brief(*mode, rein_agent::permissions::Sandboxing::Off) })).collect::<Vec<_>>(),
        "search": ["auto", "off"],
        "peers": ["accept", "hold", "refuse", "off"],
        "look": LOOKS,
        "on": ["on", "off"],
    });
    let mut page = json!({ "file": crate::rules::shown(&path), "keys": keys, "starts": starts, "choices": choices });
    if let (Some(page), Some(found)) = (page.as_object_mut(), found.as_object()) {
        page.extend(found.clone());
    }
    Ok(page)
}

const fn on(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// Sets `key` to `value` in your settings, or takes it out with `None`, once
/// rein would start with what the file then says.
pub(super) fn set(key: &str, value: Option<&str>) -> Result<(), String> {
    if settings::env(key).is_none() {
        return Err(format!("rein has no setting {key}"));
    }
    let path = crate::config_dir()?.join(settings::FILE);
    let Some(value) = value.map(str::trim) else { return settings::unset(&path, key) };
    if value.is_empty() || value.contains('\n') {
        return Err(format!("{key} takes a value on one line"));
    }
    let mut file = Settings::load(&path)?.file_only();
    file.put(key, value);
    // As rein reads it as it starts, with no variable or flag to hide it:
    // one started without them reads the file.
    AskArgs::parse(Vec::new(), &file)?;
    match key {
        "compact_at" => crate::conversation::compact_limit(None, Some(value)).map(|_| ())?,
        "look" if !LOOKS.contains(&value) => return Err(format!("look should be light, dark or follow, not {value}")),
        _ => {}
    }
    settings::set(&path, key, value)
}

/// What `agent` found that the page lists, taken while it's free: a turn
/// holds it, and the page may be open during one.
pub(super) fn found(agent: &Agent<Llm>) -> Value {
    let rules: Vec<Value> = agent
        .permissions
        .rules()
        .iter()
        .map(|rule| {
            let kind = match rule.kind() {
                Kind::Allow => "allow",
                Kind::Ask => "ask",
                Kind::Deny => "deny",
            };
            let shown = rule.to_string();
            let what = shown.split_once(' ').map_or(shown.as_str(), |(_, what)| what).to_owned();
            json!({ "rule": shown, "kind": kind, "what": what, "yours": rule.whose() == Whose::Yours })
        })
        .collect();
    let skills = agent.skills();
    let skill = |skill: &rein_agent::skills::Skill, on: bool| {
        let whose = match skill.whose {
            rein_agent::skills::Whose::Yours => "yours",
            rein_agent::skills::Whose::Project => "project",
            rein_agent::skills::Whose::Plugin => "plugin",
        };
        json!({ "name": skill.name, "whose": whose, "on": on })
    };
    let mut listed: Vec<Value> = skills.found.iter().map(|one| skill(one, true)).chain(skills.off.iter().map(|one| skill(one, false))).collect();
    listed.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let hooks: Vec<Value> = agent
        .hooks()
        .all()
        .iter()
        .map(|hook| {
            use rein_agent::hooks::{From, State};
            let state = match &hook.state {
                State::On => "on".to_owned(),
                State::Off => "off".to_owned(),
                State::ReinDoesIt(why) => format!("off: {why}"),
                State::Waiting => "waiting".to_owned(),
                State::NotRun(why) => format!("off: {why}"),
            };
            json!({ "command": hook.command.line, "event": hook.event.as_str(), "project": matches!(hook.from, From::Project(_)), "from": hook.from.describe(), "state": state })
        })
        .collect();
    let notes: Vec<Value> = agent.tools.memory().map_or_else(Vec::new, |memory| {
        memory
            .all()
            .iter()
            .map(|note| json!({ "name": note.name, "description": note.description, "reins": note.whose == rein_agent::memory::Whose::Rein, "modified": note.modified }))
            .collect()
    });
    let servers: Vec<Value> = agent.tools.mcp().map_or_else(Vec::new, |servers| {
        servers
            .statuses()
            .iter()
            .map(|(server, status)| {
                let (state, tools) = match status {
                    rein_mcp::Status::Starting => ("starting".to_owned(), 0),
                    rein_mcp::Status::Ready(tools) => ("ready".to_owned(), tools.len()),
                    rein_mcp::Status::Stopped => ("stopped".to_owned(), 0),
                    rein_mcp::Status::Failed(why) => (why.to_string(), 0),
                };
                json!({ "name": server.name, "state": state, "tools": tools })
            })
            .collect()
    });
    json!({ "rules": rules, "skills": listed, "hooks": hooks, "notes": notes, "servers": servers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_isnt_a_setting_or_is_on_two_lines_isnt_written() {
        assert_eq!(set("colour", Some("blue")).unwrap_err(), "rein has no setting colour");
        assert_eq!(set("mode", Some("ask\nmodel = x")).unwrap_err(), "mode takes a value on one line");
        assert_eq!(set("mode", Some("  ")).unwrap_err(), "mode takes a value on one line");
    }
}
