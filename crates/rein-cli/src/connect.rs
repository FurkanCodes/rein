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
use std::sync::OnceLock;
use std::time::Duration;

use rein_llm::openai::signin::Credentials;
use rein_llm::provider::{self, Provider};
use rein_llm::{Auth, Client, ModelInfo, store};

use crate::{CREDENTIALS_FILE, CliResult, USAGE_URL, config_dir, describe};

pub const KEYS_FILE: &str = "keys.json";
pub const PROVIDERS_FILE: &str = "providers";

/// rein's providers, and yours from `~/.config/rein/providers`.
pub fn providers(dir: &Path) -> CliResult<Vec<Provider>> {
    let path = dir.join(PROVIDERS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => provider::with_file(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(provider::builtin()),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
}

/// Keys saved with `rein login <provider>`, by provider.
pub type Keys = BTreeMap<String, String>;

pub fn load_keys(dir: &Path) -> CliResult<Keys> {
    Ok(store::read_json(&dir.join(KEYS_FILE)).map_err(describe)?.unwrap_or_default())
}

/// A variable from rein's environment: keys and tokens from the copy rein
/// kept when it started (see `secrets`).
fn env(name: &str) -> Option<String> {
    rein_agent::sandbox::secrets::get(name).filter(|v| !v.trim().is_empty())
}

/// The first of `provider`'s variables that holds a key, and the key.
fn env_key(provider: &Provider) -> Option<(&str, String)> {
    provider.key_env.iter().find_map(|var| env(var).map(|key| (var.as_str(), key)))
}

/// What pays for a provider's requests, in the order rein looks.
enum Payment {
    /// Your ChatGPT plan: you've signed in with `rein login`.
    Plan,
    /// Official CLI subprocess, with a saved login or a setup-token token.
    ClaudeCode,
    /// A key in one of the provider's variables.
    Key { var: String, key: String },
    /// A gateway's token, sent as a bearer token.
    Token { var: String, token: String },
    /// A key saved with `rein login <provider>`.
    Saved(String),
    /// Nothing: a server on this computer, say.
    Free,
}

fn payment(provider: &Provider, keys: &Keys) -> Option<Payment> {
    if provider.name == "claude-code" {
        return (env("CLAUDE_CODE_OAUTH_TOKEN").is_some() || config_dir().is_ok_and(|dir| dir.join(crate::claude::CODE_FILE).exists())).then_some(Payment::ClaudeCode);
    }
    if provider.chatgpt && config_dir().is_ok_and(|dir| dir.join(CREDENTIALS_FILE).exists()) {
        return Some(Payment::Plan);
    }
    if let Some((var, key)) = env_key(provider) {
        return Some(Payment::Key { var: var.to_owned(), key });
    }
    if let Some(var) = &provider.token_env
        && let Some(token) = env(var)
    {
        return Some(Payment::Token { var: var.clone(), token });
    }
    if let Some(key) = keys.get(&provider.name) {
        return Some(Payment::Saved(key.clone()));
    }
    (!provider.needs_key).then_some(Payment::Free)
}

/// Whether rein has something to pay `provider` with, or it needs nothing
/// and you've set it up.
pub fn usable(provider: &Provider, keys: &Keys) -> bool {
    match payment(provider, keys) {
        Some(Payment::Free) => provider.yours,
        Some(_) => true,
        None => false,
    }
}

/// You're signed in to nothing that runs the model `spec`: no sign-in, no
/// key, and its provider needs one. A model rein can't place isn't this: connecting to
/// it says what's wrong.
pub fn unpaid(spec: &str, providers: &[Provider], keys: &Keys) -> bool {
    resolve(spec, providers).is_ok_and(|(provider, _)| payment(provider, keys).is_none())
}

/// The model to use when none is named: OpenAI's if you can pay for it,
/// otherwise the first provider's you have a key for.
pub fn default_model(providers: &[Provider], keys: &Keys) -> String {
    providers.iter().find_map(|p| p.default_model.as_ref().filter(|_| usable(p, keys)).map(|model| provider::spec(p, model))).unwrap_or_else(|| crate::DEFAULT_MODEL.to_owned())
}

/// Where `provider`'s requests go: its own server, or the one its
/// variable points to (a proxy, say), as the provider's own libraries read it.
fn base_url(provider: &Provider) -> (String, bool) {
    match provider.url_env.as_deref().and_then(env) {
        Some(url) => (url, true),
        None => (provider.base_url.clone(), false),
    }
}

/// How `key` goes with requests to `provider`.
fn auth(provider: &Provider, key: String) -> Auth {
    match provider.key_header {
        Some(header) => Auth::Header(header, key),
        None => Auth::Bearer(key),
    }
}

/// A client that sends a key to `base`, unless the key would cross the
/// network where anyone can read it: plain http to another computer.
fn keyed(http: &rein_llm::reqwest::Client, provider: &Provider, base: &str, auth: Auth) -> CliResult<Client> {
    if !rein_llm::http::may_carry_a_key(base) {
        return Err(format!(
            "rein won't send your {} key to {base}: that's plain http, which anyone on the network in between can read. Use https, or plain http only to this computer (localhost)",
            provider.title
        ));
    }
    Ok(Client::new(http.clone(), provider.api, base.to_owned(), auth).with_quirks(provider.quirks).with_meters(provider.meters))
}

/// A client for `provider`, with the first way to pay there is, and a line
/// saying which, so it's always clear who pays.
pub async fn client_for(http: &rein_llm::reqwest::Client, provider: &Provider, keys: &Keys) -> CliResult<(Client, String)> {
    let (base, elsewhere) = base_url(provider);
    let pays = |whose: &str| if elsewhere { format!("Using {whose} with {base}") } else { format!("Using {whose} (billed to your {} API account)", provider.title) };
    let client = |auth: Auth| Client::new(http.clone(), provider.api, base.clone(), auth).with_quirks(provider.quirks).with_meters(provider.meters);
    match payment(provider, keys) {
        Some(Payment::Plan) => Ok((chatgpt(http).await?, format!("Using ChatGPT plan (manage usage: {USAGE_URL})"))),
        Some(Payment::ClaudeCode) => {
            let saved: Option<crate::claude::CodeCredentials> = store::read_json(&config_dir()?.join(crate::claude::CODE_FILE)).map_err(describe)?;
            let token = env("CLAUDE_CODE_OAUTH_TOKEN").or_else(|| saved.and_then(|c| c.token));
            Ok((Client::claude_code(http.clone(), token), "Using Claude Code CLI subscription login (subprocess; rein handles tools)".into()))
        }
        Some(Payment::Key { var, key }) => Ok((keyed(http, provider, &base, auth(provider, key))?, pays(&var))),
        Some(Payment::Token { var, token }) => Ok((keyed(http, provider, &base, Auth::Bearer(token))?, pays(&var))),
        Some(Payment::Saved(key)) => Ok((keyed(http, provider, &base, auth(provider, key))?, pays(&format!("your saved {} key", provider.title)))),
        Some(Payment::Free) => Ok((client(Auth::None), format!("Using {} at {base}", provider.title))),
        None => Err(no_key(provider)),
    }
}

fn no_key(provider: &Provider) -> String {
    if provider.name == "claude-code" {
        return "Claude Code is not enabled: install `claude`, run `claude auth login` or `claude setup-token`, then `rein login claude-code`".into();
    }
    let get = provider.keys_url.as_deref().map(|url| format!(" (get one at {url})")).unwrap_or_default();
    let vars = provider.key_env.join(" or ");
    if provider.chatgpt {
        format!("not signed in. Run `rein login` to continue with ChatGPT, or set {vars}{get}.")
    } else {
        format!("no key for {}: set {vars}, or run `rein login {}`{get}", provider.title, provider.name)
    }
}

/// A client on your ChatGPT plan, which you've signed in to with `rein login`.
async fn chatgpt(http: &rein_llm::reqwest::Client) -> CliResult<Client> {
    let path = config_dir()?.join(CREDENTIALS_FILE);
    let Some(credentials) = store::read_json::<Credentials>(&path).map_err(describe)? else {
        return Err("you're not signed in with ChatGPT any more; run `rein login`".into());
    };
    if !credentials.plan_enabled() {
        return Err("your ChatGPT sign-in doesn't allow using your plan; run `rein login` and allow it".into());
    }
    Client::chatgpt(http.clone(), path).await.map_err(describe)
}

/// A model, and the client that reaches it.
pub struct Connected {
    pub provider: Provider,
    /// Who pays, to say.
    pub pays: String,
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
    let (mut client, pays) = client_for(http, provider, keys).await?;
    if !compress {
        client = client.without_compression();
    }
    let models = {
        let client = client.clone();
        tokio::spawn(async move { client.list_models().await })
    };
    Ok(Connected { provider: provider.clone(), pays, model, client, models })
}

/// The provider called `name`, to sign in to, or what to say when there's
/// none. That doesn't say `name` back: a key typed in its place would show.
pub fn find<'a>(providers: &'a [Provider], name: &str) -> CliResult<&'a Provider> {
    providers.iter().find(|p| p.name == name).ok_or_else(|| {
        let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        format!("there's no provider by that name; rein knows chatgpt, {}", names.join(", "))
    })
}

/// How a key for `provider` is had: from its browser sign-in, or pasted.
/// `None` when it needs none.
pub fn sign_in(provider: &Provider, paste: bool) -> CliResult<Option<&str>> {
    if !provider.needs_key {
        return Err(format!("{} needs no key: use a model of it, like {}/<model>", provider.title, provider.name));
    }
    Ok(provider.sign_in_url.as_deref().filter(|_| !paste))
}

/// What to ask for when a key is pasted.
pub fn paste_prompt(provider: &Provider) -> String {
    if provider.name == "claude-code" {
        return "Run `claude setup-token` and paste its token, or press Enter to use an existing `claude auth login` session".into();
    }
    let get = provider.keys_url.as_deref().map(|url| format!(" (from {url})")).unwrap_or_default();
    format!("Paste your {} API key{get}", provider.title)
}

/// `rein login <provider>`: a key for it, made in the browser where the
/// provider has an official sign-in (unless `paste`), or pasted; checked,
/// then saved.
pub async fn login(http: &rein_llm::reqwest::Client, name: &str, from: KeyFrom) -> CliResult {
    let providers = providers(&config_dir()?)?;
    let provider = find(&providers, name)?;
    let tell = |line: &str| say!("{line}");
    let paste = match from {
        // Checked and saved as one pasted is: never sent anywhere else.
        KeyFrom::Given(key) => return save_key(http, provider, key, false, &tell).await,
        KeyFrom::Asked => false,
        KeyFrom::Pasted => true,
    };
    let made = sign_in(provider, paste)?;
    let key = match made {
        Some(auth_url) => key_from_browser(http, provider, auth_url, &tell).await?,
        None => read_secret(&format!("{}: ", paste_prompt(provider)))?,
    };
    save_key(http, provider, key, made.is_some(), &tell).await
}

/// Where `rein login` gets the key from. No `Debug`: it may hold one.
pub enum KeyFrom {
    /// A sign-in in the browser, where the provider has one, or else
    /// asked for.
    Asked,
    /// Asked for, to paste (`--key`).
    Pasted,
    /// Typed after the provider's name.
    Given(String),
}

/// Checks `key` with `provider` and saves it, saying how that went.
/// `made`: the provider made it for rein, in the browser.
pub async fn save_key(http: &rein_llm::reqwest::Client, provider: &Provider, key: String, made: bool, tell: &dyn Fn(&str)) -> CliResult {
    if provider.name == "claude-code" {
        return crate::claude::save_code(key, tell);
    }
    if key.trim().is_empty() {
        return Err("no key given; nothing was saved".into());
    }
    let key = key.trim().to_owned();
    let client = keyed(http, provider, &base_url(provider).0, auth(provider, key.clone()))?;
    let didnt = |e| format!("that key didn't work ({}); nothing was saved", describe(e));
    client.check_key(&provider.key_check).await.map_err(didnt)?;
    let models = client.list_models().await.unwrap_or_default();
    let dir = config_dir()?;
    let mut keys = load_keys(&dir)?;
    keys.insert(provider.name.clone(), key);
    store::write_json_private(&dir.join(KEYS_FILE), &keys).map_err(describe)?;
    let count = if models.len() == 1 { "1 model".to_owned() } else { format!("{} models", models.len()) };
    if made {
        let manage = provider.keys_url.as_deref().map(|url| format!("; see or delete it at {url}")).unwrap_or_default();
        tell(&format!("Signed in. {} made rein a key of its own, billed to your account{manage}.", provider.title));
        tell(&format!("It works: {count}. `rein models {}` lists them.", provider.name));
    } else {
        tell(&format!("Saved. Your {} key works: {count}. `rein models {}` lists them.", provider.title, provider.name));
    }
    match payment(provider, &keys) {
        Some(Payment::Plan) => tell("You're signed in with ChatGPT, which comes first; `rein logout chatgpt` to use this key instead."),
        Some(Payment::Key { var, .. } | Payment::Token { var, .. }) => tell(&format!("While ${var} is set, rein uses that instead.")),
        _ => {}
    }
    Ok(())
}

/// A key `provider` makes for rein when you sign in in the browser.
pub async fn key_from_browser(http: &rein_llm::reqwest::Client, provider: &Provider, auth_url: &str, tell: &dyn Fn(&str)) -> CliResult<String> {
    let pending = rein_llm::openrouter::begin(auth_url, "rein").await.map_err(describe)?;
    tell(&format!("Sign in to {} in your browser, and rein gets a key of its own...", provider.title));
    if !crate::open_browser(&pending.url).await {
        tell(&format!("Couldn't open a browser. Open this link to continue:\n\n{}\n", pending.url));
    }
    let (base, _) = base_url(provider);
    tokio::time::timeout(Duration::from_secs(600), pending.finish(http, &base))
        .await
        .map_err(|_| format!("sign-in timed out after 10 minutes; run `rein login {}` again", provider.name))?
        .map_err(describe)
}

/// `rein logout <provider>`: forgets its saved key, or your ChatGPT sign-in.
pub fn logout(name: &str, tell: &dyn Fn(&str)) -> CliResult {
    let dir = config_dir()?;
    if name == "claude-code" {
        std::fs::remove_file(dir.join(crate::claude::CODE_FILE)).map_err(|e| format!("couldn't forget Claude Code configuration: {e}"))?;
        tell("Forgot rein's Claude Code configuration. This does not sign the Claude CLI out or revoke tokens; an environment token still takes precedence.");
        return Ok(());
    }
    if name == "chatgpt" {
        return match std::fs::remove_file(dir.join(CREDENTIALS_FILE)) {
            Ok(()) => {
                tell("Signed out of ChatGPT.");
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
    tell(&format!("Forgot your {name} key."));
    let providers = providers(&dir)?;
    if let Some(provider) = providers.iter().find(|p| p.name == name && p.sign_in_url.is_some()) {
        let at = provider.keys_url.as_deref().map(|url| format!(": delete it at {url}")).unwrap_or_default();
        tell(&format!("If {} made it for rein, it still works there until you delete it{at}.", provider.title));
    }
    Ok(())
}

/// `rein models [provider]`: what each provider you can pay offers.
pub async fn models(http: &rein_llm::reqwest::Client, name: Option<&str>) -> CliResult {
    let dir = config_dir()?;
    let (providers, keys) = (providers(&dir)?, load_keys(&dir)?);
    let chosen: Vec<&Provider> = match name {
        Some(name) => vec![providers.iter().find(|p| p.name == name).ok_or_else(|| format!("no provider called {name}"))?],
        None => providers.iter().filter(|p| usable(p, &keys)).collect(),
    };
    if chosen.is_empty() {
        return Err(match providers.iter().find(|p| p.chatgpt) {
            Some(openai) => no_key(openai),
            None => "no provider has a key yet; `rein providers` lists them".into(),
        });
    }
    for provider in &chosen {
        let (client, pays) = client_for(http, provider, &keys).await?;
        say!("{pays}");
        let models = client.list_models().await.map_err(describe)?;
        if chosen.len() > 1 {
            println!("{}:", provider.name);
        }
        let pad = if chosen.len() > 1 { "  " } else { "" };
        for model in models {
            let id = provider::shown(provider, &model.id);
            if model.name == model.id {
                println!("{pad}{id}");
            } else {
                println!("{pad}{id:<24} {}", model.name);
            }
        }
    }
    Ok(())
}

/// What pays for `provider`, in a few words; `how` adds how to get a key
/// when there's none.
pub fn pays(provider: &Provider, keys: &Keys, how: bool) -> String {
    match payment(provider, keys) {
        Some(Payment::ClaudeCode) => "Claude Code CLI subscription login".to_owned(),
        Some(Payment::Plan) => "your ChatGPT plan".to_owned(),
        Some(Payment::Key { var, .. } | Payment::Token { var, .. }) => format!("${var}"),
        Some(Payment::Saved(_)) => "your saved key".to_owned(),
        Some(Payment::Free) => "nothing: no key needed".to_owned(),
        None if how && provider.name == "claude-code" => "not enabled: `rein login claude-code`".to_owned(),
        None if how => format!("no key yet: ${} or `rein login {}`", provider.key_env.join(" or $"), provider.name),
        None => "no key yet".to_owned(),
    }
}

/// Each provider: its name, its API, where its requests go, and what pays.
pub fn rows(providers: &[Provider], keys: &Keys) -> Vec<[String; 4]> {
    providers.iter().map(|provider| [provider.name.clone(), provider.api.name().to_owned(), base_url(provider).0, pays(provider, keys, true)]).collect()
}

/// `rows`, lined up in columns as wide as what's in them.
pub fn table(rows: &[[String; 4]]) -> Vec<String> {
    let width = |i: usize| rows.iter().map(|row| row[i].chars().count()).max().unwrap_or(0);
    let (a, b, c) = (width(0), width(1), width(2));
    rows.iter().map(|[name, api, url, pays]| format!("{name:<a$}  {api:<b$}  {url:<c$}  {pays}")).collect()
}

/// `rein providers`: who rein can reach, how, and what pays.
pub fn list() -> CliResult {
    let dir = config_dir()?;
    for line in table(&rows(&providers(&dir)?, &load_keys(&dir)?)) {
        println!("{line}");
    }
    Ok(())
}

/// The terminal as it was before a key was typed into it unseen, for
/// Ctrl-C to put back.
static SHOWN: OnceLock<libc::termios> = OnceLock::new();

/// Ctrl-C while a key is typed: the terminal shows what's typed again,
/// then rein stops, as Ctrl-C would have it.
extern "C" fn interrupted(_: libc::c_int) {
    // SAFETY: tcsetattr, write and _exit are async-signal-safe, and SHOWN
    // was set before this handler was.
    unsafe {
        if let Some(shown) = SHOWN.get() {
            libc::tcsetattr(0, libc::TCSANOW, shown);
        }
        libc::write(2, b"\n".as_ptr().cast(), 1);
        libc::_exit(130);
    }
}

/// A line typed without showing it, like a password; or piped in.
fn read_secret(prompt: &str) -> CliResult<String> {
    let tty = std::io::stdin().is_terminal();
    eprint!("{prompt}");
    // SAFETY: an all-zero termios is a valid value, and tcgetattr fills it.
    let mut shown: libc::termios = unsafe { std::mem::zeroed() };
    let quiet = tty && unsafe { libc::tcgetattr(0, &mut shown) } == 0;
    // Whatever handled Ctrl-C before, to put back after.
    let mut before = libc::SIG_DFL;
    if quiet {
        let _ = SHOWN.set(shown);
        let mut hidden = shown;
        hidden.c_lflag &= !libc::ECHO;
        // SAFETY: the handler only uses async-signal-safe calls; the
        // termios is one tcgetattr gave, with echo off.
        unsafe {
            before = libc::signal(libc::SIGINT, interrupted as extern "C" fn(libc::c_int) as libc::sighandler_t);
            libc::tcsetattr(0, libc::TCSANOW, &hidden);
        }
    }
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if quiet {
        // SAFETY: putting back what tcgetattr gave, and Ctrl-C as it was.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &shown);
            libc::signal(libc::SIGINT, before);
        }
        eprintln!();
    }
    read.map_err(|e| format!("couldn't read the key: {e}"))?;
    Ok(line.trim().to_owned())
}
