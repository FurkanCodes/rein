//! `rein`: a tiny, fast agent harness. You hold the reins.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use rein_llm::openai::signin::{self, Credentials};
use rein_llm::store;

/// `eprintln!` that waits its turn: the spinner and the input box draw
/// from other threads.
macro_rules! say {
    () => { $crate::ui::screen::write("\n") };
    ($($arg:tt)*) => { $crate::ui::screen::write(&format!("{}\n", format_args!($($arg)*))) };
}

/// `print!` to stdout that waits its turn, as `say!` does.
macro_rules! out {
    ($($arg:tt)*) => { $crate::ui::screen::write_out(&format!($($arg)*)) };
}

mod args;
mod claude;
mod commands;
mod connect;
mod conversation;
mod effort;
mod format;
mod harden;
mod input;
mod peers;
mod settings;
use settings::Settings;
mod terminal;
mod ui;
use args::{AskArgs, HELP};
use conversation::ask;
use format::mask_email;

/// The name OpenAI shows for rein when someone signs in.
const AGENT_NAME: &str = "rein";
const DEFAULT_MODEL: &str = "gpt-6.1-sol";
const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
const CREDENTIALS_FILE: &str = "chatgpt.json";
const HOST_ID_FILE: &str = "host-id";

type CliResult<T = ()> = Result<T, String>;

fn main() -> ExitCode {
    // First, while rein has one thread: other programs can read the
    // environment it was started with, and the keys in it are rein's.
    rein_agent::sandbox::secrets::keep();
    if let Err(e) = harden::harden() {
        eprintln!("rein: a crash could write rein's memory, keys and all, to a file: {e}");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("rein: can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            say!("rein: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Vec<String>) -> CliResult {
    let http = rein_llm::http::client(concat!("rein/", env!("CARGO_PKG_VERSION"))).map_err(|e| e.to_string())?;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["-h" | "--help"] => {
            println!("{HELP}");
            Ok(())
        }
        ["login"] | ["login", "chatgpt"] => login(&http, &|line| say!("{line}")).await,
        ["login", "anthropic", "--oauth"] => claude::login_cli(&http).await,
        ["logout", "anthropic", "--oauth"] => connect::logout(claude::NAME, &|line| say!("{line}")),
        ["login", provider] => connect::login(&http, provider, false).await,
        ["login", provider, "--key"] => connect::login(&http, provider, true).await,
        ["logout", provider] => connect::logout(provider, &|line| say!("{line}")),
        ["providers"] => connect::list(),
        ["models"] => connect::models(&http, None).await,
        ["models", provider] => connect::models(&http, Some(provider)).await,
        ["peers"] => peers::dashboard::run(&peers::folder(&config_dir()?)).await,
        _ => {
            let settings = Settings::load(&config_dir()?.join(settings::FILE))?;
            let compact_at = settings.get("compact_at", "REIN_COMPACT_AT");
            ask(&http, AskArgs::parse(args, &settings)?, compact_at).await
        }
    }
}

/// Signs in with ChatGPT, saying how it goes with `tell`.
async fn login(http: &rein_llm::reqwest::Client, tell: &dyn Fn(&str)) -> CliResult {
    let dir = config_dir()?;
    let host_id = host_id(&dir)?;
    let path = dir.join(CREDENTIALS_FILE);
    let previous: Option<Credentials> = store::read_json(&path).map_err(describe)?;

    let pending = signin::begin(&host_id, AGENT_NAME, previous.as_ref()).await.map_err(describe)?;
    tell("Continue with ChatGPT: finish signing in in your browser...");
    if !open_browser(&pending.url).await {
        tell(&format!("Couldn't open a browser. Open this link to continue:\n\n{}\n", pending.url));
    }
    let credentials = tokio::time::timeout(Duration::from_secs(600), pending.finish(http))
        .await
        .map_err(|_| "sign-in timed out after 10 minutes; run `rein login` again".to_string())?
        .map_err(describe)?;
    if let Some(previous) = &previous
        && previous.subject != credentials.subject
    {
        return Err("that's a different ChatGPT account than the saved one; rein supports one account for now".into());
    }
    store::write_json_private(&path, &credentials).map_err(describe)?;

    let who = credentials.email.as_deref().map_or_else(|| "your ChatGPT account".to_owned(), mask_email);
    if !credentials.plan_enabled() {
        tell(&format!(
            "Signed in as {who}, but rein wasn't allowed to use your ChatGPT plan. \
             Run `rein login` again and allow it, or set OPENAI_API_KEY to pay with an API key."
        ));
        return Ok(());
    }
    if previous.is_none() {
        tell(&format!(
            "\nYou're using your ChatGPT plan.\n\
             Eligible requests in rein use your ChatGPT plan. Manage usage: {USAGE_URL}\n"
        ));
    }
    tell(&format!("Signed in as {who}."));
    Ok(())
}

/// Error text with the next step, where there's an obvious one.
fn describe(e: rein_llm::Error) -> String {
    if e.kind() == Some(rein_llm::ErrorKind::Refusal) {
        return format!("the model declined: {e}. Rephrase it, or try another model with /model");
    }
    if let rein_llm::Error::Http(http) = &e
        && http.is_connect()
    {
        let at = http.url().map(|url| url.origin().ascii_serialization()).unwrap_or_else(|| "the server".into());
        return format!("couldn't connect to {at}: is it running? ({})", with_causes(http));
    }
    match (e.code(), e.status()) {
        (Some("subscription_sharing_usage_limit_exceeded"), _) => {
            format!("usage limit reached for your ChatGPT plan or for rein. Manage usage: {USAGE_URL}")
        }
        (Some("subscription_sharing_user_not_eligible"), _) => {
            format!("ChatGPT plan usage isn't available for this account or workspace: {e}")
        }
        (Some("authentication_error"), _) => format!("{e}. Check $ANTHROPIC_API_KEY, or save another key with `rein login anthropic`"),
        // A bad API key's message already says what to do.
        (code, Some(401)) if code != Some("invalid_api_key") => format!("{e}. Run `rein login` to sign in again."),
        _ => with_causes(&e),
    }
}

/// `error`'s message, then each cause underneath it that says something
/// more: "error sending request: dns error: no such host", not just the
/// first part.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(underneath) = cause {
        let said = underneath.to_string();
        if !text.contains(&said) {
            text.push_str(": ");
            text.push_str(&said);
        }
        cause = underneath.source();
    }
    text
}

fn config_dir() -> CliResult<PathBuf> {
    store::config_dir().ok_or_else(|| "can't find your home directory; set REIN_HOME".into())
}

/// This installation's host ID, created on first use.
fn host_id(dir: &Path) -> CliResult<String> {
    let path = dir.join(HOST_ID_FILE);
    if let Ok(id) = std::fs::read_to_string(&path) {
        return Ok(id.trim().to_owned());
    }
    let id = signin::new_host_id().map_err(describe)?;
    store::write_private(&path, id.as_bytes()).map_err(describe)?;
    Ok(id)
}

/// Opens `url` with `$BROWSER` if it's set, as many tools do, or else the
/// system's way.
///
/// `open` and `xdg-open` hand the link over and exit at once, but a browser
/// named in `$BROWSER` may run until it's closed, and rein has one thread:
/// it waits a moment for a failure, and a browser still running then is
/// showing the page.
async fn open_browser(url: &str) -> bool {
    let mut command = match std::env::var("BROWSER").ok().filter(|b| !b.trim().is_empty()) {
        Some(browser) => {
            let mut words = browser.split_whitespace();
            let mut command = tokio::process::Command::new(words.next().unwrap_or_default());
            command.args(words);
            command
        }
        None if cfg!(target_os = "macos") => tokio::process::Command::new("open"),
        None if cfg!(windows) => {
            let mut command = tokio::process::Command::new("cmd");
            command.args(["/C", "start", ""]);
            command
        }
        None => tokio::process::Command::new("xdg-open"),
    };
    let Ok(mut child) = command.arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return false };
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(status) => status.is_ok_and(|s| s.success()),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_went_wrong_underneath() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let wrapped = std::io::Error::other(io);
        assert_eq!(with_causes(&rein_llm::Error::from(wrapped)), "connection refused");
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let outer = Outer(std::io::Error::other("dns error: no such host"));
        assert_eq!(with_causes(&outer), "error sending request: dns error: no such host");
    }
}
