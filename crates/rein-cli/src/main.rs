//! `rein`: a tiny, fast agent harness. You hold the reins.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use rein_llm::openai::signin::{self, Credentials};
use rein_agent::agent::{Agent, Approval, MAX_TURNS, OpenAi, Outcome, Ui};
use rein_agent::permissions::{Mode, Permissions};
use rein_agent::tools::Tools;
use rein_llm::openai::Client;
use rein_llm::{ToolCall, Usage, store};

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
  rein [options] <prompt...>     Ask rein to do something. It can read, write
                                 and edit files in this folder and run commands

Options:
  -m, --model <id>      Model to use (default: $REIN_MODEL, or gpt-6.1-sol)
      --effort <level>  Reasoning effort: low, medium, high, ...
  -s, --system <text>   Standing instructions for the model (the system prompt)
      --mode <mode>     What runs without asking:
                          ask        nothing; ask about every change (default)
                          edits      file changes in this folder
                          read-only  nothing runs or changes; only reading
                          trust      everything, except what deny rules block
      --record <file>   Also save the raw response stream, for tests

Rules in .rein/permissions refine the mode for commands, one per line:
  allow cargo test
  ask   git commit
  deny  git push
Deny beats ask beats allow, and a command only runs without asking if every
part of it is allowed. Rules decide when rein asks; they aren't a sandbox.

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
    /// The system prompt: standing instructions sent with every request.
    system: Option<String>,
    mode: Mode,
    record: Option<PathBuf>,
    prompt: String,
}

impl AskArgs {
    fn parse(args: Vec<String>) -> CliResult<Self> {
        let mut model = std::env::var("REIN_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
        let (mut effort, mut system, mut record, mut words) = (None, None, None, Vec::new());
        let mut mode = Mode::Ask;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-m" | "--model" => model = value(&mut args, &arg)?,
                "--effort" => effort = Some(value(&mut args, &arg)?),
                "--mode" => {
                    let name = value(&mut args, &arg)?;
                    mode = Mode::parse(&name).ok_or(format!("unknown mode {name}; use ask, edits, read-only or trust"))?;
                }
                "-s" | "--system" => system = Some(value(&mut args, &arg)?),
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
        Ok(AskArgs { model, effort, system, mode, record, prompt: words.join(" ") })
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> CliResult<String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

async fn ask(http: &rein_llm::reqwest::Client, args: AskArgs) -> CliResult {
    let client = client(http).await?;
    let record = match &args.record {
        Some(path) => Some(File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?),
        None => None,
    };
    let root = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
    let permissions = load_permissions(&root, args.mode)?;
    let mut agent = Agent {
        model: OpenAi { client, record },
        tools: Tools::new(root),
        permissions,
        model_name: args.model,
        instructions: args.system,
        reasoning_effort: args.effort,
        session_id: Some(rein_llm::openai::new_session_id().map_err(describe)?),
        history: Vec::new(),
    };
    let mut terminal = Terminal::default();
    let outcome = agent.send(args.prompt, &mut terminal).await;
    terminal.end_line();
    match outcome.map_err(describe)? {
        Outcome::Done => Ok(()),
        Outcome::TurnLimit => Err(format!("stopped after {MAX_TURNS} turns of tool calls; ask again to continue")),
    }
}

/// The mode plus the project's `.rein/permissions`, if it has one. Says what
/// it's using whenever that's anything other than the plain default.
fn load_permissions(root: &Path, mode: Mode) -> CliResult<Permissions> {
    let path = root.join(".rein").join("permissions");
    let rules = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("can't read .rein/permissions: {e}")),
    };
    let permissions = Permissions::new(mode, &rules).map_err(|e| format!(".rein/permissions {e}"))?;
    let count = permissions.rule_count();
    if count > 0 {
        eprintln!("Using {count} rule{} from .rein/permissions", if count == 1 { "" } else { "s" });
    }
    match mode {
        Mode::Ask => {}
        Mode::Edits => eprintln!("Mode: edits (file changes in this folder don't ask; commands do)"),
        Mode::ReadOnly => eprintln!("Mode: read-only (nothing runs or changes)"),
        Mode::Trust => eprintln!("Mode: trust. Commands run and files change without asking; only deny rules stop them"),
    }
    Ok(permissions)
}

/// The terminal as rein's [`Ui`]: answer text on stdout, everything else
/// (tool activity, questions, token counts) on stderr.
#[derive(Default)]
struct Terminal {
    mid_line: bool,
}

impl Terminal {
    fn end_line(&mut self) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
    }
}

impl Ui for Terminal {
    fn text(&mut self, delta: &str) {
        print!("{delta}");
        std::io::stdout().flush().ok();
        self.mid_line = !delta.ends_with('\n');
    }

    fn approve(&mut self, call: &ToolCall) -> Approval {
        self.end_line();
        if let Some(preview) = preview(call) {
            eprint!("{preview}");
        }
        let question = match call.name.as_str() {
            "bash" => format!("run {}?", show(call)),
            _ => format!("{}?", show(call)),
        };
        let always = match call.name.as_str() {
            "bash" => "a = yes, and don't ask again for this command",
            _ => "a = yes, and don't ask again about file changes",
        };
        eprint!("{question} [y/N/a] ({always}) ");
        let mut answer = String::new();
        // No answer (for example, input isn't a terminal) means no.
        if std::io::stdin().read_line(&mut answer).is_err() {
            return Approval::No;
        }
        approval(&answer)
    }

    fn tool_started(&mut self, call: &ToolCall) {
        self.end_line();
        eprintln!("> {}", show(call));
    }

    fn tool_blocked(&mut self, call: &ToolCall, reason: &str) {
        self.end_line();
        eprintln!("x {}: {reason}", show(call));
    }

    fn tool_finished(&mut self, _call: &ToolCall, output: &str) {
        let lines = output.lines().count();
        eprintln!("  ({lines} line{} back to the model)", if lines == 1 { "" } else { "s" });
    }

    fn usage(&mut self, usage: &Usage) {
        self.end_line();
        eprintln!("[{}]", usage_line(usage));
    }
}

/// Token counts for one request, with the share served from the prompt
/// cache, so you can see whether caching is working.
fn usage_line(u: &Usage) -> String {
    let cached = match u.cached_input_tokens {
        0 => "0 cached".to_owned(),
        n => format!("{n} cached, {}%", n * 100 / u.input_tokens.max(1)),
    };
    format!("{} in ({cached}), {} out ({} reasoning)", u.input_tokens, u.output_tokens, u.reasoning_tokens)
}

/// A tool call as a person reads it: `read src/main.rs`, `bash `ls``.
fn show(call: &ToolCall) -> String {
    let args: rein_llm::serde_json::Value = rein_llm::serde_json::from_str(&call.arguments).unwrap_or_default();
    match (call.name.as_str(), args["path"].as_str(), args["command"].as_str()) {
        ("read" | "write" | "edit", Some(path), _) => format!("{} {path}", call.name),
        ("bash", _, Some(command)) => format!("`{command}`"),
        _ => format!("{} {}", call.name, call.arguments),
    }
}

/// Reads a typed answer: y or a; anything else, including nothing, is no.
fn approval(answer: &str) -> Approval {
    match answer.trim() {
        "y" | "Y" | "yes" => Approval::Yes,
        "a" | "A" | "always" => Approval::Always,
        _ => Approval::No,
    }
}

/// What a file change would do, shown before you approve it: the lines an
/// edit removes (`-`) and adds (`+`), or the start of a file being written.
fn preview(call: &ToolCall) -> Option<String> {
    let args: rein_llm::serde_json::Value = rein_llm::serde_json::from_str(&call.arguments).ok()?;
    let mut out = String::new();
    match call.name.as_str() {
        "edit" => {
            if args["replace_all"].as_bool() == Some(true) {
                out.push_str("  (every occurrence)\n");
            }
            out.push_str(&marked(args["old_text"].as_str()?, '-'));
            out.push_str(&marked(args["new_text"].as_str()?, '+'));
        }
        "write" => out.push_str(&marked(args["content"].as_str()?, '+')),
        _ => return None,
    }
    Some(out)
}

/// The first lines of `text`, each prefixed with `mark`.
fn marked(text: &str, mark: char) -> String {
    const SHOWN: usize = 12;
    let mut out: String = text.lines().take(SHOWN).map(|line| format!("  {mark} {line}\n")).collect();
    let more = text.lines().count().saturating_sub(SHOWN);
    if more > 0 {
        out.push_str(&format!("  {mark} … {more} more lines\n"));
    }
    out
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
    fn edits_are_previewed_before_approval() {
        let call = |name: &str, args: rein_llm::serde_json::Value| ToolCall { call_id: "c".into(), name: name.into(), arguments: args.to_string() };
        let edit = call("edit", rein_llm::serde_json::json!({"path": "a.rs", "old_text": "fn a() {}", "new_text": "fn b() {}", "replace_all": false}));
        assert_eq!(show(&edit), "edit a.rs");
        assert_eq!(preview(&edit).unwrap(), "  - fn a() {}\n  + fn b() {}\n");
        let write = call("write", rein_llm::serde_json::json!({"path": "n.txt", "content": "hi\nthere"}));
        assert_eq!(preview(&write).unwrap(), "  + hi\n  + there\n");
        assert_eq!(preview(&call("bash", rein_llm::serde_json::json!({"command": "ls"}))), None);
    }

    #[test]
    fn reads_answers_and_modes() {
        assert_eq!(approval("y\n"), Approval::Yes);
        assert_eq!(approval("a\n"), Approval::Always);
        assert_eq!(approval("\n"), Approval::No);
        assert_eq!(approval("yolo"), Approval::No);
        let parsed = args("--mode edits fix the bug").unwrap();
        assert_eq!((parsed.mode, parsed.prompt.as_str()), (Mode::Edits, "fix the bug"));
        assert_eq!(args("--mode yolo hi").err().as_deref(), Some("unknown mode yolo; use ask, edits, read-only or trust"));
    }

    #[test]
    fn usage_shows_the_cache_hit_rate() {
        let usage = |input, cached| Usage { input_tokens: input, cached_input_tokens: cached, output_tokens: 6, reasoning_tokens: 0 };
        assert_eq!(usage_line(&usage(5347, 0)), "5347 in (0 cached), 6 out (0 reasoning)");
        assert_eq!(usage_line(&usage(5506, 5248)), "5506 in (5248 cached, 95%), 6 out (0 reasoning)");
    }

    #[test]
    fn masks_emails() {
        assert_eq!(mask_email("someone@example.com"), "so•••@example.com");
        assert_eq!(mask_email("a@example.com"), "a•••@example.com");
        assert_eq!(mask_email("not-an-email"), "•••");
    }

    fn args(line: &str) -> CliResult<AskArgs> {
        AskArgs::parse(line.split(' ').map(str::to_owned).collect())
    }

    #[test]
    fn parses_a_system_prompt() {
        let parsed = args("-s Be-brief what is a harness").unwrap();
        assert_eq!(parsed.system.as_deref(), Some("Be-brief"));
        assert_eq!(parsed.prompt, "what is a harness");

        assert_eq!(args("what is a harness").unwrap().system, None);
        assert_eq!(args("--system").err().as_deref(), Some("--system needs a value"));
    }
}
