//! What effort the model gets, and how rein says so. You choose a level
//! (`/effort`, `--effort`, the settings), each model takes some of them,
//! and it gets the closest it has without going over
//! ([`rein_llm::Efforts::sent`]). Your choice stays as it was, so a model
//! that has it gets it again.

use rein_agent::agent::{Agent, Llm};
use rein_llm::{Effort, Efforts};

/// The level the model thinks at now: what it's sent, or, when that's
/// nothing or it turned that down, its own default. `None` where that isn't
/// known, or for a model that takes none.
pub(crate) fn now(agent: &Agent<Llm>) -> Option<Effort> {
    match agent.effort() {
        Some(level) if !turned_down(agent).contains(&level) => Some(level),
        _ => agent.efforts.default(),
    }
}

fn turned_down(agent: &Agent<Llm>) -> Vec<Effort> {
    agent.model.client.turned_down(&agent.model_name)
}

/// Why the model doesn't get the level you chose, when it doesn't.
pub(crate) fn unlike_chosen(agent: &Agent<Llm>) -> Option<String> {
    let chosen = agent.reasoning_effort?;
    let model = &agent.model_name;
    if agent.efforts == Efforts::NotTaken {
        return Some(format!("{model} takes no effort setting, so {chosen} waits for a model that does"));
    }
    match agent.effort() {
        Some(sent) if turned_down(agent).contains(&sent) => Some(format!("{model} turned {sent} down, so it thinks as it does by default")),
        Some(sent) if sent != chosen => Some(format!("{model} has no {chosen}, so it gets {sent}")),
        _ => None,
    }
}

/// The effort in use, in words: `xhigh`, `low, the model's default`, and
/// why it isn't what you chose when it isn't.
pub(crate) fn words(agent: &Agent<Llm>) -> String {
    if agent.efforts == Efforts::NotTaken {
        return format!("none: {} takes no effort setting", agent.model_name);
    }
    let level = match (now(agent), agent.effort().is_some_and(|sent| now(agent) == Some(sent))) {
        (Some(level), true) => level.to_string(),
        (Some(level), false) => format!("{level}, the model's default"),
        (None, _) => "the model's default".to_owned(),
    };
    match unlike_chosen(agent) {
        Some(why) => format!("{level} ({why})"),
        None => level,
    }
}

/// What `/effort` offers for the model in use.
pub(crate) struct Choices {
    /// The default (`None`), then the levels the model takes, least first.
    pub levels: Vec<Option<Effort>>,
    /// Each one's name and what it's for.
    pub items: Vec<(String, String)>,
    /// The one in use.
    pub current: Option<usize>,
}

/// The `/effort` menu for the model in use; `None` for a model that takes
/// no effort, where there's nothing to choose.
pub(crate) fn choices(agent: &Agent<Llm>) -> Option<Choices> {
    if agent.efforts == Efforts::NotTaken {
        return None;
    }
    let out = turned_down(agent);
    let default = match agent.efforts.default() {
        Some(level) => format!("the model's own: {level}"),
        None => "the model's own".to_owned(),
    };
    let mut levels = vec![None];
    let mut items = vec![("default".to_owned(), default)];
    for level in agent.efforts.offered().into_iter().filter(|level| !out.contains(&level.effort)) {
        levels.push(Some(level.effort));
        items.push((level.effort.to_string(), level.about.unwrap_or_else(|| about(level.effort).to_owned())));
    }
    // What you chose, or where the model hasn't got it, what it gets.
    let current = levels.iter().position(|&level| level == agent.reasoning_effort).or_else(|| levels.iter().position(|&level| level.is_some() && level == now(agent)));
    Some(Choices { levels, items, current })
}

/// What a level is for, where its provider doesn't say.
fn about(effort: Effort) -> &'static str {
    match effort {
        Effort::Off => "no thinking",
        Effort::Minimal => "as little thinking as there can be",
        Effort::Low => "quick: for simple work",
        Effort::Medium => "a balance",
        Effort::High => "thorough",
        Effort::XHigh => "more thorough",
        Effort::Max => "the most thinking there is",
    }
}

/// The level sent as a turn starts, to tell afterwards whether the model
/// turned it down.
pub(crate) struct Watch {
    sent: Option<Effort>,
    before: Vec<Effort>,
}

pub(crate) fn watch(agent: &Agent<Llm>) -> Watch {
    Watch { sent: agent.effort(), before: turned_down(agent) }
}

impl Watch {
    /// What to say if the model turned down the level it was sent during
    /// the turn: said once, as it isn't sent again.
    pub(crate) fn said(self, agent: &Agent<Llm>) -> Option<String> {
        let level = self.sent?;
        (!self.before.contains(&level) && turned_down(agent).contains(&level))
            .then(|| format!("({} doesn't take effort {level}, so it thought as it does by default; /effort shows what it takes)", agent.model_name))
    }
}
