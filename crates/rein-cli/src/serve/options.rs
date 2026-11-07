//! What a client can set, as ACP's session config options: model, effort,
//! mode and sandbox, so that any client gets them with no method of rein's.
//! A choice goes through the command it would be, typed: there's one way to
//! change each, whoever asks, and the same care is taken.

use rein_agent::agent::{Agent, Llm};
use rein_agent::permissions::{Mode, Sandboxing};
use serde_json::{Value, json};

use super::wire::Refused;
use crate::commands::{self, Slash};
use crate::conversation::Setup;

/// Marks the level an effort comes to: `default` is a choice, not a level.
const LEVEL: &str = "io.github.furkancodes/level";

/// A choice in an option.
fn choice(value: &str, name: &str, description: &str) -> Value {
    json!({ "value": value, "name": name, "description": description })
}

fn select(id: &str, name: &str, category: &str, current: &str, options: &[Value]) -> Value {
    json!({ "id": id, "name": name, "category": category, "type": "select", "currentValue": current, "options": options })
}

fn mode(mode: Mode, sandboxing: Sandboxing) -> Value {
    let about = |mode: Mode| crate::modes::brief(mode, sandboxing).to_owned();
    let options: Vec<Value> = Mode::ALL.iter().map(|&one| choice(one.as_str(), &crate::modes::shown_name(one, sandboxing), &about(one))).collect();
    select("mode", "Mode", "mode", mode.as_str(), &options)
}

/// The models known so far, by provider: the others are looked up as you
/// ask for the list, which takes a moment and asks each of them.
fn model(agent: &Agent<Llm>, setup: &Setup) -> Value {
    let current = setup.current(agent);
    let groups: Vec<Value> = setup
        .models_known(agent)
        .iter()
        .map(|(provider, models)| {
            let options: Vec<Value> = models
                .iter()
                .map(|model| {
                    let window = model.context_window.map(|tokens| format!("{tokens} tokens")).unwrap_or_default();
                    choice(&rein_llm::provider::spec(provider, &model.id), &rein_llm::provider::shown(provider, &model.id), &window)
                })
                .collect();
            json!({ "group": provider.name, "name": provider.title, "options": options })
        })
        .collect();
    select("model", "Model", "model", &current, &groups)
}

fn effort(agent: &Agent<Llm>) -> Option<Value> {
    let choices = crate::effort::choices(agent)?;
    let options: Vec<Value> = choices.items.iter().map(|(name, about)| choice(name, name, about)).collect();
    let current = choices.current.and_then(|at| choices.items.get(at)).map_or("default", |(name, _)| name.as_str());
    let mut option = select("effort", "Effort", "thought_level", current, &options);
    option["_meta"] = json!({ LEVEL: crate::effort::now(agent).map(|level| level.to_string()) });
    Some(option)
}

fn sandbox(agent: &Agent<Llm>) -> Value {
    let (sandboxing, walls) = (agent.permissions.sandboxing(), agent.permissions.sandbox());
    let options = [choice("on", "on", &walls.describe()), choice("off", "off", "commands run as you'd run them, and the mode decides what asks")];
    select("sandbox", "Sandbox", "_sandbox", if sandboxing == Sandboxing::Off { "off" } else { "on" }, &options)
}

/// Whether new conversations start in the mode and with the sandbox this
/// one has now, as your settings file says: `on` keeps them there.
fn keep(agent: &Agent<Llm>) -> Value {
    let settings = crate::config_dir().and_then(|dir| crate::settings::Settings::load(&dir.join(crate::settings::FILE))).unwrap_or_default();
    let kept_mode = settings.in_file("mode").and_then(|name| name.parse::<Mode>().ok()).unwrap_or_default();
    // Off unless the file says on, as when rein starts.
    let kept_sandbox = settings.in_file("sandbox") == Some("on");
    let kept = kept_mode == agent.permissions.mode && kept_sandbox == (agent.permissions.sandboxing() != Sandboxing::Off);
    let options = [choice("on", "on", "new conversations start this way"), choice("off", "off", "new conversations start as rein does")];
    select("keep", "Keep for new conversations", "_keep", if kept { "on" } else { "off" }, &options)
}

/// Keeps the mode and sandbox `agent` has for new conversations, `on`, or
/// takes them out of your settings, so rein's own hold.
pub(super) fn set_keep(agent: &Agent<Llm>, on: bool) -> Result<(), String> {
    let path = crate::config_dir()?.join(crate::settings::FILE);
    if on {
        crate::settings::set(&path, "mode", agent.permissions.mode.as_str())?;
        crate::settings::set(&path, "sandbox", if agent.permissions.sandboxing() == Sandboxing::Off { "off" } else { "on" })
    } else {
        crate::settings::unset(&path, "mode")?;
        crate::settings::unset(&path, "sandbox")
    }
}

/// What `rein` says the command a check runs is, beside its switch.
const COMMAND: &str = "io.github.furkancodes/command";

/// The check after edits, as a switch, where the project has one.
fn check(agent: &Agent<Llm>) -> Option<Value> {
    let (name, on) = crate::check::switch(agent)?;
    let options = [choice("on", "on", "runs after a step changes files"), choice("off", "off", "doesn't run")];
    let mut option = select("check", "Check after edits", "_check", if on { "on" } else { "off" }, &options);
    option["_meta"] = json!({ COMMAND: name });
    Some(option)
}

/// All of them, as they stand.
pub(super) fn all(agent: &Agent<Llm>, setup: &Setup) -> Value {
    let mut options = vec![mode(agent.permissions.mode, agent.permissions.sandboxing()), model(agent, setup)];
    options.extend(effort(agent));
    options.push(sandbox(agent));
    options.push(keep(agent));
    options.extend(check(agent));
    Value::Array(options)
}

/// The command that sets `option` to `value`.
pub(super) fn command(option: &str, value: &str) -> Result<Slash, Refused> {
    if !matches!(option, "mode" | "model" | "effort" | "sandbox" | "check") {
        return Err(Refused::params(format!("rein has no option {option}")));
    }
    match commands::parse(&format!("/{option} {value}")) {
        Some(Ok(command)) => Ok(command),
        Some(Err(refused)) => Err(Refused::params(refused.to_string())),
        None => Err(Refused::params(format!("{value} isn't something {option} can be"))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::INVALID_PARAMS;
    use super::*;

    #[test]
    fn a_choice_is_the_command_it_would_be() {
        assert_eq!(command("mode", "edits").ok(), Some(Slash::Mode(Some(Mode::Edits))));
        assert_eq!(command("model", "groq/llama").ok(), Some(Slash::Model(Some("groq/llama".to_owned()))));
        assert_eq!(command("sandbox", "on").ok(), Some(Slash::Sandbox(Some(true))));
        assert!(command("check", "off").is_ok(), "the switch is /check off");
    }

    #[test]
    fn what_isnt_an_option_runs_nothing() {
        assert_eq!(command("logout", "openai").map(|_| ()).unwrap_err().code, INVALID_PARAMS);
        assert!(command("mode", "nonsense").is_err());
    }
}
