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
    let about = |mode: Mode| {
        let description = crate::modes::description(mode, sandboxing);
        description.split_once(": ").map_or(description, |(_, about)| about).to_owned()
    };
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

/// All of them, as they stand.
pub(super) fn all(agent: &Agent<Llm>, setup: &Setup) -> Value {
    let mut options = vec![mode(agent.permissions.mode, agent.permissions.sandboxing()), model(agent, setup)];
    options.extend(effort(agent));
    options.push(sandbox(agent));
    Value::Array(options)
}

/// The command that sets `option` to `value`.
pub(super) fn command(option: &str, value: &str) -> Result<Slash, Refused> {
    if !matches!(option, "mode" | "model" | "effort" | "sandbox") {
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
    }

    #[test]
    fn what_isnt_an_option_runs_nothing() {
        assert_eq!(command("logout", "openai").map(|_| ()).unwrap_err().code, INVALID_PARAMS);
        assert!(command("mode", "nonsense").is_err());
    }
}
