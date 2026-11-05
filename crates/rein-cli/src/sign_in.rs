//! Signing in without leaving rein: the menu `/login` shows, a sign-in in
//! the box, and the first run, where you're signed in to nothing yet and
//! rein opens on that menu instead of stopping.

use crate::connect::{self, Keys};
use crate::conversation::waiting;
use crate::input::{Menu, Read};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;
use crate::{CREDENTIALS_FILE, CliResult, config_dir, describe, login};
use rein_llm::provider::{self, Provider};
use std::time::Duration;

/// The providers there are to sign in to, by name, and the menu that offers
/// them, `current` marked.
pub(crate) fn menu(providers: &[Provider], keys: &Keys, current: Option<&str>) -> CliResult<(Vec<String>, Menu)> {
    let signed_in = config_dir()?.join(CREDENTIALS_FILE).exists();
    let mut names = vec!["chatgpt".to_owned()];
    let mut items = vec![("chatgpt".to_owned(), if signed_in { "signed in: your ChatGPT plan, for OpenAI's models" } else { "your ChatGPT plan, for OpenAI's models" }.to_owned())];
    for provider in providers.iter().filter(|p| p.needs_key) {
        names.push(provider.name.clone());
        let how = if provider.sign_in_url.is_some() { ", signs in in the browser" } else { "" };
        let description =
            if provider.name == "claude-code" { "Claude Code subscription: CLI login or setup-token".to_owned() } else { format!("{}{how}", connect::pays(provider, keys, false)) };
        items.push((provider.name.clone(), description));
    }
    let current = current.and_then(|current| names.iter().position(|name| name == current));
    Ok((names, Menu { title: "sign in to a provider".to_owned(), items, current, at: None }))
}

/// Signs in to the provider called `name` here: a key given with it, or
/// pasted in the box, where it shows as dots; a provider with a browser
/// sign-in, and ChatGPT, sign in there. Returns the provider that pays now,
/// or `None` if you stopped it.
pub(crate) async fn here(
    name: &str,
    given: Option<crate::commands::Key>,
    terminal: &mut Terminal,
    http: &rein_llm::reqwest::Client,
    providers: &[Provider],
) -> CliResult<Option<String>> {
    let pad = " ".repeat(GUTTER);
    let tell = |text: &str| {
        for line in text.trim_matches('\n').lines() {
            say!("{pad}{line}");
        }
    };
    if name == "chatgpt" {
        if given.is_some() {
            return Err("ChatGPT is signed in to in the browser: /login chatgpt takes no key".into());
        }
        let signed_in = waiting(terminal, "Waiting for you to sign in to ChatGPT in the browser", login(http, &tell)).await?;
        return Ok(signed_in.map(|()| "openai".to_owned()));
    }
    let provider = connect::find(providers, name)?;
    // A key typed after the name is checked and saved as one pasted is.
    let (key, made) = match given {
        Some(key) => (Some(key.into_inner()), false),
        None => {
            let made = connect::sign_in(provider, false)?;
            let key = match made {
                Some(auth_url) => {
                    let what = format!("Waiting for you to sign in to {} in the browser", provider.title);
                    waiting(terminal, &what, connect::key_from_browser(http, provider, auth_url, &tell)).await?
                }
                None => match terminal.input.read_secret(&connect::paste_prompt(provider)).await {
                    Some(Read::Line(key)) => Some(key),
                    Some(_) => None,
                    None => return Err(format!("a key typed here would show; run `rein login {name}` in a terminal instead")),
                },
            };
            (key, made.is_some())
        }
    };
    let Some(key) = key else { return Ok(None) };
    connect::save_key(http, provider, key, made, &tell).await?;
    Ok(Some(name.to_owned()))
}

/// Signed in to nothing that runs `spec`: the sign-in menu, as `/login` shows it,
/// until you sign in to something or leave with Esc. Then the model to
/// use: `spec` if you named it or are going on with a conversation
/// (`keep`), or once it's paid for; otherwise the model rein starts with
/// for what you signed in to, or one of its models, chosen and kept in the
/// settings. `None` if you left.
pub(crate) async fn first(http: &rein_llm::reqwest::Client, providers: &[Provider], keys: &mut Keys, terminal: &mut Terminal, spec: &str, keep: bool) -> CliResult<Option<String>> {
    let pad = " ".repeat(GUTTER);
    let shown = connect::resolve(spec, providers).map_or_else(|_| spec.to_owned(), |(provider, model)| provider::shown(provider, &model));
    say!("{pad}{}", terminal.err.dim(&format!("Sign in to a provider to use {shown}, or press Esc to leave.")));
    let signed_in = loop {
        let (names, menu) = menu(providers, keys, None)?;
        let Some(picked) = terminal.input.choose(menu).await else { return Ok(None) };
        match here(&names[picked], None, terminal, http, providers).await {
            Ok(Some(signed_in)) => break signed_in,
            Ok(None) => say!("{pad}{}", terminal.err.dim("(cancelled; nothing was saved)")),
            Err(e) => say!("{pad}{}", terminal.err.red(&format!("✗ {e}"))),
        }
    };
    *keys = connect::load_keys(&config_dir()?)?;
    if keep || !connect::unpaid(spec, providers, keys) {
        return Ok(Some(spec.to_owned()));
    }
    let start = connect::default_model(providers, keys);
    if !connect::unpaid(&start, providers, keys) {
        return Ok(Some(start));
    }
    // Signed in to a provider rein has no model to start with for: one of its own.
    let provider = connect::find(providers, &signed_in)?;
    let (client, _) = connect::client_for(http, provider, keys).await?;
    let models = match tokio::time::timeout(Duration::from_secs(10), client.list_models()).await {
        Ok(listed) => listed.map_err(describe)?,
        Err(_) => return Err(format!("{} didn't list its models in 10 seconds; name one with -m {}/<model>", provider.title, provider.name)),
    };
    let items = models.iter().map(|model| (provider::shown(provider, &model.id), if model.name == model.id { String::new() } else { model.name.clone() })).collect();
    let menu = Menu { title: "choose a model".to_owned(), items, current: None, at: None };
    let Some(picked) = terminal.input.choose(menu).await else { return Ok(None) };
    let spec = provider::spec(provider, &models[picked].id);
    // Otherwise the next rein would start with OpenAI's again, and ask again.
    let path = config_dir()?.join(crate::settings::FILE);
    crate::settings::set(&path, "model", &spec)?;
    say!("{pad}{} rein starts with {spec} from now on: model = {spec} in {}", terminal.err.green("✓"), crate::rules::shown(&path));
    Ok(Some(spec))
}
