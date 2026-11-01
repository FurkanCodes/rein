//! `rein`: a tiny, fast agent harness. You hold the reins.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rein_llm::openai::signin::{self, Credentials};
use rein_agent::agent::{Agent, Approval, Compacted, MAX_TURNS, OpenAi, Outcome, Ui, Window};
use rein_agent::compact;
use rein_agent::permissions::{Mode, Permissions};
use rein_agent::session::{self, Header, Loaded, Session};
use rein_agent::tools::Tools;
use rein_agent::{context, prompt};
use rein_llm::Item;
use rein_llm::openai::Client;
use rein_llm::{ToolCall, Usage, store};

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

mod commands;
mod settings;
use settings::Settings;
mod ui;
use commands::Slash;
use ui::editor::{Action, Editor};
use ui::keys::{Key, Keys};
use ui::markdown::Markdown;
use ui::screen::{self, Bottom, Pinned, Raw};
use ui::spinner::Spinner;
use ui::steps;
use ui::style::{GUTTER, Style, strip};
use ui::viewer::{Entry, Viewer};
use ui::wrap::Wrap;

/// The name OpenAI shows for rein when someone signs in.
const AGENT_NAME: &str = "rein";
const DEFAULT_MODEL: &str = "gpt-6.1-sol";
const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
const CREDENTIALS_FILE: &str = "chatgpt.json";
const HOST_ID_FILE: &str = "host-id";

const HELP: &str = "\
rein: a tiny, fast agent harness. You hold the reins.

Usage:
  rein [options]                 Start a conversation in this folder
  rein [options] <prompt...>     Do one task and exit
  rein -c [<prompt...>]          Continue the latest conversation in this folder
  rein -r [<prompt...>]          Choose a saved conversation to continue
  rein login                     Continue with ChatGPT (use your ChatGPT plan)
  rein models                    List the models you can use

rein can read, write and edit files in this folder and run commands. In a
conversation, type /exit or press Ctrl-D to leave. Conversations are saved in
~/.config/rein/sessions, readable only by you.

A long conversation is compacted when it nears $REIN_COMPACT_AT tokens
(default 150000): stale tool output goes first, then the model summarizes.
Type /compact [focus] to do it now. Nothing is lost: the model can search
the whole conversation with its recall tool.

rein tells the model about the project with its AGENTS.md files, from the
repository's root down to this folder (CLAUDE.md where a folder has no
AGENTS.md), plus your own ~/.config/rein/AGENTS.md. 32 KB at most.

Options:
  -m, --model <id>      Model to use (default: $REIN_MODEL, or gpt-6.1-sol)
      --effort <level>  Reasoning effort: low, medium, high, ...
  -c, --continue        Continue the latest conversation in this folder
  -r, --resume          Choose which saved conversation to continue
  -s, --system <text>   Standing instructions, added to rein's system prompt
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

Defaults go in ~/.config/rein/config, one per line: model, effort, mode and
compact_at, like `mode = edits`. Flags win over $REIN_MODEL, $REIN_EFFORT,
$REIN_MODE and $REIN_COMPACT_AT, which win over the file.

Without `rein login`, rein uses $OPENAI_API_KEY, billed to your API account.";

type CliResult<T = ()> = Result<T, String>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            say!("rein: {message}");
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
        ["-h" | "--help"] => {
            println!("{HELP}");
            Ok(())
        }
        ["login"] => login(&http).await,
        ["models"] => models(&http).await,
        _ => {
            let settings = Settings::load(&config_dir()?.join(settings::FILE))?;
            let compact_at = settings.get("compact_at", "REIN_COMPACT_AT");
            ask(&http, AskArgs::parse(args, &settings)?, compact_at).await
        }
    }
}

async fn login(http: &rein_llm::reqwest::Client) -> CliResult {
    let dir = config_dir()?;
    let host_id = host_id(&dir)?;
    let path = dir.join(CREDENTIALS_FILE);
    let previous: Option<Credentials> = store::read_json(&path).map_err(describe)?;

    let pending = signin::begin(&host_id, AGENT_NAME, previous.as_ref()).await.map_err(describe)?;
    say!("Continue with ChatGPT: finish signing in in your browser...");
    if !open_browser(&pending.url) {
        say!("Couldn't open a browser. Open this link to continue:\n\n{}\n", pending.url);
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
        say!(
            "Signed in as {who}, but rein wasn't allowed to use your ChatGPT plan. \
             Run `rein login` again and allow it, or set OPENAI_API_KEY to pay with an API key."
        );
        return Ok(());
    }
    if previous.is_none() {
        say!(
            "\nYou're using your ChatGPT plan.\n\
             Eligible requests in rein use your ChatGPT plan. Manage usage: {USAGE_URL}\n"
        );
    }
    say!("Signed in as {who}.");
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

/// Which conversation to work in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Resume {
    New,
    /// The most recently used one in this folder.
    Latest,
    /// One the user picks from a list.
    Pick,
}

struct AskArgs {
    model: String,
    /// `-m` was given, so a continued conversation switches to `model`.
    model_given: bool,
    resume: Resume,
    effort: Option<String>,
    /// The system prompt: standing instructions sent with every request.
    system: Option<String>,
    mode: Mode,
    record: Option<PathBuf>,
    /// `None` starts a conversation instead of doing one task.
    prompt: Option<String>,
}

impl AskArgs {
    /// Flags first; what they don't say comes from `settings` (and the
    /// environment), then rein's defaults.
    fn parse(args: Vec<String>, settings: &Settings) -> CliResult<Self> {
        let mut model = settings.get("model", "REIN_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let (mut effort, mut system, mut record, mut words) = (settings.get("effort", "REIN_EFFORT"), None, None, Vec::new());
        let mut mode = match settings.get("mode", "REIN_MODE") {
            Some(name) => Mode::parse(&name).ok_or(format!("unknown mode {name} in the settings; use ask, edits, read-only or trust"))?,
            None => Mode::Ask,
        };
        let (mut model_given, mut resume) = (false, Resume::New);
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-m" | "--model" => {
                    model = value(&mut args, &arg)?;
                    model_given = true;
                }
                "-c" | "--continue" => resume = Resume::Latest,
                "-r" | "--resume" => resume = Resume::Pick,
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
        let prompt = if words.is_empty() { None } else { Some(words.join(" ")) };
        Ok(AskArgs { model, model_given, resume, effort, system, mode, record, prompt })
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> CliResult<String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

async fn ask(http: &rein_llm::reqwest::Client, args: AskArgs, compact_at: Option<String>) -> CliResult {
    let client = client(http).await?;
    // The model list says how big each model's context window is. Fetched
    // while everything else gets ready.
    let models = {
        let client = client.clone();
        tokio::spawn(async move { client.list_models().await })
    };
    let record = match &args.record {
        Some(path) => Some(File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?),
        None => None,
    };
    let root = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
    let permissions = load_permissions(&root, args.mode)?;
    let mut terminal = Terminal::new(root.clone(), args.mode, args.prompt.is_none());
    let dir = config_dir()?;
    let mut setup = Setup {
        folder: session::folder(&dir.join("sessions"), &root),
        personal: dir.join("AGENTS.md"),
        root: root.clone(),
        system: args.system.clone(),
        models: Vec::new(),
        compact_at,
    };

    let saved = match args.resume {
        Resume::New => None,
        Resume::Latest => match session::list(&setup.folder, 1).into_iter().next() {
            Some(latest) => Some(latest.path),
            None => return Err("no saved conversation in this folder yet".into()),
        },
        Resume::Pick => match pick(&setup.folder, &mut terminal).await? {
            Some(path) => Some(path),
            None => return Ok(()),
        },
    };
    if saved.is_some() && args.system.is_some() {
        return Err("-s can't change a saved conversation's system prompt; start a new one instead".into());
    }
    let (loaded, context) = setup.open(saved, &args.model)?;
    let mut loaded = loaded;
    // A continued conversation keeps its model unless -m says otherwise,
    // and then it's saved with it.
    if args.resume != Resume::New && args.model_given && loaded.session.header().model != args.model {
        loaded.session.append_model(&args.model).map_err(describe)?;
    }
    let header = loaded.session.header().clone();
    let model_name = if args.model_given || args.resume == Resume::New { args.model } else { header.model };
    setup.models = match tokio::time::timeout(Duration::from_secs(5), models).await {
        Ok(Ok(Ok(models))) => models,
        _ => Vec::new(),
    };
    let limit = setup.limit(&model_name)?;
    terminal.context_limit = limit;
    terminal.model = model_name.clone();
    terminal.input.set_bar(terminal.bar());
    let mut agent = Agent {
        model: OpenAi { client, record },
        tools: Tools::new(root),
        permissions,
        model_name,
        instructions: header.instructions,
        reasoning_effort: args.effort,
        session_id: Some(header.id),
        history: loaded.history,
        session: Some(loaded.session),
        context,
        window: Window { limit, measured: None, full: loaded.full },
    };
    match args.prompt {
        Some(prompt) => {
            terminal.start_turn(Some(&prompt));
            turn(&mut agent, prompt, &mut terminal).await
        }
        None => conversation(&mut agent, &mut terminal, &mut setup).await,
    }
}

/// What it takes to start or reopen a conversation in this folder, kept
/// for `/new`, `/resume` and `/model`.
struct Setup {
    root: PathBuf,
    /// Where this folder's conversations are saved.
    folder: PathBuf,
    /// Your own AGENTS.md, for every project.
    personal: PathBuf,
    /// `-s`: standing instructions added to the system prompt.
    system: Option<String>,
    /// The models you can use, with their context windows.
    models: Vec<rein_llm::openai::ModelInfo>,
    /// `compact_at` from the settings or the environment.
    compact_at: Option<String>,
}

impl Setup {
    /// A saved conversation (`saved`), or a new one: a header, the system
    /// prompt, and the first message with the project's instructions.
    fn open(&self, saved: Option<PathBuf>, model: &str) -> CliResult<(Loaded, Option<String>)> {
        if let Some(path) = saved {
            let loaded = Session::load(&path).map_err(describe)?;
            say_where_we_were(loaded.session.header(), &loaded.full);
            return Ok((loaded, None));
        }
        let now = local_time(SystemTime::now());
        let header = Header {
            id: rein_llm::openai::new_session_id().map_err(describe)?,
            cwd: self.root.clone(),
            started: format!("{} {:02}:{:02}", now.date(), now.hour, now.minute),
            model: model.to_owned(),
            instructions: Some(prompt::system(self.system.as_deref())),
        };
        let stamp = format!("{}T{:02}-{:02}-{:02}", now.date(), now.hour, now.minute, now.second);
        let files = context::find(&self.root, Some(&self.personal));
        if !files.is_empty() {
            let names: Vec<String> = files.iter().map(|f| shown_path(&f.path, &self.root)).collect();
            say!("Instructions: {}", names.join(", "));
        }
        let context = context::message(&self.root, std::env::consts::OS, &now.date(), &files);
        let session = Session::new(&self.folder, &stamp, header);
        Ok((Loaded { session, history: Vec::new(), full: Vec::new() }, Some(context)))
    }

    /// When to compact with `model`: see [`compact_limit`].
    fn limit(&self, model: &str) -> CliResult<u64> {
        let window = self.models.iter().find(|m| m.id == model).and_then(|m| m.context_window);
        compact_limit(window, self.compact_at.as_deref())
    }
}

/// One message from you, worked on until the model is done, or until
/// Ctrl-C. Interrupting drops the turn mid-flight: the request is closed,
/// a running command is killed, and the history is repaired so the
/// conversation can go on.
async fn turn(agent: &mut Agent<OpenAi>, prompt: String, terminal: &mut Terminal) -> CliResult {
    let interrupted = terminal.interrupted.clone();
    let outcome = tokio::select! {
        outcome = agent.send(prompt, terminal) => Some(outcome),
        _ = tokio::signal::ctrl_c() => None,
        _ = interrupted.notified() => None,
    };
    terminal.end_line();
    let Some(outcome) = outcome else {
        agent.repair();
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(interrupted)"));
        terminal.end_turn();
        return Ok(());
    };
    terminal.end_turn();
    match outcome.map_err(describe)? {
        Outcome::Done => Ok(()),
        Outcome::TurnLimit => Err(format!("stopped after {MAX_TURNS} turns of tool calls; ask again to continue")),
    }
}

/// When to compact: `compact_at` tokens if set (the settings, or
/// $REIN_COMPACT_AT), otherwise rein's default, and never later than 90% of
/// the model's window, when known.
fn compact_limit(window: Option<u64>, compact_at: Option<&str>) -> CliResult<u64> {
    let wanted = match compact_at {
        Some(value) => value.trim().parse::<u64>().map_err(|_| format!("compact_at should be a number of tokens, not {value}"))?,
        None => compact::DEFAULT_LIMIT,
    };
    Ok(window.map_or(wanted, |window| wanted.min(window / 10 * 9)))
}

/// Compacts the conversation now, keeping `focus` in mind. Ctrl-C stops it
/// and leaves the conversation as it was.
async fn compact_now(agent: &mut Agent<OpenAi>, focus: Option<&str>, terminal: &mut Terminal) -> CliResult {
    let interrupted = terminal.interrupted.clone();
    let done = tokio::select! {
        done = agent.compact(focus, true, terminal) => Some(done),
        _ = tokio::signal::ctrl_c() => None,
        _ = interrupted.notified() => None,
    };
    terminal.end_turn();
    match done {
        Some(done) => done.map(|_| ()).map_err(describe),
        None => {
            say!("(interrupted; nothing was compacted)");
            Ok(())
        }
    }
}

/// Lists this folder's recent conversations and asks which to continue.
/// `None` if the user changes their mind.
async fn pick(folder: &Path, terminal: &mut Terminal) -> CliResult<Option<PathBuf>> {
    let saved = session::list(folder, 10);
    if saved.is_empty() {
        return Err("no saved conversations in this folder yet".into());
    }
    for (i, conversation) in saved.iter().enumerate() {
        let when = local_time(conversation.modified);
        let first = conversation.first.as_deref().map(|text| shorten(text, 50)).unwrap_or_default();
        let messages = format!("{} message{}", conversation.messages, if conversation.messages == 1 { "" } else { "s" });
        say!("{:>2}  {} {:02}:{:02}  {messages:<12}  {first}", i + 1, when.date(), when.hour, when.minute);
    }
    loop {
        match terminal.input.read("Continue which one? (number, or Enter to cancel) ", false).await {
            Read::Line(line) if line.trim().is_empty() => return Ok(None),
            Read::Line(line) => match line.trim().parse::<usize>() {
                Ok(n) if (1..=saved.len()).contains(&n) => return Ok(Some(saved[n - 1].path.clone())),
                _ => say!("Type a number from 1 to {}.", saved.len()),
            },
            Read::Interrupted | Read::Eof => return Ok(None),
        }
    }
}

/// A reminder of where a continued conversation left off.
fn say_where_we_were(header: &Header, history: &[Item]) {
    // The first message is the context rein added, not something typed.
    let typed: Vec<&str> = history
        .iter()
        .skip(1)
        .filter_map(|item| match item {
            Item::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    say!("Continuing the conversation from {} ({} message{} so far).", header.started, typed.len(), if typed.len() == 1 { "" } else { "s" });
    if let Some(last) = typed.last() {
        say!("Last message: {}", shorten(last, 70));
    }
}

/// The first line of `text`, at most `max` characters.
fn shorten(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= max && !text.trim_end().contains('\n') {
        return line.to_owned();
    }
    let cut: String = line.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

/// `path` relative to the working folder (`../AGENTS.md`), so it reads
/// shorter; absolute when they only share the filesystem root.
fn shown_path(path: &Path, cwd: &Path) -> String {
    for (up, folder) in cwd.ancestors().enumerate() {
        if folder.parent().is_none() {
            break;
        }
        if let Ok(rest) = path.strip_prefix(folder) {
            return format!("{}{}", "../".repeat(up), rest.display());
        }
    }
    path.display().to_string()
}

/// A moment in the computer's local time zone.
struct LocalTime {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

impl LocalTime {
    fn date(&self) -> String {
        format!("{}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn local_time(time: SystemTime) -> LocalTime {
    let seconds = time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value, and localtime_r only
    // writes into the one we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

/// Message after message with the same agent, so it remembers the whole
/// conversation, until /exit or Ctrl-D. A failed turn is reported and the
/// conversation goes on.
async fn conversation(agent: &mut Agent<OpenAi>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    if !terminal.input.pinned() {
        say!("Type a message, or /help for commands. End a line with \\ to keep typing on the next.");
        say!("Ctrl-C stops an answer; /exit or Ctrl-D leaves.");
    }
    let mut last_interrupt: Option<std::time::Instant> = None;
    loop {
        // The box starts each message on its own; plain lines need a gap.
        if !terminal.input.pinned() {
            say!();
        }
        let Some(line) = message(terminal, &mut last_interrupt).await else {
            return Ok(());
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match commands::parse(line) {
            Some(Ok(Slash::Exit)) => return Ok(()),
            Some(Ok(command)) => {
                if terminal.input.pinned() {
                    say!("\n{}{line}", terminal.err.label("you"));
                }
                if let Err(e) = run_command(command, agent, terminal, setup).await {
                    say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&e));
                }
                terminal.input.set_bar(terminal.bar());
            }
            Some(Err(e)) => {
                if terminal.input.pinned() {
                    say!("\n{}{line}", terminal.err.label("you"));
                }
                say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&e));
            }
            None => {
                // A plain terminal already shows what was typed; the box doesn't.
                let echo = terminal.input.pinned().then_some(line);
                terminal.start_turn(echo);
                if let Err(e) = turn(agent, line.to_owned(), terminal).await {
                    say!("rein: {e}");
                }
            }
        }
    }
}

/// Does what a slash command asks. What it says goes under the gutter,
/// like a step's result.
async fn run_command(command: Slash, agent: &mut Agent<OpenAi>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    let pad = " ".repeat(GUTTER);
    let s = terminal.err;
    match command {
        Slash::Help => {
            for line in commands::help() {
                say!("{pad}{line}");
            }
            say!("{pad}{}", s.dim("Ctrl-O shows a step's full output; \\ or Ctrl-J starts a new line."));
        }
        Slash::Mode(None) => say!("{pad}{}", mode_description(agent.permissions.mode)),
        Slash::Mode(Some(mode)) => {
            agent.permissions.mode = mode;
            terminal.mode = mode_name(mode);
            say!("{pad}{} {}", s.green("✓"), mode_description(mode));
        }
        Slash::Model(None) => {
            if setup.models.is_empty() {
                say!("{pad}{} (the model list couldn't be fetched)", agent.model_name);
            }
            for model in &setup.models {
                let current = model.id == agent.model_name;
                let window = model.context_window.map(|w| format!("  {} tokens", thousands(w))).unwrap_or_default();
                let line = format!("{} {}{}", if current { "›" } else { " " }, model.id, s.dim(&window));
                say!("{pad}{}", if current { s.bold(&line) } else { line });
            }
        }
        Slash::Model(Some(id)) => {
            if !setup.models.is_empty() && !setup.models.iter().any(|m| m.id == id) {
                return Err(format!("no model called {id}; /model lists them"));
            }
            agent.window.limit = setup.limit(&id)?;
            terminal.context_limit = agent.window.limit;
            if let Some(session) = &mut agent.session {
                session.append_model(&id).map_err(describe)?;
            }
            agent.model_name = id.clone();
            terminal.model = id.clone();
            say!("{pad}{} now using {id}", s.green("✓"));
            say!("{pad}{}", s.dim("The prompt cache starts over: it's kept per model."));
        }
        Slash::Effort(None) => {
            say!("{pad}reasoning effort: {}", agent.reasoning_effort.as_deref().unwrap_or("the model's default"));
        }
        Slash::Effort(Some(level)) => {
            agent.reasoning_effort = level;
            say!("{pad}{} reasoning effort: {}", s.green("✓"), agent.reasoning_effort.as_deref().unwrap_or("the model's default"));
        }
        Slash::Status => {
            let header = agent.session.as_ref().map(|session| session.header().clone());
            let messages = agent.window.full.iter().skip(1).filter(|item| matches!(item, Item::User(_))).count();
            let rows = [
                ("model", agent.model_name.clone()),
                ("effort", agent.reasoning_effort.clone().unwrap_or_else(|| "the model's default".into())),
                ("mode", mode_description(agent.permissions.mode).to_owned()),
                ("rules", format!("{} from .rein/permissions", agent.permissions.rule_count())),
                ("context", format!("about {} of {} tokens before compacting", thousands(agent.estimated()), thousands(agent.window.limit))),
                ("started", header.map(|h| format!("{} · {messages} message{} so far", h.started, if messages == 1 { "" } else { "s" })).unwrap_or_default()),
                ("folder", setup.root.display().to_string()),
            ];
            for (name, value) in rows {
                say!("{pad}{}  {value}", s.dim(&format!("{name:<8}")));
            }
        }
        Slash::Compact(focus) => compact_now(agent, focus.as_deref(), terminal).await?,
        Slash::New => {
            let (loaded, context) = setup.open(None, &agent.model_name)?;
            switch(agent, loaded, context);
            say!("{pad}{} a fresh conversation; the last one is saved, /resume brings it back", s.green("✓"));
        }
        Slash::Resume => {
            let Some(path) = pick(&setup.folder, terminal).await? else { return Ok(()) };
            let (loaded, context) = setup.open(Some(path), &agent.model_name)?;
            let model = loaded.session.header().model.clone();
            switch(agent, loaded, context);
            if model != agent.model_name {
                agent.window.limit = setup.limit(&model)?;
                terminal.context_limit = agent.window.limit;
                agent.model_name = model.clone();
                terminal.model = model;
            }
        }
        Slash::Exit => {}
    }
    Ok(())
}

/// Moves the agent to another conversation. The mode, the rules and the
/// model stay; what you said "always" to was for the old one.
fn switch(agent: &mut Agent<OpenAi>, loaded: Loaded, context: Option<String>) {
    let header = loaded.session.header().clone();
    agent.session_id = Some(header.id);
    agent.instructions = header.instructions;
    agent.history = loaded.history;
    agent.session = Some(loaded.session);
    agent.context = context;
    agent.window.full = loaded.full;
    agent.window.measured = None;
    agent.permissions.forget_approvals();
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask",
        Mode::Edits => "edits",
        Mode::ReadOnly => "read-only",
        Mode::Trust => "trust",
    }
}

fn mode_description(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "ask: file changes and commands ask first",
        Mode::Edits => "edits: file changes in this folder don't ask; commands do",
        Mode::ReadOnly => "read-only: nothing runs or changes",
        Mode::Trust => "trust: everything runs without asking; only deny rules stop it",
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
        say!("Using {count} rule{} from .rein/permissions", if count == 1 { "" } else { "s" });
    }
    match mode {
        Mode::Ask => {}
        Mode::Edits => say!("Mode: edits (file changes in this folder don't ask; commands do)"),
        Mode::ReadOnly => say!("Mode: read-only (nothing runs or changes)"),
        Mode::Trust => say!("Mode: trust. Commands run and files change without asking; only deny rules stop them"),
    }
    Ok(permissions)
}

/// Reads your next message. A line ending in `\` continues on the next.
/// Ctrl-C clears what you typed; a second one within two seconds, or
/// Ctrl-D, ends the conversation (`None`).
async fn message(terminal: &mut Terminal, last_interrupt: &mut Option<std::time::Instant>) -> Option<String> {
    let mut text = String::new();
    loop {
        let prompt = if text.is_empty() { "you    " } else { "…      " };
        let read = tokio::select! {
            read = terminal.input.read(prompt, true) => read,
            // Without an editor, Ctrl-C is a signal; the terminal has
            // already thrown away what was typed.
            _ = tokio::signal::ctrl_c() => Read::Interrupted,
        };
        match read {
            Read::Line(line) => match line.strip_suffix('\\') {
                Some(more) => {
                    text.push_str(more);
                    text.push('\n');
                }
                None => {
                    text.push_str(&line);
                    return Some(text);
                }
            },
            Read::Interrupted => {
                if last_interrupt.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    return None;
                }
                *last_interrupt = Some(std::time::Instant::now());
                say!("{}", terminal.err.dim("(press Ctrl-C again to leave)"));
                text.clear();
            }
            Read::Eof => return None,
        }
    }
}

/// The width of the terminal showing rein's answers, if stdout is one.
fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a `winsize` into the struct we pass.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// What reading a line produced.
enum Read {
    Line(String),
    /// Ctrl-C while typing. In a real terminal the editor turns it into a
    /// keypress, so it arrives here instead of as a signal.
    Interrupted,
    /// Ctrl-D, or input ended.
    Eof,
}

/// What the keyboard is for right now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Waiting {
    /// rein is working; keys type ahead, Ctrl-C or Esc stop the turn.
    Nothing,
    Message,
    /// The answer to a question, like y, n or a.
    Answer,
}

/// The keyboard, read by its own thread so that waiting for a key never
/// stops rein from noticing Ctrl-C. In a terminal it drives the pinned
/// input box; otherwise it reads plain lines, so piping into rein works.
struct Input {
    /// Plain lines: prompts to show; the thread reads one line for each.
    requests: std::sync::mpsc::Sender<String>,
    replies: tokio::sync::mpsc::UnboundedReceiver<Read>,
    /// A read asked for but not answered yet, kept if the asker gave up.
    pending: bool,
    /// The pinned box and bar, when rein runs in a terminal.
    boxed: Option<std::sync::Arc<std::sync::Mutex<Boxed>>>,
    /// Puts the keyboard back when rein ends.
    _raw: Option<Raw>,
}

/// The input box, shared by the keyboard's thread and the rest of rein.
struct Boxed {
    pinned: Pinned,
    editor: Editor,
    waiting: Waiting,
    /// The question being answered, shown in the empty box.
    question: String,
    /// Every step's full output, for the viewer (Ctrl-O).
    outputs: Vec<Entry>,
    viewer: Option<Viewer>,
    bar: String,
    replies: tokio::sync::mpsc::UnboundedSender<Read>,
    interrupted: std::sync::Arc<tokio::sync::Notify>,
}

impl Boxed {
    fn draw(&mut self) {
        let (lines, cursor) = self.editor.layout(self.pinned.text_width());
        let placeholder = match self.waiting {
            Waiting::Message => "type a message · / for commands · \\ or ctrl-j for a new line",
            Waiting::Answer => self.question.as_str(),
            Waiting::Nothing => "type ahead while rein works · ctrl-c stops it · ctrl-o shows output",
        };
        // While a command's name is being typed, the bar shows the
        // commands it could be.
        let typed = self.editor.text();
        let hints = if self.waiting == Waiting::Message { commands::matching(&typed) } else { Vec::new() };
        let bar = if hints.is_empty() {
            self.bar.clone()
        } else {
            let s = Style::stderr();
            let (_, cols) = screen::size();
            let tab = if commands::completion(&typed).is_some() { s.dim("  · tab completes") } else { String::new() };
            // With what each does, if that fits; otherwise just the names.
            let long: Vec<String> = hints.iter().map(|(name, _, what)| format!("{} {}", s.bold(name), s.dim(what))).collect();
            let long = format!(" {}{tab}", long.join(&s.dim("  ·  ")));
            if ui::style::width(&long) < cols {
                long
            } else {
                let names: Vec<String> = hints.iter().map(|(name, _, _)| s.bold(name)).collect();
                format!(" {}{tab}", names.join("  "))
            }
        };
        let bottom = Bottom { focused: self.waiting != Waiting::Nothing, lines, cursor, placeholder: placeholder.into(), bar };
        self.pinned.draw(&bottom);
    }

    fn key(&mut self, key: Key) {
        if let Some(viewer) = &mut self.viewer {
            let (rows, cols) = screen::size();
            if viewer.key(&key) {
                let shown = viewer.render(rows, cols, Style::stderr());
                self.pinned.show_viewer(&shown);
            } else {
                self.viewer = None;
                self.pinned.close_viewer();
                self.draw();
            }
            return;
        }
        if key == Key::CtrlO && !self.outputs.is_empty() {
            let mut viewer = Viewer::new(self.outputs.clone());
            let (rows, cols) = screen::size();
            let shown = viewer.render(rows, cols, Style::stderr());
            self.pinned.open_viewer(&shown);
            self.viewer = Some(viewer);
            return;
        }
        match (self.waiting, &key) {
            (Waiting::Nothing, Key::CtrlC | Key::Esc) => self.interrupted.notify_one(),
            // Ctrl-C clears what you typed; on an empty box it's for leaving.
            (Waiting::Message, Key::CtrlC) if !self.editor.is_empty() => {
                self.editor.take();
            }
            (_, Key::CtrlC) => self.reply(Read::Interrupted),
            (Waiting::Message, Key::CtrlD) if self.editor.is_empty() => self.reply(Read::Eof),
            // What you type ahead is sent once rein is done and you press Enter.
            (Waiting::Nothing, Key::Enter) => {}
            // Tab completes a command's name, when only one fits.
            (Waiting::Message, Key::Tab) => {
                if let Some((name, args, _)) = commands::completion(&self.editor.text()) {
                    self.editor.replace(&format!("{name}{}", if args.is_empty() { "" } else { " " }));
                }
            }
            _ => {
                if self.editor.key(&key) == Action::Submit {
                    let text = self.editor.take();
                    if self.waiting == Waiting::Message {
                        self.editor.remember(&text);
                    }
                    self.reply(Read::Line(text));
                }
            }
        }
        self.draw();
    }

    fn reply(&mut self, read: Read) {
        self.waiting = Waiting::Nothing;
        let _ = self.replies.send(read);
    }
}

impl Input {
    /// `pinned` asks for the input box, which needs a terminal all round.
    fn start(pinned: bool, interrupted: std::sync::Arc<tokio::sync::Notify>) -> Self {
        let (requests, asked) = std::sync::mpsc::channel::<String>();
        let (tx, replies) = tokio::sync::mpsc::unbounded_channel();
        let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
        if pinned && terminal && let Some(raw) = Raw::enter() {
            screen::restore_on_panic();
            let boxed = std::sync::Arc::new(std::sync::Mutex::new(Boxed {
                pinned: Pinned::start(Style::stderr()),
                editor: Editor::default(),
                waiting: Waiting::Nothing,
                question: String::new(),
                outputs: Vec::new(),
                viewer: None,
                bar: String::new(),
                replies: tx,
                interrupted,
            }));
            let keyboard = boxed.clone();
            std::thread::spawn(move || {
                let mut keys = Keys::default();
                let mut buffer = [0u8; 1024];
                let mut stdin = std::io::stdin();
                while let Ok(n @ 1..) = std::io::Read::read(&mut stdin, &mut buffer) {
                    for key in keys.feed(&buffer[..n]) {
                        keyboard.lock().unwrap_or_else(|p| p.into_inner()).key(key);
                    }
                }
            });
            return Input { requests, replies, pending: false, boxed: Some(boxed), _raw: Some(raw) };
        }
        std::thread::spawn(move || {
            for prompt in asked {
                eprint!("{prompt}");
                let mut line = String::new();
                let read = match std::io::stdin().read_line(&mut line) {
                    Ok(0) | Err(_) => Read::Eof,
                    Ok(_) => Read::Line(line.trim_end_matches(['\n', '\r']).to_owned()),
                };
                if tx.send(read).is_err() {
                    break;
                }
            }
        });
        Input { requests, replies, pending: false, boxed: None, _raw: None }
    }

    fn pinned(&self) -> bool {
        self.boxed.is_some()
    }

    /// Reads one line: a message (`message`, kept for ↑) or an answer to
    /// a question. Plain lines show `prompt` first.
    async fn read(&mut self, prompt: &str, message: bool) -> Read {
        if !self.pending {
            match &self.boxed {
                Some(boxed) => {
                    let mut boxed = boxed.lock().unwrap_or_else(|p| p.into_inner());
                    boxed.waiting = if message { Waiting::Message } else { Waiting::Answer };
                    boxed.question = prompt.trim().to_owned();
                    boxed.draw();
                }
                None => {
                    if self.requests.send(prompt.to_owned()).is_err() {
                        return Read::Eof;
                    }
                }
            }
            self.pending = true;
        }
        let read = self.replies.recv().await.unwrap_or(Read::Eof);
        self.pending = false;
        read
    }

    /// Keeps a step's full output for the viewer.
    fn keep_output(&self, title: String, output: &str) {
        if let Some(boxed) = &self.boxed {
            boxed.lock().unwrap_or_else(|p| p.into_inner()).outputs.push(Entry::new(title, output));
        }
    }

    /// Shows `bar` under the box.
    fn set_bar(&self, bar: String) {
        if let Some(boxed) = &self.boxed {
            let mut boxed = boxed.lock().unwrap_or_else(|p| p.into_inner());
            boxed.bar = bar;
            boxed.draw();
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        if let Some(boxed) = &self.boxed {
            boxed.lock().unwrap_or_else(|p| p.into_inner()).pinned.stop();
        }
    }
}

/// The terminal as rein's [`Ui`]: answers on stdout, steps, questions and
/// the status bar on stderr. Each step sits under a label in a column:
///
/// ```text
/// you    fix the trailing comma bug
///
/// read   src/parser.rs  lines 190-240
/// run    cargo test  ✗ exit 101
///        └ test parser::trailing_comma ... FAILED
/// rein   Found it: …
/// ```
///
/// When stdout or stderr isn't a terminal, that stream gets plain text.
struct Terminal {
    /// Colours for stdout and stderr.
    out: Style,
    err: Style,
    /// stderr is a terminal: spinners, labels and the status bar.
    fancy: bool,
    /// The project folder, for line numbers in file changes.
    root: PathBuf,
    model: String,
    mode: &'static str,
    input: Input,
    /// Ctrl-C pressed at a question; the turn waits on this to stop.
    interrupted: std::sync::Arc<tokio::sync::Notify>,
    /// When the conversation gets compacted, in tokens.
    context_limit: u64,
    /// The answer being written, if one is.
    answer: Option<Answer>,
    mid_line: bool,
    spinner: Option<Spinner>,
    /// The step on screen, between its question and its result.
    step: Option<Step>,
    /// A step was shown last, so an answer starts after a blank line.
    after_step: bool,
    /// rein's label has been shown this turn; what follows lines up under it.
    labeled: bool,
    /// The model is writing a summary, not an answer.
    compacting: bool,
    turn: TurnStats,
}

struct Answer {
    markdown: Option<Markdown>,
    wrap: Option<Wrap>,
}

struct Step {
    call_id: String,
    /// What starts its line: rein's label, or room for it.
    lead: String,
    /// Its line is on screen already, from the question.
    header_shown: bool,
    /// Its result is on screen already (it was blocked).
    done: bool,
}

/// What one turn cost, for the status bar.
#[derive(Default)]
struct TurnStats {
    input: u64,
    cached: u64,
    last_input: u64,
    started: Option<std::time::Instant>,
}

impl Terminal {
    fn new(root: PathBuf, mode: Mode, pinned: bool) -> Terminal {
        let mode = mode_name(mode);
        let interrupted: std::sync::Arc<tokio::sync::Notify> = Default::default();
        Terminal {
            out: Style::stdout(),
            err: Style::stderr(),
            fancy: std::io::stderr().is_terminal(),
            root,
            model: String::new(),
            mode,
            input: Input::start(pinned, interrupted.clone()),
            interrupted,
            context_limit: compact::DEFAULT_LIMIT,
            answer: None,
            mid_line: false,
            spinner: None,
            step: None,
            after_step: false,
            labeled: false,
            compacting: false,
            turn: TurnStats::default(),
        }
    }

    /// Stops the spinner, if one is going, which also erases its line.
    fn quiet(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop();
        }
    }

    /// Finishes whatever answer is on screen, so something else can follow.
    fn end_line(&mut self) {
        self.quiet();
        if let Some(mut answer) = self.answer.take() {
            let mut rest = answer.markdown.as_mut().map(Markdown::finish).unwrap_or_default();
            if let Some(wrap) = &mut answer.wrap {
                rest = wrap.push(&rest) + &wrap.finish();
            }
            out!("{rest}");
            if !rest.is_empty() {
                self.mid_line = !rest.ends_with('\n');
            }
        }
        if self.mid_line {
            out!("\n");
            self.mid_line = false;
        }
        std::io::stdout().flush().ok();
    }

    /// A new turn: your message under its label, then a blank line.
    fn start_turn(&mut self, echo: Option<&str>) {
        self.turn = TurnStats { started: Some(std::time::Instant::now()), ..TurnStats::default() };
        self.after_step = false;
        self.labeled = false;
        if self.fancy {
            if let Some(message) = echo {
                let mut wrap = Wrap::new(terminal_width().unwrap_or(80), GUTTER, GUTTER);
                let text = wrap.push(message) + &wrap.finish();
                say!("\n{}{text}", self.err.label("you"));
            }
            say!();
        }
    }

    /// The end of a turn: the status bar.
    ///
    /// ```text
    /// ────────────────────────────────────────────
    ///  gpt-6.1-sol · ask · context ▰▱▱▱▱▱▱▱▱▱ 2% · cache 97% · 6.4s
    /// ```
    fn end_turn(&mut self) {
        self.end_line();
        if !self.fancy {
            return;
        }
        let bar = self.bar();
        if self.input.pinned() {
            self.input.set_bar(bar);
            return;
        }
        let columns = terminal_width().unwrap_or(80);
        say!();
        say!("{}", self.err.dim(&"─".repeat(columns.min(100))));
        say!("{bar}");
    }

    /// The model, the mode, how full the context is, how much came from
    /// the cache, and how long the turn took.
    fn bar(&self) -> String {
        let s = self.err;
        let pct = (self.turn.last_input * 100 / self.context_limit.max(1)).min(100);
        let cells = if self.turn.last_input == 0 { 0 } else { pct.div_ceil(10).clamp(1, 10) as usize };
        let gauge = s.blue(&"▰".repeat(cells)) + &s.dim(&"▱".repeat(10 - cells));
        let cache = (self.turn.cached * 100).checked_div(self.turn.input).unwrap_or(0);
        let seconds = self.turn.started.map_or(0.0, |t| t.elapsed().as_secs_f32());
        let dot = s.dim("  ·  ");
        format!(
            " {}{dot}{}{dot}{} {gauge} {pct}%{dot}{} {cache}%{dot}{}",
            s.bold(&self.model),
            self.mode,
            s.dim("context"),
            s.dim("cache"),
            s.dim(&format!("{seconds:.1}s")),
        )
    }

    /// What starts rein's next line: its label the first time in a turn,
    /// then room for it, so everything rein does lines up under one name.
    fn lead(&mut self) -> String {
        if std::mem::replace(&mut self.labeled, true) { " ".repeat(GUTTER) } else { self.err.label("rein") }
    }

    /// The same, without using up the label: for a spinner, whose line
    /// goes away again.
    fn peek_lead(&self) -> String {
        if self.labeled { " ".repeat(GUTTER) } else { self.err.label("rein") }
    }

    fn columns(&self) -> usize {
        terminal_width().unwrap_or(80)
    }

    /// Prints a step's line before its question, with the change it would
    /// make, unless it's on screen already.
    fn show_header(&mut self, call: &ToolCall) {
        let shown = self.step.as_ref().is_some_and(|step| step.call_id == call.call_id && step.header_shown);
        if !shown {
            let lead = self.lead();
            say!("{}", steps::line(call, &lead, &self.err.dim("○"), "", self.err, self.columns()));
            if matches!(call.name.as_str(), "edit" | "write") {
                for line in steps::diff_box(call, &self.root, self.err, self.columns(), false) {
                    say!("{line}");
                }
            }
            self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: true, done: false });
        }
    }
}

impl Ui for Terminal {
    fn text(&mut self, delta: &str) {
        self.quiet();
        if self.answer.is_none() {
            let width = terminal_width();
            if width.is_some() {
                if self.after_step {
                    out!("\n");
                    self.after_step = false;
                }
                // A summary has no label: it isn't an answer to you.
                let lead = if self.compacting { " ".repeat(GUTTER) } else { self.lead() };
                // The label came from stderr's style; stdout may differ.
                let lead = if self.out.on() { lead } else { strip(&lead) };
                out!("{lead}");
            }
            self.answer = Some(Answer {
                markdown: width.map(|_| Markdown::new(self.out.on())),
                wrap: width.map(|width| Wrap::new(width, GUTTER, GUTTER)),
            });
        }
        let answer = self.answer.as_mut().expect("set above");
        let mut text = match &mut answer.markdown {
            Some(markdown) => markdown.push(delta),
            None => delta.to_owned(),
        };
        if let Some(wrap) = &mut answer.wrap {
            text = wrap.push(&text);
        }
        out!("{text}");
        if !text.is_empty() {
            self.mid_line = !text.ends_with('\n');
        }
    }

    fn thinking(&mut self) {
        self.end_line();
        if self.fancy {
            let s = self.err;
            let lead = self.peek_lead();
            self.spinner = Some(Spinner::start(move |frame, seconds| {
                format!("{lead}{} Thinking… {}", s.yellow(&frame.to_string()), s.dim(&format!("({seconds}s · ctrl-c to stop)")))
            }));
        }
    }

    async fn approve(&mut self, call: &ToolCall) -> Approval {
        self.end_line();
        self.step = None;
        self.show_header(call);
        let question = match call.name.as_str() {
            "bash" => "Allow this command?",
            "edit" => "Allow this edit?",
            "write" => "Allow writing this file?",
            _ => "Allow this?",
        };
        let prompt = format!("{}{question}  [y]es  [n]o  [a]lways › ", " ".repeat(steps::INDENT));
        match self.input.read(&prompt, false).await {
            Read::Line(answer) => {
                // The box keeps nothing; the conversation keeps the answer.
                if self.input.pinned() {
                    say!("{prompt}{}", self.err.bold(answer.trim()));
                }
                approval(&answer)
            }
            // Ctrl-C at a question stops the whole turn.
            Read::Interrupted => {
                self.interrupted.notify_one();
                std::future::pending().await
            }
            // No answer (input has ended) means no.
            Read::Eof => Approval::No,
        }
    }

    fn tool_started(&mut self, call: &ToolCall) {
        self.end_line();
        let asked = self.step.as_ref().is_some_and(|step| step.call_id == call.call_id);
        if !asked {
            // Not asked about: the step's own line spins, then says how it went.
            let lead = self.lead();
            self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: false, done: false });
        }
        if self.fancy {
            let s = self.err;
            let step = self.step.as_ref().expect("set above");
            // A step already on screen spins under itself; otherwise its
            // own line spins, and turns into its result when it's done.
            let (call, lead, own_line) = (call.clone(), step.lead.clone(), !step.header_shown);
            self.spinner = Some(Spinner::start(move |frame, seconds| {
                let (_, columns) = ui::screen::size();
                let time = s.dim(&format!("{seconds}s"));
                let line = if own_line {
                    steps::line(&call, &lead, &s.yellow(&frame.to_string()), &time, s, columns)
                } else {
                    format!("{}{} {}", " ".repeat(steps::INDENT), s.yellow(&frame.to_string()), s.dim(&format!("running · {seconds}s")))
                };
                ui::style::fit(&line, columns.saturating_sub(1))
            }));
        }
    }

    fn tool_blocked(&mut self, call: &ToolCall, reason: &str) {
        self.end_line();
        self.step = None;
        let lead = self.lead();
        let s = self.err;
        say!("{}", steps::line(call, &lead, &s.red("●"), &s.red(&format!("blocked: {reason}")), s, self.columns()));
        self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: true, done: true });
        self.after_step = true;
    }

    fn tool_finished(&mut self, call: &ToolCall, output: &str) {
        self.quiet();
        let step = self.step.take().filter(|step| step.call_id == call.call_id);
        if step.as_ref().is_some_and(|step| step.done) {
            return;
        }
        let s = self.err;
        let asked = step.as_ref().is_some_and(|step| step.header_shown);
        let outcome = steps::outcome(call, output, s);
        if self.input.pinned() {
            let (name, target) = steps::describe(call);
            self.input.keep_output(format!("{name}  {target} · {}", strip(&outcome.status)), output);
        }
        let dot = match outcome.ok {
            Some(true) => s.green("●"),
            Some(false) => s.red("●"),
            None => s.dim("●"),
        };
        match step {
            // Asked about first: its line is up there; the result goes under it.
            Some(step) if step.header_shown => say!("{}{dot} {}", " ".repeat(steps::INDENT), outcome.status),
            step => {
                let lead = step.map_or_else(|| self.lead(), |step| step.lead);
                say!("{}", steps::line(call, &lead, &dot, &outcome.status, s, self.columns()));
            }
        }
        for line in steps::detail_lines(&outcome, s, self.columns(), self.input.pinned()) {
            say!("{line}");
        }
        // An edit made without asking shows what it changed; a question
        // showed it already. A new file's whole text is Ctrl-O away.
        if !asked && call.name == "edit" && outcome.ok == Some(true) {
            for line in steps::diff_box(call, &self.root, s, self.columns(), true) {
                say!("{line}");
            }
        }
        self.after_step = true;
    }

    fn usage(&mut self, usage: &Usage) {
        self.turn.input += usage.input_tokens;
        self.turn.cached += usage.cached_input_tokens;
        self.turn.last_input = usage.input_tokens;
        // The box's bar follows along during the turn.
        if self.input.pinned() {
            self.input.set_bar(self.bar());
        }
    }

    fn compacting(&mut self) {
        self.end_line();
        let lead = self.lead();
        say!("{lead}{}", self.err.dim("compacting the conversation…"));
        self.compacting = true;
    }

    fn compacted(&mut self, report: &Compacted) {
        self.end_line();
        self.compacting = false;
        let retired = match report.retired {
            0 => String::new(),
            1 => ", 1 stale tool output retired".to_owned(),
            n => format!(", {n} stale tool outputs retired"),
        };
        let how = if report.summarized { "summarized" } else { "no summary needed" };
        let line = format!("compacted: about {} → {} tokens ({how}{retired})", thousands(report.before), thousands(report.after));
        say!("{}{}", " ".repeat(GUTTER), self.err.dim(&line));
        self.after_step = true;
    }
}

/// `12345` → `12k`: token counts read at a glance.
fn thousands(n: u64) -> String {
    if n < 1000 { n.to_string() } else { format!("{}k", (n + 500) / 1000) }
}

/// Reads a typed answer: y or a; anything else, including nothing, is no.
fn approval(answer: &str) -> Approval {
    match answer.trim() {
        "y" | "Y" | "yes" => Approval::Yes,
        "a" | "A" | "always" => Approval::Always,
        _ => Approval::No,
    }
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
        say!("Using ChatGPT plan (manage usage: {USAGE_URL})");
        return Ok(Client::new(http.clone(), credentials.access_token));
    }
    match std::env::var("OPENAI_API_KEY") {
        Ok(key) if !key.is_empty() => {
            say!("Using OPENAI_API_KEY (billed to your API account)");
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
    fn reads_answers_and_modes() {
        assert_eq!(approval("y\n"), Approval::Yes);
        assert_eq!(approval("a\n"), Approval::Always);
        assert_eq!(approval("\n"), Approval::No);
        assert_eq!(approval("yolo"), Approval::No);
        let parsed = args("--mode edits fix the bug").unwrap();
        assert_eq!((parsed.mode, parsed.prompt.as_deref()), (Mode::Edits, Some("fix the bug")));
        assert_eq!(args("--mode edits").unwrap().prompt, None, "no task: a conversation");
        assert_eq!(args("--mode yolo hi").err().as_deref(), Some("unknown mode yolo; use ask, edits, read-only or trust"));
    }

    #[test]
    fn masks_emails() {
        assert_eq!(mask_email("someone@example.com"), "so•••@example.com");
        assert_eq!(mask_email("a@example.com"), "a•••@example.com");
        assert_eq!(mask_email("not-an-email"), "•••");
    }

    fn args(line: &str) -> CliResult<AskArgs> {
        AskArgs::parse(line.split(' ').map(str::to_owned).collect(), &Settings::default())
    }

    #[test]
    fn flags_win_over_the_settings() {
        let settings = Settings::parse("mode = edits\nmodel = from-file\neffort = low").unwrap();
        let parsed = AskArgs::parse(vec!["hi".into()], &settings).unwrap();
        assert_eq!((parsed.mode, parsed.effort.as_deref()), (Mode::Edits, Some("low")));
        let parsed = AskArgs::parse(["--mode", "trust", "-m", "flagged", "hi"].map(str::to_owned).to_vec(), &settings).unwrap();
        assert_eq!((parsed.mode, parsed.model.as_str()), (Mode::Trust, "flagged"));
    }

    #[test]
    fn parses_a_system_prompt() {
        let parsed = args("-s Be-brief what is a harness").unwrap();
        assert_eq!(parsed.system.as_deref(), Some("Be-brief"));
        assert_eq!(parsed.prompt.as_deref(), Some("what is a harness"));

        assert_eq!(args("what is a harness").unwrap().system, None);
        assert_eq!(args("--system").err().as_deref(), Some("--system needs a value"));
    }
}
