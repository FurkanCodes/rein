//! Signing in before there's a conversation: a client that can show what a
//! command asks hears that you're signed in to nothing that runs the model
//! as a refusal with a code of its own, with the ways there are to sign in,
//! and asks for one. Each runs as `/login` does in the terminal, its menus
//! and key boxes asked of the client.

use std::rc::Rc;
use std::time::Duration;

use rein_agent::permissions::Mode;
use rein_llm::provider::{self, Provider};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Link;
use super::ui::Asker;
use crate::args::AskArgs;
use crate::connect::{self, Keys};
use crate::input::Menu;
use crate::settings::{self, Settings};
use crate::terminal::Terminal;
use crate::ui::history::History;
use crate::{CREDENTIALS_FILE, CliResult, config_dir};

/// How long a server on this computer gets to list its models.
const LOCAL_WITHIN: Duration = Duration::from_secs(3);

/// A way to sign in, as a client asks for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Way {
    /// Your ChatGPT plan, in the browser.
    Chatgpt,
    /// Your Claude plan, through Claude Code.
    ClaudeCode,
    /// An API key, for a provider you choose.
    Key,
    /// A server on this computer, which needs no key.
    Local,
}

/// Whether a new conversation, started as `flags` and the settings say,
/// would find you signed in to nothing that runs its model.
pub(super) fn signed_out(flags: &[String]) -> CliResult<bool> {
    let dir = config_dir()?;
    let settings = Settings::load(&dir.join(settings::FILE))?;
    let args = AskArgs::parse(flags.to_vec(), &settings)?;
    let (providers, keys) = (connect::providers(&dir)?, connect::load_keys(&dir)?);
    let wanted = args.model.unwrap_or_else(|| connect::default_model(&providers, &keys));
    Ok(connect::unpaid(&wanted, &providers, &keys))
}

/// The ways there are to sign in, and what's needed to tell them apart:
/// the client words them.
pub(super) fn ways() -> CliResult<Value> {
    let dir = config_dir()?;
    let providers = connect::providers(&dir)?;
    let keys = connect::load_keys(&dir)?;
    // Those you can't use yet: a key you have is no way to sign in to more.
    let keyed: Vec<&str> = key_providers(&providers).filter(|provider| !connect::usable(provider, &keys)).map(|provider| provider.title.as_str()).collect();
    let local: Vec<&str> = providers.iter().filter(|provider| !provider.needs_key).map(|provider| provider.title.as_str()).collect();
    Ok(json!({ "ways": [
        { "way": "chatgpt", "signedIn": dir.join(CREDENTIALS_FILE).exists() },
        { "way": "claude-code" },
        { "way": "key", "providers": keyed },
        { "way": "local", "providers": local },
    ] }))
}

/// The providers an API key is for: not ChatGPT's plan or Claude Code's,
/// which are ways of their own.
fn key_providers(providers: &[Provider]) -> impl Iterator<Item = &Provider> {
    providers.iter().filter(|provider| provider.needs_key && provider.name != "claude-code")
}

/// Signs in the `way` asked for. `false`: you stopped it, and nothing was
/// saved.
pub(super) async fn sign_in(way: Way, http: &rein_llm::reqwest::Client, link: &Link) -> CliResult<bool> {
    let dir = config_dir()?;
    let providers = connect::providers(&dir)?;
    let root = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
    // Nothing runs here: the walls are never asked for.
    let places = rein_agent::sandbox::Places::new(&root, None, None);
    let sandbox = std::sync::Arc::new(rein_agent::sandbox::Sandbox::missing(places, rein_agent::sandbox::Missing::NotHere));
    let mut terminal = Terminal::new(root, sandbox, Mode::default(), false, History::default(), false);
    // Before a conversation, under no conversation's name.
    terminal.input.stand_in(Rc::new(Asker { session: String::new(), link: link.clone() }));
    let name = match way {
        Way::Chatgpt => "chatgpt".to_owned(),
        Way::ClaudeCode => "claude-code".to_owned(),
        Way::Key => {
            let keys = connect::load_keys(&dir)?;
            let offered: Vec<&Provider> = key_providers(&providers).collect();
            let items = offered.iter().map(|provider| (provider.title.clone(), connect::pays(provider, &keys, false))).collect();
            let menu = Menu { title: "add a key for".to_owned(), items, current: None, at: None };
            let Some(picked) = terminal.input.choose(menu).await else { return Ok(false) };
            offered.get(picked).map(|provider| provider.name.clone()).ok_or("that isn't one of them")?
        }
        Way::Local => return use_local(http, &providers, &mut terminal).await,
    };
    Ok(crate::sign_in::here(&name, None, &mut terminal, http, &providers).await?.is_some())
}

/// Finds the servers on this computer and what they run, and starts new
/// conversations on the model you choose of theirs.
async fn use_local(http: &rein_llm::reqwest::Client, providers: &[Provider], terminal: &mut Terminal) -> CliResult<bool> {
    let keys = Keys::default();
    let mut found = Vec::new();
    for provider in providers.iter().filter(|provider| !provider.needs_key) {
        let Ok((client, _)) = connect::client_for(http, provider, &keys).await else { continue };
        if let Ok(Ok(models)) = tokio::time::timeout(LOCAL_WITHIN, client.list_models()).await {
            found.extend(models.iter().map(|model| (provider.title.clone(), provider::spec(provider, &model.id))));
        }
    }
    if found.is_empty() {
        let names: Vec<&str> = providers.iter().filter(|provider| !provider.needs_key).map(|provider| provider.title.as_str()).collect();
        return Err(format!("nothing is running here: start {} and look again", names.join(" or ")));
    }
    let items = found.iter().map(|(title, spec)| (spec.clone(), title.clone())).collect();
    let menu = Menu { title: "choose a model".to_owned(), items, current: None, at: None };
    let Some(picked) = terminal.input.choose(menu).await else { return Ok(false) };
    let (_, spec) = found.get(picked).ok_or("that isn't one of them")?;
    settings::set(&config_dir()?.join(settings::FILE), "model", spec)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::Way;

    #[test]
    fn a_way_is_one_of_four_and_any_other_is_an_error() {
        let way = |text: &str| serde_json::from_str::<Way>(text);
        assert_eq!(
            [way(r#""chatgpt""#).ok(), way(r#""claude-code""#).ok(), way(r#""key""#).ok(), way(r#""local""#).ok()],
            [Some(Way::Chatgpt), Some(Way::ClaudeCode), Some(Way::Key), Some(Way::Local)]
        );
        assert!(way(r#""github""#).is_err());
    }
}
