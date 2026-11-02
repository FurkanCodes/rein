//! Reaching the provider that serves a model: which one it is, and whose
//! key pays for it.
//!
//! A key comes from the provider's environment variable, or else from
//! `rein login <provider>`, which keeps it in `~/.config/rein/keys.json`,
//! readable only by you. The variable wins, as rein's flags and variables
//! win over its files. OpenAI can also be paid for with your ChatGPT plan
//! (`rein login`), which comes first when you've signed in.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use rein_llm::openai::signin::{self, Credentials};
use rein_llm::provider::{self, Provider};
use rein_llm::{Auth, Client, ModelInfo, store};

use crate::{CREDENTIALS_FILE, CliResult, USAGE_URL, config_dir, describe};

pub const KEYS_FILE: &str = "keys.json";

/// Keys saved with `rein login <provider>`, by provider.
pub type Keys = BTreeMap<String, String>;

pub fn load_keys(dir: &Path) -> CliResult<Keys> {
    Ok(store::read_json(&dir.join(KEYS_FILE)).map_err(describe)?.unwrap_or_default())
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The first of `provider`'s variables that holds a key, and the key.
fn env_key(provider: &Provider) -> Option<(&str, String)> {
    provider.key_env.iter().find_map(|var| env(var).map(|key| (var.as_str(), key)))
}

/// Whether rein has something to pay `provider` with.
pub fn usable(provider: &Provider, keys: &Keys) -> bool {
    env_key(provider).is_some()
        || keys.contains_key(&provider.name)
        || (provider.name == "anthropic" && env("ANTHROPIC_AUTH_TOKEN").is_some())
        || (provider.name == "openai" && config_dir().is_ok_and(|dir| dir.join(CREDENTIALS_FILE).exists()))
}

/// The model to use when none is named: OpenAI's if you can pay for it,
/// otherwise the first provider's you have a key for.
pub fn default_model(providers: &[Provider], keys: &Keys) -> String {
    providers
        .iter()
        .find_map(|p| p.default_model.as_ref().filter(|_| usable(p, keys)).map(|model| provider::spec(p, model)))
        .unwrap_or_else(|| crate::DEFAULT_MODEL.to_owned())
}

/// A client for `provider`, with the first key there is. Prints which
/// one, so it's always clear who pays.
pub async fn client_for(http: &rein_llm::reqwest::Client, provider: &Provider, keys: &Keys) -> CliResult<Client> {
    if provider.name == "openai"
        && let Some(client) = chatgpt(http).await?
    {
        return Ok(client);
    }
    // Another server that speaks the same API, like a proxy: the variables
    // the providers' own libraries read.
    let url = provider.url_env.as_deref().and_then(env);
    let base = url.clone().unwrap_or_else(|| provider.base_url.clone());
    let pays = |whose: &str| match &url {
        Some(url) => format!("{whose} with {url}"),
        None if provider.name == "openai" => format!("{whose} (billed to your API account)"),
        None => format!("{whose} (billed to your {} account)", provider.title),
    };
    let auth = |key: String| match provider.key_header {
        Some(header) => Auth::Header(header, key),
        None => Auth::Bearer(key),
    };
    if let Some((var, key)) = env_key(provider) {
        say!("Using {}", pays(var));
        return Ok(Client::new(http.clone(), provider.api, base, auth(key)));
    }
    // A gateway in front of Claude takes its own token, as a bearer token.
    if provider.name == "anthropic"
        && let Some(token) = env("ANTHROPIC_AUTH_TOKEN")
    {
        say!("Using {}", pays("ANTHROPIC_AUTH_TOKEN"));
        return Ok(Client::new(http.clone(), provider.api, base, Auth::Bearer(token)));
    }
    if let Some(key) = keys.get(&provider.name) {
        say!("Using {}", pays(&format!("your saved {} key", provider.title)));
        return Ok(Client::new(http.clone(), provider.api, base, auth(key.clone())));
    }
    Err(no_key(provider))
}

fn no_key(provider: &Provider) -> String {
    let get = provider.keys_url.as_deref().map(|url| format!(" (get one at {url})")).unwrap_or_default();
    let vars = provider.key_env.join(" or ");
    match provider.name.as_str() {
        "openai" => format!("not signed in. Run `rein login` to continue with ChatGPT, or set {vars}{get}."),
        name => format!("no key for {}: set {vars}, or run `rein login {name}`{get}", provider.title),
    }
}

/// A client on your ChatGPT plan, if you've signed in with `rein login`.
async fn chatgpt(http: &rein_llm::reqwest::Client) -> CliResult<Option<Client>> {
    let path = config_dir()?.join(CREDENTIALS_FILE);
    let Some(mut credentials) = store::read_json::<Credentials>(&path).map_err(describe)? else { return Ok(None) };
    if !credentials.plan_enabled() {
        return Err("your ChatGPT sign-in doesn't allow using your plan; run `rein login` and allow it".into());
    }
    if credentials.needs_refresh() {
        credentials = signin::refresh(http, &credentials)
            .await
            .map_err(|e| format!("couldn't renew your ChatGPT sign-in ({}); run `rein login` to sign in again", describe(e)))?;
        store::write_json_private(&path, &credentials).map_err(describe)?;
    }
    say!("Using ChatGPT plan (manage usage: {USAGE_URL})");
    Ok(Some(Client::openai(http.clone(), credentials.access_token)))
}

/// A model, and the client that reaches it.
pub struct Connected {
    pub provider: Provider,
    /// The model's ID at the provider.
    pub model: String,
    pub client: Client,
    /// The provider's model list, fetched while everything else gets ready:
    /// it says how big each model's context window is.
    pub models: tokio::task::JoinHandle<rein_llm::Result<Vec<ModelInfo>>>,
}

impl Connected {
    /// The model list, if it came within a few seconds.
    pub async fn models(self) -> (Client, Vec<ModelInfo>) {
        let models = match tokio::time::timeout(Duration::from_secs(5), self.models).await {
            Ok(Ok(Ok(models))) => models,
            _ => Vec::new(),
        };
        (self.client, models)
    }
}

/// Which provider serves `spec`, said as rein can't take it otherwise.
pub fn resolve<'a>(spec: &str, providers: &'a [Provider]) -> CliResult<(&'a Provider, String)> {
    provider::resolve(spec, providers).ok_or_else(|| {
        let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        format!("no provider for {spec}; name one, like anthropic/claude-opus-5-5 ({})", names.join(", "))
    })
}

/// Connects to whoever serves `spec`, and starts fetching its model list.
pub async fn connect(http: &rein_llm::reqwest::Client, providers: &[Provider], keys: &Keys, spec: &str, compress: bool) -> CliResult<Connected> {
    let (provider, model) = resolve(spec, providers)?;
    let mut client = client_for(http, provider, keys).await?;
    if !compress {
        client = client.without_compression();
    }
    let models = {
        let client = client.clone();
        tokio::spawn(async move { client.list_models().await })
    };
    Ok(Connected { provider: provider.clone(), model, client, models })
}

/// `rein login <provider>`: saves a key for it, after checking it works.
pub async fn login(http: &rein_llm::reqwest::Client, name: &str) -> CliResult {
    let providers = provider::builtin();
    let Some(provider) = providers.iter().find(|p| p.name == name) else {
        let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        return Err(format!("no provider called {name}; rein knows {}", names.join(", ")));
    };
    let get = provider.keys_url.as_deref().map(|url| format!(" (from {url})")).unwrap_or_default();
    let key = read_secret(&format!("Paste your {} API key{get}: ", provider.title))?;
    if key.is_empty() {
        return Err("no key given; nothing was saved".into());
    }
    let base = provider.url_env.as_deref().and_then(env).unwrap_or_else(|| provider.base_url.clone());
    let client = Client::new(
        http.clone(),
        provider.api,
        base,
        match provider.key_header {
            Some(header) => Auth::Header(header, key.clone()),
            None => Auth::Bearer(key.clone()),
        },
    );
    let models = client.list_models().await.map_err(|e| format!("that key didn't work ({}); nothing was saved", describe(e)))?;
    let dir = config_dir()?;
    let mut keys = load_keys(&dir)?;
    keys.insert(provider.name.clone(), key);
    store::write_json_private(&dir.join(KEYS_FILE), &keys).map_err(describe)?;
    let count = if models.len() == 1 { "1 model".to_owned() } else { format!("{} models", models.len()) };
    say!("Saved. Your {} key works: {count}. `rein models {name}` lists them.", provider.title);
    if let Some((var, _)) = env_key(provider) {
        say!("While {var} is set, rein uses that instead.");
    }
    Ok(())
}

/// `rein logout <provider>`: forgets its saved key, or your ChatGPT sign-in.
pub fn logout(name: &str) -> CliResult {
    let dir = config_dir()?;
    if name == "chatgpt" {
        return match std::fs::remove_file(dir.join(CREDENTIALS_FILE)) {
            Ok(()) => {
                say!("Signed out of ChatGPT.");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("you weren't signed in with ChatGPT".into()),
            Err(e) => Err(format!("couldn't sign out: {e}")),
        };
    }
    let mut keys = load_keys(&dir)?;
    if keys.remove(name).is_none() {
        return Err(format!("there's no saved key for {name}"));
    }
    store::write_json_private(&dir.join(KEYS_FILE), &keys).map_err(describe)?;
    say!("Forgot your {name} key.");
    Ok(())
}

/// `rein models [provider]`: what each provider you can pay offers.
pub async fn models(http: &rein_llm::reqwest::Client, name: Option<&str>) -> CliResult {
    let providers = provider::builtin();
    let keys = load_keys(&config_dir()?)?;
    let chosen: Vec<&Provider> = match name {
        Some(name) => vec![providers.iter().find(|p| p.name == name).ok_or_else(|| format!("no provider called {name}"))?],
        None => providers.iter().filter(|p| usable(p, &keys)).collect(),
    };
    if chosen.is_empty() {
        return Err(no_key(&providers[0]));
    }
    for provider in &chosen {
        let client = client_for(http, provider, &keys).await?;
        let models = client.list_models().await.map_err(describe)?;
        if chosen.len() > 1 {
            println!("{}:", provider.name);
        }
        let pad = if chosen.len() > 1 { "  " } else { "" };
        for model in models {
            let id = if chosen.len() > 1 || provider.name != "openai" { provider::shown(provider, &model.id) } else { model.id.clone() };
            if model.name == model.id {
                println!("{pad}{id}");
            } else {
                println!("{pad}{id:<24} {}", model.name);
            }
        }
    }
    Ok(())
}

/// A line typed without showing it, like a password; or piped in.
fn read_secret(prompt: &str) -> CliResult<String> {
    let tty = std::io::stdin().is_terminal();
    eprint!("{prompt}");
    // SAFETY: an all-zero termios is a valid value, and tcgetattr fills it.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let quiet = tty && unsafe { libc::tcgetattr(0, &mut saved) } == 0;
    if quiet {
        let mut hidden = saved;
        hidden.c_lflag &= !libc::ECHO;
        // SAFETY: a termios that tcgetattr gave, with echo off.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &hidden) };
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if quiet {
        // SAFETY: putting back what tcgetattr gave.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        eprintln!();
    }
    read.map_err(|e| format!("couldn't read the key: {e}"))?;
    Ok(line.trim().to_owned())
}
