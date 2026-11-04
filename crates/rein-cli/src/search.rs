//! `/search`: which model searches the web for the model you're working
//! with (`web_search`). As rein decides, that's the model in use when its
//! provider can search, or else the first you can pay for that can; or one
//! you pick, or none. Kept in your settings as `search`.

use std::time::SystemTime;

use rein_agent::agent::{Agent, Llm};
use rein_agent::web::Searcher;
use rein_llm::provider::{self, Provider};
use rein_llm::{Api, ModelInfo};

use crate::conversation::{Setup, other_models};
use crate::input::Menu;
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;
use crate::{CliResult, config_dir, settings};

/// Who searches the web, as you chose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum SearchWith {
    /// The model in use, when its provider can search; else the first you
    /// can pay for that can.
    #[default]
    Auto,
    /// Nothing: the model isn't offered `web_search`.
    Off,
    /// This model, as `provider/model`.
    Model(String),
}

impl std::str::FromStr for SearchWith {
    type Err = String;

    fn from_str(value: &str) -> Result<SearchWith, String> {
        match value.trim() {
            "" => Err("search should be auto, off, or a model like openai/gpt-6.1-sol".into()),
            "auto" => Ok(SearchWith::Auto),
            "off" => Ok(SearchWith::Off),
            model => Ok(SearchWith::Model(model.to_owned())),
        }
    }
}

impl std::fmt::Display for SearchWith {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchWith::Auto => f.write_str("auto"),
            SearchWith::Off => f.write_str("off"),
            SearchWith::Model(model) => f.write_str(model),
        }
    }
}

/// The providers whose models can search, in the order rein tries them
/// when the model in use can't.
const SEARCHING: [&str; 3] = ["openai", "claude-code", "anthropic"];

/// Whether `provider`'s models can search the web for rein: with OpenAI's
/// or Anthropic's own search, or Claude Code's.
fn searches(provider: &Provider) -> bool {
    provider.name == "claude-code" || matches!(provider.api, Api::Responses | Api::Anthropic)
}

/// The day, as the searching model is told it.
fn today() -> String {
    crate::format::local_time(SystemTime::now()).date()
}

/// Who searches for `agent`, as `with` says: `None` when it's off, or
/// nothing you can pay for can.
pub(crate) async fn searcher(with: &SearchWith, agent: &Agent<Llm>, setup: &Setup) -> CliResult<Option<Searcher>> {
    match with {
        SearchWith::Off => Ok(None),
        SearchWith::Model(spec) => {
            let (provider, model) = crate::connect::resolve(spec, setup.providers())?;
            if !searches(provider) {
                return Err(format!("{}'s models can't search the web for rein: OpenAI's, Anthropic's and Claude Code's can", provider.title));
            }
            let client =
                if provider.name == setup.provider().name { agent.model.client.clone() } else { crate::connect::client_for(setup.http(), provider, setup.keys()).await?.0 };
            Ok(Some(Searcher::new(client, model, today())))
        }
        SearchWith::Auto if agent.model.client.searches() => Ok(Some(Searcher::new(agent.model.client.clone(), agent.model_name.clone(), today()))),
        SearchWith::Auto => {
            for name in SEARCHING {
                let Some(provider) = setup.providers().iter().find(|provider| provider.name == name && crate::connect::usable(provider, setup.keys())) else { continue };
                let (Some(model), Ok((client, _))) = (provider.default_model.clone(), crate::connect::client_for(setup.http(), provider, setup.keys()).await) else { continue };
                return Ok(Some(Searcher::new(client, model, today())));
            }
            Ok(None)
        }
    }
}

/// Sets who searches for `agent` as `setup` says, saying why not when it
/// can't: at the start, and as the model in use changes.
pub(crate) async fn follow(agent: &mut Agent<Llm>, setup: &Setup) {
    match searcher(setup.search(), agent, setup).await {
        Ok(found) => agent.set_searcher(found),
        Err(why) => {
            say!("{}web search is off: {why}; /search picks another", " ".repeat(GUTTER));
            agent.set_searcher(None);
        }
    }
}

/// `/search`: who searches the web, picked from those that can, or as
/// typed; kept in your settings.
pub(crate) async fn command(with: Option<SearchWith>, agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    let with = match with {
        Some(with) => with,
        None => match pick(agent, terminal, setup).await {
            Some(with) => with,
            None => return Ok(()),
        },
    };
    let found = searcher(&with, agent, setup).await?;
    settings::set(&config_dir()?.join(settings::FILE), "search", &with.to_string())?;
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    match (&found, &with) {
        (Some(searcher), _) => say!("{pad}{} {} searches the web for the model, from your next message", s.green("✓"), searcher.model()),
        (None, SearchWith::Off) => say!("{pad}{} the model can't search the web now: /search turns it on", s.green("✓")),
        (None, _) => say!("{pad}{}", s.dim("Nothing you can pay for can search the web: OpenAI's, Anthropic's and Claude Code's models can, once /login has one")),
    }
    agent.set_searcher(found);
    setup.set_search(with);
    Ok(())
}

/// Who searches, picked from a list: as rein decides, each model that
/// can, and nobody.
async fn pick(agent: &Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> Option<SearchWith> {
    let now = agent.searcher().map_or_else(|| "none can now".to_owned(), |searcher| format!("now: {}", searcher.model()));
    let mut choices = vec![SearchWith::Auto];
    let mut items = vec![("as rein decides".to_owned(), format!("the model in use, if it can search; {now}"))];
    let mut add = |provider: &Provider, models: &[ModelInfo]| {
        if searches(provider) {
            for model in models {
                choices.push(SearchWith::Model(provider::spec(provider, &model.id)));
                items.push((provider::shown(provider, &model.id), provider.title.clone()));
            }
        }
    };
    let models = if setup.models().is_empty() { vec![ModelInfo::named(&agent.model_name)] } else { setup.models().to_vec() };
    add(&setup.provider().clone(), &models);
    for (provider, models) in other_models(setup, terminal).await {
        add(&provider, &models);
    }
    choices.push(SearchWith::Off);
    items.push(("off".to_owned(), "the model can't search the web".to_owned()));
    let current = choices.iter().position(|choice| choice == setup.search());
    let picked = terminal.input.choose(Menu { title: "who searches the web".into(), items, current, at: None }).await?;
    choices.into_iter().nth(picked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_reads_as_auto_off_or_a_model() {
        assert_eq!("auto".parse::<SearchWith>(), Ok(SearchWith::Auto));
        assert_eq!(" off ".parse::<SearchWith>(), Ok(SearchWith::Off));
        assert_eq!("openai/gpt-6.1-luna".parse::<SearchWith>(), Ok(SearchWith::Model("openai/gpt-6.1-luna".into())));
        assert!("".parse::<SearchWith>().is_err());
        for with in [SearchWith::Auto, SearchWith::Off, SearchWith::Model("claude-code/haiku".into())] {
            assert_eq!(with.to_string().parse::<SearchWith>(), Ok(with.clone()), "written as it's read");
        }
    }

    #[test]
    fn only_providers_with_a_search_of_their_own_search() {
        let all = rein_llm::provider::builtin();
        let can: Vec<&str> = all.iter().filter(|provider| searches(provider)).map(|provider| provider.name.as_str()).collect();
        for name in SEARCHING {
            assert!(can.contains(&name), "{name}: {can:?}");
        }
        assert!(!can.contains(&"gemini"), "Google's terms want its suggestions shown, which a terminal can't");
        assert!(!can.contains(&"openrouter"), "Chat Completions has no search of its own");
    }
}
