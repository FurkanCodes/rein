//! `rein`: a tiny, fast agent harness. You hold the reins.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use rein_llm::openai::signin::{self, Credentials};
use rein_llm::openai::{Client, Message, Request};
use rein_llm::{Event, store};

/// The name OpenAI shows for rein when someone signs in.
const AGENT_NAME: &str = "rein";
const DEFAULT_MODEL: &str = "gpt-6.1-sol";
const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
const CREDENTIALS_FILE: &str = "chatgpt.json";
const HOST_ID_FILE: &str = "host-id";

const HELP: &str = "\
rein: a tiny, fast agent harness. You hold the reins.

Usage:
  rein login                     Continue with ChatGPT (use your ChatGPT plan)
  rein models                    List the models you can use
  rein [options] <prompt...>     Ask the model and stream the answer

Options:
  -m, --model <id>      Model to use (default: $REIN_MODEL, or gpt-6.1-sol)
      --effort <level>  Reasoning effort: low, medium, high, ...
      --record <file>   Also save the raw response stream, for tests

Without `rein login`, rein uses $OPENAI_API_KEY, billed to your API account.";

type CliResult<T = ()> = Result<T, String>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rein: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Vec<String>) -> CliResult {
    let http = rein_llm::reqwest::Client::builder()
        .user_agent(concat!("rein/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["-h" | "--help"] => {
            println!("{HELP}");
            Ok(())
        }
        ["login"] => login(&http).await,
        ["models"] => models(&http).await,
        _ => ask(&http, AskArgs::parse(args)?).await,
    }
}

async fn login(http: &rein_llm::reqwest::Client) -> CliResult {
    let dir = config_dir()?;
    let host_id = host_id(&dir)?;
    let path = dir.join(CREDENTIALS_FILE);
    let previous: Option<Credentials> = store::read_json(&path).map_err(describe)?;

    let pending = signin::begin(&host_id, AGENT_NAME, previous.as_ref()).await.map_err(describe)?;
    eprintln!("Continue with ChatGPT: finish signing in in your browser...");
    if !open_browser(&pending.url) {
        eprintln!("Couldn't open a browser. Open this link to continue:\n\n{}\n", pending.url);
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
        eprintln!(
            "Signed in as {who}, but rein wasn't allowed to use your ChatGPT plan. \
             Run `rein login` again and allow it, or set OPENAI_API_KEY to pay with an API key."
        );
        return Ok(());
    }
    if previous.is_none() {
        eprintln!(
            "\nYou're using your ChatGPT plan.\n\
             Eligible requests in rein use your ChatGPT plan. Manage usage: {USAGE_URL}\n"
        );
    }
    eprintln!("Signed in as {who}.");
    Ok(())
}

async fn models(http: &rein_llm::reqwest::Client) -> CliResult {
    let client = client(http).await?;
    for model in client.list_models().await.map_err(describe)? {
        if model.name == model.id {
            println!("{}", model.id);
        } else {
            println!("{:<24} {}", model.id, model.name);
        }
    }
    Ok(())
}

struct AskArgs {
    model: String,
    effort: Option<String>,
    record: Option<PathBuf>,
    prompt: String,
}

impl AskArgs {
    fn parse(args: Vec<String>) -> CliResult<Self> {
        let mut model = std::env::var("REIN_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
        let (mut effort, mut record, mut words) = (None, None, Vec::new());
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-m" | "--model" => model = value(&mut args, &arg)?,
                "--effort" => effort = Some(value(&mut args, &arg)?),
                "--record" => record = Some(PathBuf::from(value(&mut args, &arg)?)),
                "--" => words.extend(args.by_ref()),
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(format!("unknown option {flag}; see `rein --help`"));
                }
                _ => words.push(arg),
            }
        }
        if words.is_empty() {
            return Err("nothing to ask; see `rein --help`".into());
        }
        Ok(AskArgs { model, effort, record, prompt: words.join(" ") })
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> CliResult<String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

async fn ask(http: &rein_llm::reqwest::Client, args: AskArgs) -> CliResult {
    let client = client(http).await?;
    let request = Request {
        model: args.model,
        instructions: None,
        input: vec![Message::user(args.prompt)],
        reasoning_effort: args.effort,
    };
    let mut stream = client.stream(&request).await.map_err(describe)?;
    if let Some(path) = &args.record {
        let file = File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?;
        stream.record_to(file);
    }

    let mut stdout = std::io::stdout();
    let mut mid_line = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::TextDelta(text)) => {
                print!("{text}");
                stdout.flush().ok();
                mid_line = !text.ends_with('\n');
            }
            Ok(Event::Completed { usage }) => {
                if mid_line {
                    println!();
                }
                eprintln!(
                    "[{} in ({} cached), {} out ({} reasoning)]",
                    usage.input_tokens, usage.cached_input_tokens, usage.output_tokens, usage.reasoning_tokens
                );
            }
            Err(e) => {
                if mid_line {
                    println!();
                }
                return Err(describe(e));
            }
        }
    }
    Ok(())
}

/// A client using the saved ChatGPT sign-in, or else `OPENAI_API_KEY`.
/// Prints which one, so it's always clear who pays.
async fn client(http: &rein_llm::reqwest::Client) -> CliResult<Client> {
    let path = config_dir()?.join(CREDENTIALS_FILE);
    if let Some(mut credentials) = store::read_json::<Credentials>(&path).map_err(describe)? {
        if !credentials.plan_enabled() {
            return Err("your ChatGPT sign-in doesn't allow using your plan; run `rein login` and allow it".into());
        }
        if credentials.needs_refresh() {
            credentials = signin::refresh(http, &credentials).await.map_err(|e| {
                format!("couldn't renew your ChatGPT sign-in ({}); run `rein login` to sign in again", describe(e))
            })?;
            store::write_json_private(&path, &credentials).map_err(describe)?;
        }
        eprintln!("Using ChatGPT plan (manage usage: {USAGE_URL})");
        return Ok(Client::new(http.clone(), credentials.access_token));
    }
    match std::env::var("OPENAI_API_KEY") {
        Ok(key) if !key.is_empty() => {
            eprintln!("Using OPENAI_API_KEY (billed to your API account)");
            Ok(Client::new(http.clone(), key))
        }
        _ => Err("not signed in. Run `rein login` to continue with ChatGPT, or set OPENAI_API_KEY.".into()),
    }
}

/// Error text with the next step, where there's an obvious one.
fn describe(e: rein_llm::Error) -> String {
    match (e.code(), e.status()) {
        (Some("subscription_sharing_usage_limit_exceeded"), _) => {
            format!("usage limit reached for your ChatGPT plan or for rein. Manage usage: {USAGE_URL}")
        }
        (Some("subscription_sharing_user_not_eligible"), _) => {
            format!("ChatGPT plan usage isn't available for this account or workspace: {e}")
        }
        // A bad API key's message already says what to do.
        (code, Some(401)) if code != Some("invalid_api_key") => format!("{e}. Run `rein login` to sign in again."),
        _ => e.to_string(),
    }
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

/// `someone@example.com` -> `so•••@example.com`: enough to recognize your own
/// account, safe to show on a screen recording or stream.
fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((name, domain)) => format!("{}•••@{domain}", name.chars().take(2).collect::<String>()),
        None => "•••".to_owned(),
    }
}

fn open_browser(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        Command::new("xdg-open")
    };
    command.arg(url).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_emails() {
        assert_eq!(mask_email("someone@example.com"), "so•••@example.com");
        assert_eq!(mask_email("a@example.com"), "a•••@example.com");
        assert_eq!(mask_email("not-an-email"), "•••");
    }
}
