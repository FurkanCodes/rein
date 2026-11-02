//! `rein`: a tiny, fast agent harness. You hold the reins.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rein_llm::openai::signin::{self, Credentials};
use rein_agent::agent::{Agent, Approval, Compacted, Llm, MAX_TURNS, Outcome, Ui, Window};
use rein_agent::compact;
use rein_agent::permissions::{Mode, Permissions};
use rein_agent::session::{self, Header, Loaded, Session};
use rein_agent::tools::Tools;
use rein_agent::{context, prompt};
use rein_llm::Item;
use rein_llm::provider::{self, Provider};
use rein_llm::{ModelInfo, ToolCall, Usage, store};

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
mod connect;
mod input;
mod settings;
use settings::Settings;
mod ui;
use commands::Slash;
use input::{Input, Question, Read};
use ui::history::{self, History};
use ui::markdown::Markdown;
use ui::spinner::Spinner;
use ui::steps;
use ui::style::{GUTTER, Style, strip};
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
  rein login <provider>          Save an API key for a provider, like anthropic
  rein logout <provider>         Forget it (rein logout chatgpt signs out)
  rein models [<provider>]       List the models you can use

rein can read, write and edit files in this folder and run commands. In a
conversation, type /exit or press Ctrl-D to leave. Conversations are saved in
~/.config/rein/sessions, and what you send from each folder, for ↑ and
Ctrl-R, in ~/.config/rein/history, both readable only by you.

rein rings the terminal's bell when it needs you and you're away: it asks
something, or it's done. Away means its window isn't in front, or, where the
terminal doesn't say, that you haven't touched a key for 10 seconds.

A long conversation is compacted when it nears $REIN_COMPACT_AT tokens
(default 150000): stale tool output goes first, then the model summarizes.
Type /compact [focus] to do it now. Nothing is lost: the model can search
the whole conversation with its recall tool.

rein tells the model about the project with its AGENTS.md files, from the
repository's root down to this folder (CLAUDE.md where a folder has no
AGENTS.md), plus your own ~/.config/rein/AGENTS.md. 32 KB at most.

Options:
  -m, --model <id>      Model to use, like claude-opus-5-5 or anthropic/claude-sonnet-5-5
                        (default: $REIN_MODEL, or gpt-6.1-sol; claude-opus-5-5 when
                        Anthropic is the only provider you have a key for)
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

Defaults go in ~/.config/rein/config, one per line: model, effort, mode,
compact_at, bell and compress (on or off), like `mode = edits`. Flags win
over $REIN_MODEL, $REIN_EFFORT, $REIN_MODE, $REIN_COMPACT_AT, $REIN_BELL and
$REIN_COMPRESS, which win over the file.

Each request carries the whole conversation, so rein gzips it on the way:
about three times fewer bytes, and the same text for the model. A server that
doesn't take compressed requests gets them uncompressed instead.

A model's name says who serves it: claude-* is Anthropic's, gpt-* OpenAI's.
Or name the provider first, like anthropic/claude-opus-5-5; /model switches
mid-conversation, even to another provider. Keys come from $OPENAI_API_KEY and
$ANTHROPIC_API_KEY ($ANTHROPIC_AUTH_TOKEN for a gateway), or from
`rein login <provider>`, which keeps them in ~/.config/rein/keys.json, readable
only by you. $OPENAI_BASE_URL and $ANTHROPIC_BASE_URL point rein at another
server that speaks the same API. Signed in with ChatGPT, OpenAI's models use
your plan; otherwise $OPENAI_API_KEY, billed to your API account.";

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
        ["login"] | ["login", "chatgpt"] => login(&http).await,
        ["login", provider] => connect::login(&http, provider).await,
        ["logout", provider] => connect::logout(provider),
        ["models"] => connect::models(&http, None).await,
        ["models", provider] => connect::models(&http, Some(provider)).await,
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
    /// From `-m`, the environment or the settings; `None` lets rein choose.
    model: Option<String>,
    /// `-m` was given, so a continued conversation switches to `model`.
    model_given: bool,
    resume: Resume,
    effort: Option<String>,
    /// The system prompt: standing instructions sent with every request.
    system: Option<String>,
    mode: Mode,
    record: Option<PathBuf>,
    /// Ring the bell when rein needs you and you're away.
    bell: bool,
    /// Gzip request bodies.
    compress: bool,
    /// `None` starts a conversation instead of doing one task.
    prompt: Option<String>,
}

impl AskArgs {
    /// Flags first; what they don't say comes from `settings` (and the
    /// environment), then rein's defaults.
    fn parse(args: Vec<String>, settings: &Settings) -> CliResult<Self> {
        let mut model = settings.get("model", "REIN_MODEL");
        let (mut effort, mut system, mut record, mut words) = (settings.get("effort", "REIN_EFFORT"), None, None, Vec::new());
        let mut mode = match settings.get("mode", "REIN_MODE") {
            Some(name) => Mode::parse(&name).ok_or(format!("unknown mode {name} in the settings; use ask, edits, read-only or trust"))?,
            None => Mode::Ask,
        };
        let switch = |key: &str, env: &str| match settings.get(key, env).as_deref() {
            None | Some("on") => Ok(true),
            Some("off") => Ok(false),
            Some(other) => Err(format!("{key} should be on or off, not {other}")),
        };
        let (bell, compress) = (switch("bell", "REIN_BELL")?, switch("compress", "REIN_COMPRESS")?);
        let (mut model_given, mut resume) = (false, Resume::New);
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-m" | "--model" => {
                    model = Some(value(&mut args, &arg)?);
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
        Ok(AskArgs { model, model_given, resume, effort, system, mode, record, bell, compress, prompt })
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> CliResult<String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

async fn ask(http: &rein_llm::reqwest::Client, args: AskArgs, compact_at: Option<String>) -> CliResult {
    let dir = config_dir()?;
    let keys = connect::load_keys(&dir)?;
    let providers = provider::builtin();
    let root = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
    let folder = session::folder(&dir.join("sessions"), &root);
    let latest = match args.resume {
        Resume::Latest => match session::list(&folder, 1).into_iter().next() {
            Some(latest) => Some(latest.path),
            None => return Err("no saved conversation in this folder yet".into()),
        },
        _ => None,
    };
    let wanted = args.model.clone().unwrap_or_else(|| connect::default_model(&providers, &keys));
    // Whoever serves the model, as soon as that's known: the model list,
    // which says how big each model's context window is, loads while
    // everything else gets ready.
    let early = if args.model_given || args.resume == Resume::New {
        Some(wanted.clone())
    } else {
        latest.as_deref().and_then(session::peek).map(|header| header.model)
    };
    let mut connected = match &early {
        Some(spec) => Some(connect::connect(http, &providers, &keys, spec, args.compress).await?),
        None => None,
    };
    let record = match &args.record {
        Some(path) => Some(File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?),
        None => None,
    };
    let permissions = load_permissions(&root, args.mode)?;
    // What you sent from this folder before, for ↑ and Ctrl-R: only a
    // conversation has a box to bring it back in.
    let sent = if args.prompt.is_none() { History::load(history::file(&dir, &root)) } else { History::default() };
    let mut terminal = Terminal::new(root.clone(), args.mode, args.prompt.is_none(), sent, args.bell);
    // The box goes up now, so typing can start while the model list loads.
    terminal.model = connected.as_ref().map(|c| provider::shown(&c.provider, &c.model)).unwrap_or_default();
    terminal.effort = args.effort.clone();
    terminal.input.set_bar(terminal.bar());
    let mut setup = Setup {
        folder,
        personal: dir.join("AGENTS.md"),
        root: root.clone(),
        system: args.system.clone(),
        models: Vec::new(),
        compact_at,
        http: http.clone(),
        provider: providers[0].clone(),
        providers,
        keys,
        compress: args.compress,
    };

    let saved = match args.resume {
        Resume::New => None,
        Resume::Latest => latest,
        Resume::Pick => match pick(&setup.folder, &mut terminal).await? {
            Some(path) => Some(path),
            None => return Ok(()),
        },
    };
    if saved.is_some() && args.system.is_some() {
        return Err("-s can't change a saved conversation's system prompt; start a new one instead".into());
    }
    let wanted = setup.canonical(&wanted)?;
    let (mut loaded, context) = setup.open(saved, &wanted)?;
    // A continued conversation keeps its model unless -m says otherwise,
    // and then it's saved with it.
    let theirs = setup.canonical(&loaded.session.header().model)?;
    let spec = if args.model_given || args.resume == Resume::New { wanted } else { theirs.clone() };
    if args.resume != Resume::New && args.model_given && theirs != spec {
        loaded.session.append_model(&spec).map_err(describe)?;
    }
    let (provider, model_name) = connect::resolve(&spec, &setup.providers)?;
    let provider = provider.clone();
    // Connected to another provider than the conversation's, or to none yet.
    let connected = match connected.take() {
        Some(connected) if connected.provider.name == provider.name => connected,
        _ => connect::connect(http, &setup.providers, &setup.keys, &spec, args.compress).await?,
    };
    let (client, models) = connected.models().await;
    setup.provider = provider;
    setup.models = models;
    let header = loaded.session.header().clone();
    let limit = setup.limit(&model_name)?;
    terminal.context_limit = limit;
    terminal.model = provider::shown(&setup.provider, &model_name);
    terminal.input.set_bar(terminal.bar());
    let mut agent = Agent {
        model: Llm { client, record },
        tools: Tools::new(root),
        permissions,
        max_output: setup.max_output(&model_name),
        model_name,
        instructions: header.instructions,
        reasoning_effort: args.effort,
        session_id: Some(header.id),
        history: loaded.history,
        session: Some(loaded.session),
        context,
        window: Window { limit, measured: None, full: loaded.full },
    };
    // A continued conversation is as full as it was.
    terminal.context_used = agent.estimated();
    terminal.input.set_bar(terminal.bar());
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
    /// The models the provider in use offers, with their context windows.
    models: Vec<ModelInfo>,
    /// `compact_at` from the settings or the environment.
    compact_at: Option<String>,
    /// For connecting to another provider, when `/model` names one.
    http: rein_llm::reqwest::Client,
    /// The provider in use.
    provider: Provider,
    providers: Vec<Provider>,
    keys: connect::Keys,
    /// Gzip request bodies.
    compress: bool,
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
            id: rein_llm::new_session_id().map_err(describe)?,
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

    /// The most one answer from `model` may take, when its provider says.
    fn max_output(&self, model: &str) -> Option<u64> {
        self.models.iter().find(|m| m.id == model).and_then(|m| m.max_output)
    }

    /// `spec` as saved: `provider/model`.
    fn canonical(&self, spec: &str) -> CliResult<String> {
        let (provider, model) = connect::resolve(spec, &self.providers)?;
        Ok(provider::spec(provider, &model))
    }

    /// The model in use, as saved.
    fn current(&self, agent: &Agent<Llm>) -> String {
        provider::spec(&self.provider, &agent.model_name)
    }
}

/// One message from you, worked on until the model is done, or until
/// Ctrl-C. Interrupting drops the turn mid-flight: the request is closed,
/// a running command is killed, and the history is repaired so the
/// conversation can go on.
async fn turn(agent: &mut Agent<Llm>, prompt: String, terminal: &mut Terminal) -> CliResult {
    let interrupted = terminal.interrupted.clone();
    let outcome = tokio::select! {
        outcome = agent.send(prompt, terminal) => Some(outcome),
        _ = tokio::signal::ctrl_c() => None,
        _ = interrupted.notified() => None,
    };
    terminal.end_line();
    if std::mem::take(&mut terminal.after_you) && terminal.fancy {
        say!();
    }
    let Some(outcome) = outcome else {
        agent.repair();
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(interrupted; what should rein do instead?)"));
        terminal.end_turn();
        return Ok(());
    };
    terminal.end_turn();
    // Said once: rein's requests to this server go uncompressed.
    let refused = agent.model.client.traffic().compression == rein_llm::Compression::Refused;
    if refused && !std::mem::replace(&mut terminal.said_uncompressed, true) {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(this server doesn't take compressed requests, so they go uncompressed)"));
    }
    match outcome.map_err(describe)? {
        Outcome::Done => Ok(()),
        Outcome::Stopped => {
            say!("{}{}", " ".repeat(GUTTER), terminal.err.dim("(stopped; what should rein do instead?)"));
            Ok(())
        }
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
async fn compact_now(agent: &mut Agent<Llm>, focus: Option<&str>, terminal: &mut Terminal) -> CliResult {
    terminal.turn.started = Some(std::time::Instant::now());
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
    // Under the gutter, like everything else rein says.
    let pad = " ".repeat(GUTTER);
    for (i, conversation) in saved.iter().enumerate() {
        let when = local_time(conversation.modified);
        let first = conversation.first.as_deref().map(|text| shorten(text, 50)).unwrap_or_default();
        let messages = format!("{} message{}", conversation.messages, if conversation.messages == 1 { "" } else { "s" });
        say!("{pad}{:>2}  {} {:02}:{:02}  {messages:<12}  {first}", i + 1, when.date(), when.hour, when.minute);
    }
    loop {
        match terminal.input.read("Continue which one? (number, or Enter to cancel) ", false).await {
            Read::Line(line) if line.trim().is_empty() => return Ok(None),
            Read::Line(line) => match line.trim().parse::<usize>() {
                Ok(n) if (1..=saved.len()).contains(&n) => return Ok(Some(saved[n - 1].path.clone())),
                _ => say!("{pad}Type a number from 1 to {}.", saved.len()),
            },
            Read::Interrupted | Read::Eof => return Ok(None),
            Read::NextMode => {}
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
async fn conversation(agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
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
        let line = match message(terminal, &mut last_interrupt).await {
            Said::Message(line) => line,
            Said::NextMode => {
                let mode = next_mode(agent.permissions.mode);
                agent.permissions.mode = mode;
                terminal.mode = mode_name(mode);
                terminal.input.set_bar(terminal.bar());
                terminal.input.notice(&format!("{} · shift-tab for the next", mode_description(mode)));
                continue;
            }
            Said::Leave => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let pad = " ".repeat(GUTTER);
        match commands::parse(line) {
            Some(Ok(Slash::Exit)) => break,
            Some(parsed) => {
                if terminal.input.pinned() {
                    say!("\n{}", you_said(line, terminal.err, terminal.wrap_width().unwrap_or(80)));
                }
                let done = match parsed {
                    Ok(command) => run_command(command, agent, terminal, setup).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = done {
                    say!("{pad}{}", terminal.err.red(&format!("✗ {e}")));
                }
                terminal.input.set_bar(terminal.bar());
            }
            None => {
                // A plain terminal already shows what was typed; the box doesn't.
                let echo = terminal.input.pinned().then_some(line);
                terminal.start_turn(echo);
                if let Err(e) = turn(agent, line.to_owned(), terminal).await {
                    say!("{pad}{}", terminal.err.red(&format!("✗ {e}")));
                }
            }
        }
    }
    // Everything was saved as it went: say how to come back to it.
    let talked = agent.window.full.iter().skip(1).any(|item| matches!(item, Item::User(_)));
    if talked && terminal.fancy {
        say!("\n{}", terminal.err.dim("rein -c continues this conversation"));
    }
    Ok(())
}

/// Shift-Tab's order: asking about everything, then about commands only,
/// then nothing running at all. `trust` is only for asking by name.
fn next_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Ask => Mode::Edits,
        Mode::Edits => Mode::ReadOnly,
        Mode::ReadOnly | Mode::Trust => Mode::Ask,
    }
}

/// Does what a slash command asks. What it says goes under the gutter,
/// like a step's result.
async fn run_command(command: Slash, agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult {
    let pad = " ".repeat(GUTTER);
    let s = terminal.err;
    match command {
        Slash::Help => {
            for line in commands::help() {
                say!("{pad}{line}");
            }
            for keys in [
                "shift-enter, ctrl-j or \\ at the end starts a new line · shift-tab changes the mode",
                "↑ brings back what you sent before, from this folder · ctrl-r finds it by its words",
                "@ points at a file or folder: it's read with your message · @file:10-20 for some lines",
                "enter while rein works steers it: your message goes in at its next step",
                "tab while rein works sends it after the turn instead · esc or ctrl-c stops the turn",
                "wheel, pgup/pgdn or shift-↑↓ scroll · drag over text to copy it",
                "ctrl-o shows every step's full output · ctrl-l draws the window again",
            ] {
                say!("{pad}{}", s.dim(keys));
            }
        }
        Slash::Mode(None) => say!("{pad}{}", mode_description(agent.permissions.mode)),
        Slash::Mode(Some(mode)) => {
            agent.permissions.mode = mode;
            terminal.mode = mode_name(mode);
            say!("{pad}{} {}", s.green("✓"), mode_description(mode));
        }
        Slash::Model(None) => {
            if setup.models.is_empty() {
                say!("{pad}{} (the model list couldn't be fetched)", terminal.model);
            }
            for model in &setup.models {
                let current = model.id == agent.model_name;
                let window = model.context_window.map(|w| format!("  {} tokens", thousands(w))).unwrap_or_default();
                let line = format!("{} {}{}", if current { "›" } else { " " }, provider::shown(&setup.provider, &model.id), s.dim(&window));
                say!("{pad}{}", if current { s.bold(&line) } else { line });
            }
            let others: Vec<&str> =
                setup.providers.iter().filter(|p| p.name != setup.provider.name && connect::usable(p, &setup.keys)).map(|p| p.name.as_str()).collect();
            if !others.is_empty() {
                say!("{pad}{}", s.dim(&format!("You also have {}: /model <provider>/<model> switches", others.join(", "))));
            }
        }
        Slash::Model(Some(spec)) => {
            let shown = use_model(&spec, agent, terminal, setup).await?;
            let current = setup.current(agent);
            if let Some(session) = &mut agent.session {
                session.append_model(&current).map_err(describe)?;
            }
            say!("{pad}{} now using {shown}", s.green("✓"));
            say!("{pad}{}", s.dim("The prompt cache starts over: it's kept per model."));
        }
        Slash::Effort(None) => {
            say!("{pad}reasoning effort: {}", agent.reasoning_effort.as_deref().unwrap_or("the model's default"));
        }
        Slash::Effort(Some(level)) => {
            agent.reasoning_effort = level;
            terminal.effort = agent.reasoning_effort.clone();
            say!("{pad}{} reasoning effort: {}", s.green("✓"), agent.reasoning_effort.as_deref().unwrap_or("the model's default"));
        }
        Slash::Status => {
            let header = agent.session.as_ref().map(|session| session.header().clone());
            let messages = agent.window.full.iter().skip(1).filter(|item| matches!(item, Item::User(_))).count();
            let rows = [
                ("model", terminal.model.clone()),
                ("provider", format!("{} · {}", setup.provider.title, agent.model.client.base_url())),
                ("effort", agent.reasoning_effort.clone().unwrap_or_else(|| "the model's default".into())),
                ("mode", mode_description(agent.permissions.mode).to_owned()),
                ("rules", format!("{} from .rein/permissions", agent.permissions.rule_count())),
                ("context", format!("about {} of {} tokens before compacting", thousands(agent.estimated()), thousands(agent.window.limit))),
                ("sent", traffic(agent.model.client.traffic())),
                ("started", header.map(|h| format!("{} · {messages} message{} so far", h.started, if messages == 1 { "" } else { "s" })).unwrap_or_default()),
                ("folder", setup.root.display().to_string()),
            ];
            for (name, value) in rows {
                say!("{pad}{}  {value}", s.dim(&format!("{name:<8}")));
            }
        }
        Slash::Compact(focus) => compact_now(agent, focus.as_deref(), terminal).await?,
        Slash::Copy => {
            let last = agent.window.full.iter().rev().find_map(|item| match item {
                Item::Assistant(text) if !text.trim().is_empty() => Some(text.trim()),
                _ => None,
            });
            let Some(text) = last else { return Err("nothing to copy yet: rein hasn't answered in this conversation".into()) };
            if !input::copy(text) {
                return Err("couldn't copy: no clipboard".into());
            }
            let lines = text.lines().count();
            say!("{pad}{} copied rein's last answer ({lines} line{})", s.green("✓"), if lines == 1 { "" } else { "s" });
        }
        Slash::Undo => {
            let undone = agent.undo().map_err(describe)?;
            if undone.is_empty() {
                say!("{pad}nothing to undo: rein hasn't changed any files in this session");
            } else {
                say!("{pad}{} put back {}", s.green("✓"), undone.join(", "));
                say!("{pad}{}", s.dim("Commands rein ran aren't undone; the model has been told."));
            }
        }
        Slash::New => {
            let (loaded, context) = setup.open(None, &setup.current(agent))?;
            switch(agent, loaded, context);
            (terminal.context_used, terminal.last_cache) = (0, None);
            say!("{pad}{} a fresh conversation; the last one is saved, /resume brings it back", s.green("✓"));
        }
        Slash::Resume => {
            let Some(path) = pick(&setup.folder, terminal).await? else { return Ok(()) };
            let (loaded, context) = setup.open(Some(path), &setup.current(agent))?;
            // Its own model, from whichever provider serves it: connected
            // first, so a missing key leaves everything as it was.
            let model = setup.canonical(&loaded.session.header().model)?;
            if model != setup.current(agent) {
                use_model(&model, agent, terminal, setup).await?;
            }
            switch(agent, loaded, context);
            (terminal.context_used, terminal.last_cache) = (agent.estimated(), None);
        }
        Slash::Exit => {}
    }
    Ok(())
}

/// Switches to the model `spec`, connecting to its provider if that's
/// another one. Returns the model as shown.
async fn use_model(spec: &str, agent: &mut Agent<Llm>, terminal: &mut Terminal, setup: &mut Setup) -> CliResult<String> {
    let (provider, model) = connect::resolve(spec, &setup.providers)?;
    let provider = provider.clone();
    let missing = |models: &[ModelInfo]| !models.is_empty() && !models.iter().any(|m| m.id == model);
    if provider.name != setup.provider.name {
        let (client, models) = connect::connect(&setup.http, &setup.providers, &setup.keys, spec, setup.compress).await?.models().await;
        if missing(&models) {
            return Err(format!("{} has no model called {model}; `rein models {}` lists them", provider.title, provider.name));
        }
        agent.model.client = client;
        (setup.provider, setup.models) = (provider, models);
    } else if missing(&setup.models) {
        return Err(format!("no model called {model}; /model lists them"));
    }
    agent.window.limit = setup.limit(&model)?;
    terminal.context_limit = agent.window.limit;
    agent.max_output = setup.max_output(&model);
    terminal.model = provider::shown(&setup.provider, &model);
    agent.model_name = model;
    Ok(terminal.model.clone())
}

/// Moves the agent to another conversation. The mode, the rules and the
/// model stay; what you said "always" to was for the old one.
fn switch(agent: &mut Agent<Llm>, loaded: Loaded, context: Option<String>) {
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

/// What you did at the prompt.
enum Said {
    Message(String),
    /// Shift-Tab: on to the next mode.
    NextMode,
    /// Ctrl-D, or Ctrl-C twice.
    Leave,
}

/// Reads your next message. A line ending in `\` continues on the next.
/// Ctrl-C clears what you typed; a second one within two seconds, or
/// Ctrl-D, ends the conversation.
async fn message(terminal: &mut Terminal, last_interrupt: &mut Option<std::time::Instant>) -> Said {
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
                    return Said::Message(text);
                }
            },
            Read::Interrupted => {
                if last_interrupt.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    return Said::Leave;
                }
                *last_interrupt = Some(std::time::Instant::now());
                if terminal.input.pinned() {
                    terminal.input.notice("press ctrl-c again to leave");
                } else {
                    say!("{}", terminal.err.dim("(press Ctrl-C again to leave)"));
                }
                text.clear();
            }
            Read::Eof => return Said::Leave,
            Read::NextMode => return Said::NextMode,
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

/// The terminal as rein's [`Ui`]: answers on stdout, steps, questions and
/// the status bar on stderr. Each step sits under a label in a column:
///
/// ```text
/// you    fix the trailing comma bug
///
/// rein   ● Read   src/parser.rs, lines 190-240
///        ● Run    cargo test  exit 101
///          └ test parser::trailing_comma ... FAILED
///
///        Found it: …
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
    effort: Option<String>,
    mode: &'static str,
    input: Input,
    /// Ctrl-C pressed at a question; the turn waits on this to stop.
    interrupted: std::sync::Arc<tokio::sync::Notify>,
    /// When the conversation gets compacted, in tokens.
    context_limit: u64,
    /// How many tokens the conversation took at the last count.
    context_used: u64,
    /// How much of the last finished turn came from the prompt cache, in percent.
    last_cache: Option<u64>,
    /// The answer being written, if one is.
    answer: Option<Answer>,
    mid_line: bool,
    /// Without the box, what rein is doing spins on a line of its own.
    spinner: Option<Spinner>,
    /// The step on screen, between its question and its result.
    step: Option<Step>,
    /// A note to show under the step once its line is up, like that it
    /// won't be asked about again.
    noted: Option<String>,
    /// A step was shown last, so an answer starts after a blank line.
    after_step: bool,
    /// An answer was shown last, so a step starts after a blank line.
    after_text: bool,
    /// Your message was shown last, so whatever comes next starts after a
    /// blank line. It waits until then, so the spinner's own gap isn't
    /// doubled.
    after_you: bool,
    /// rein's label has been shown this turn; what follows lines up under it.
    labeled: bool,
    /// What the model says it was thinking about, as it streams in, and
    /// how many of its headlines are on screen.
    thoughts: String,
    thoughts_shown: usize,
    /// The plan's steps as last shown in full: an update that only moves
    /// along it gets one line.
    plan: Vec<String>,
    /// The model is writing a summary, not an answer.
    compacting: bool,
    /// It's been said that requests go uncompressed.
    said_uncompressed: bool,
    /// The reads on screen as one line, which the next joins if nothing
    /// came between.
    reads: Option<Reads>,
    /// The key the next run of reads is shown under.
    next_key: u64,
    turn: TurnStats,
}

/// A run of reads, shown as one line.
struct Reads {
    key: u64,
    lead: String,
    /// The first read: alone, the run looks like any step.
    first: ToolCall,
    paths: Vec<String>,
    statuses: Vec<String>,
}

impl Reads {
    fn draw(&self, s: Style) -> ui::transcript::Draw {
        let (lead, first, paths, statuses) = (self.lead.clone(), self.first.clone(), self.paths.clone(), self.statuses.clone());
        Box::new(move |width| {
            let dot = s.green("●");
            vec![if paths.len() == 1 { steps::line(&first, &lead, &dot, &statuses[0], s, width) } else { steps::reads_line(&paths, &statuses, &lead, &dot, s, width) }]
        })
    }
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
    started: Option<std::time::Instant>,
    /// How long it took, once it's over.
    took: Option<Duration>,
}

/// The most lines of your message the conversation shows again; the rest
/// is counted.
const ECHO_LINES: usize = 12;

impl Terminal {
    fn new(root: PathBuf, mode: Mode, pinned: bool, history: History, bell: bool) -> Terminal {
        let mode = mode_name(mode);
        let interrupted: std::sync::Arc<tokio::sync::Notify> = Default::default();
        let input = Input::start(pinned, interrupted.clone(), history, bell, root.clone());
        Terminal {
            out: Style::stdout(),
            err: Style::stderr(),
            fancy: std::io::stderr().is_terminal(),
            root,
            model: String::new(),
            effort: None,
            mode,
            input,
            interrupted,
            context_limit: compact::DEFAULT_LIMIT,
            context_used: 0,
            last_cache: None,
            answer: None,
            mid_line: false,
            spinner: None,
            step: None,
            noted: None,
            after_step: false,
            after_text: false,
            after_you: false,
            labeled: false,
            thoughts: String::new(),
            thoughts_shown: 0,
            plan: Vec::new(),
            compacting: false,
            said_uncompressed: false,
            reads: None,
            next_key: 0,
            turn: TurnStats::default(),
        }
    }

    /// Stops the spinner, if one is going, which also erases its line.
    fn quiet(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop();
        }
    }

    /// Says what rein is doing: over the box, with the turn's time, or
    /// without one, on a spinning line of its own until something prints.
    fn doing(&mut self, what: &str) {
        if self.input.pinned() {
            self.input.busy(Some(what.to_owned()), self.turn.started.unwrap_or_else(std::time::Instant::now));
            return;
        }
        if !self.fancy {
            return;
        }
        self.quiet();
        let s = self.err;
        let lead = self.peek_lead();
        let what = what.to_owned();
        self.spinner = Some(Spinner::start(move |frame, seconds| {
            let line = format!("{lead}{} {what}… {}", s.yellow(&frame.to_string()), s.dim(&format!("({seconds}s · ctrl-c to stop)")));
            ui::style::fit(&line, ui::screen::size().1.saturating_sub(1))
        }));
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
            self.after_text = true;
        }
        if self.mid_line {
            out!("\n");
            self.mid_line = false;
        }
        std::io::stdout().flush().ok();
    }

    /// A blank line before a step that follows an answer or your message.
    fn gap_before_step(&mut self) {
        let after_you = std::mem::take(&mut self.after_you);
        if (std::mem::take(&mut self.after_text) || after_you) && self.fancy {
            say!();
        }
    }

    /// A new turn: your message under its label, then a blank line.
    fn start_turn(&mut self, echo: Option<&str>) {
        self.turn = TurnStats { started: Some(std::time::Instant::now()), ..TurnStats::default() };
        self.after_step = false;
        self.after_text = false;
        self.after_you = true;
        self.labeled = false;
        if self.fancy
            && let Some(message) = echo
        {
            let shown = self.input.shown(message);
            say!("\n{}", you_said(&shown, self.err, self.wrap_width().unwrap_or(80)));
        }
        // Without the box, the spinner's line comes straight after this one.
        if self.fancy && !self.input.pinned() {
            say!();
            self.after_you = false;
        }
        // The last turn's time goes; the spinner counts this one.
        self.input.set_bar(self.bar());
    }

    /// The end of a turn: the status bar.
    ///
    /// ```text
    /// ────────────────────────────────────────────
    ///  gpt-6.1-sol · ask · context ▰▱▱▱▱▱▱▱▱▱ 2% · cache 97% · 6.4s
    /// ```
    fn end_turn(&mut self) {
        self.end_line();
        self.input.busy(None, std::time::Instant::now());
        self.input.look_for_paths();
        self.turn.took = self.turn.started.map(|t| t.elapsed());
        if let Some(cache) = (self.turn.cached * 100).checked_div(self.turn.input) {
            self.last_cache = Some(cache);
        }
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

    /// The model and its effort, the mode, how full the context is, how
    /// much came from the cache, and how long the last turn took.
    fn bar(&self) -> String {
        let s = self.err;
        let used = self.context_used;
        // Never 0% once anything is in there, nor a full cell short of it.
        let pct = if used == 0 { 0 } else { (used * 100).div_ceil(self.context_limit.max(1)).min(100) };
        let cells = pct.div_ceil(10) as usize;
        let gauge = s.blue(&"▰".repeat(cells)) + &s.dim(&"▱".repeat(10 - cells));
        let dot = s.dim("  ·  ");
        let model = match &self.effort {
            Some(effort) => format!("{} {}", s.bold(&self.model), s.dim(effort)),
            None => s.bold(&self.model),
        };
        // The modes that ask less stand out.
        let mode = match self.mode {
            "trust" => s.red(self.mode),
            "edits" => s.yellow(self.mode),
            mode => mode.to_owned(),
        };
        let cache = (self.turn.cached * 100).checked_div(self.turn.input).or(self.last_cache);
        let cache = cache.map(|cache| format!("{dot}{} {cache}%", s.dim("cache"))).unwrap_or_default();
        // The time only once the turn is over; the spinner counts until then.
        let took = self.turn.took.map(|t| format!("{dot}{}", s.dim(&format!("{:.1}s", t.as_secs_f32())))).unwrap_or_default();
        format!(" {model}{dot}{mode}{dot}{} {gauge} {pct}%{cache}{took}", s.dim("context"))
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

    /// How wide text is wrapped as it's printed. On the full screen it
    /// isn't: the screen lays it out itself, for whatever width the window
    /// has, then and after every resize.
    fn wrap_width(&self) -> Option<usize> {
        if self.input.pinned() { Some(UNWRAPPED) } else { terminal_width() }
    }

    /// Shows lines laid out for the window's width by `draw`: a step. On the
    /// full screen they're laid out again whenever the width changes.
    fn show(&self, draw: impl Fn(usize) -> Vec<String> + Send + 'static) {
        ui::screen::show(Box::new(draw));
    }

    /// A read that went well: it joins the reads on screen just before it,
    /// if nothing came between, or starts a run of its own.
    fn show_read(&mut self, call: &ToolCall, status: String, lead: String) {
        let s = self.err;
        let args: rein_llm::serde_json::Value = rein_llm::serde_json::from_str(&call.arguments).unwrap_or_default();
        let path = args["path"].as_str().unwrap_or_default().to_owned();
        if let Some(reads) = &mut self.reads {
            reads.paths.push(path.clone());
            reads.statuses.push(status.clone());
            if ui::screen::redraw(reads.key, reads.draw(s)) {
                return;
            }
        }
        self.next_key += 1;
        let reads = Reads { key: self.next_key, lead, first: call.clone(), paths: vec![path], statuses: vec![status] };
        ui::screen::show_as(reads.key, reads.draw(s));
        self.reads = Some(reads);
    }

    /// Prints a step's line before its question, with the change it would
    /// make, unless it's on screen already.
    fn show_header(&mut self, call: &ToolCall) {
        let shown = self.step.as_ref().is_some_and(|step| step.call_id == call.call_id && step.header_shown);
        if !shown {
            self.gap_before_step();
            let lead = self.lead();
            let s = self.err;
            let diff = if matches!(call.name.as_str(), "edit" | "write") { steps::diff_box(call, &self.root, s, WIDE, false) } else { Vec::new() };
            let (call_, lead_) = (call.clone(), lead.clone());
            self.show(move |width| {
                let mut lines = vec![steps::line(&call_, &lead_, &s.dim("○"), "", s, width)];
                lines.extend(diff.iter().map(|line| ui::style::fit(line, width)));
                lines
            });
            self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: true, done: false });
        }
    }
}

/// A width nothing is wrapped at: text goes to the full screen whole, and
/// is laid out there.
const UNWRAPPED: usize = usize::MAX / 4;

/// The width a file change's frame is made at once, to be cut to the
/// window's width as it's shown: it's worked out from the file as it is
/// then, which may change later.
const WIDE: usize = 400;

/// Your message as the conversation shows it: under your label, wrapped,
/// and only its first lines if it's long, like a big paste.
fn you_said(message: &str, s: Style, columns: usize) -> String {
    let lines: Vec<&str> = message.lines().collect();
    let shown = if lines.len() > ECHO_LINES { &lines[..ECHO_LINES - 1] } else { &lines[..] };
    let mut wrap = Wrap::new(columns, GUTTER, GUTTER);
    let text = wrap.push(&shown.join("\n")) + &wrap.finish();
    let mut out = format!("{}{}", s.you(), s.bold(&text));
    if lines.len() > shown.len() {
        let rest = lines.len() - shown.len();
        out += &format!("\n{}{}", " ".repeat(GUTTER), s.dim(&format!("… {rest} more lines")));
    }
    out
}

impl Ui for Terminal {
    fn text(&mut self, delta: &str) {
        self.quiet();
        if self.answer.is_none() {
            let width = self.wrap_width();
            if width.is_some() {
                if std::mem::take(&mut self.after_step) | std::mem::take(&mut self.after_you) {
                    out!("\n");
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
            if self.input.pinned() && !self.compacting {
                self.doing("Writing");
            }
        }
        let answer = self.answer.as_mut().expect("set above");
        let mut text = match &mut answer.markdown {
            Some(markdown) => markdown.push(delta),
            None => delta.to_owned(),
        };
        let width = if self.input.pinned() { None } else { terminal_width() };
        if let Some(wrap) = &mut answer.wrap {
            if let Some(width) = width {
                wrap.set_width(width);
            }
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
            self.thoughts.clear();
            self.thoughts_shown = 0;
            self.doing("Thinking");
        }
    }

    /// Each headline of the model's reasoning summary, once it's complete,
    /// becomes one dim line: what the model was thinking about. (On this
    /// route they arrive when the thinking is done, not during it.)
    fn thought(&mut self, delta: &str) {
        self.thoughts.push_str(delta);
        let headlines = headlines(&self.thoughts);
        if headlines.len() > self.thoughts_shown && self.fancy {
            self.quiet();
            self.gap_before_step();
            for headline in &headlines[self.thoughts_shown..] {
                let lead = self.lead();
                say!("{lead}{}", self.err.dim(&format!("✻ {headline}")));
            }
            self.thoughts_shown = headlines.len();
            if !self.input.pinned() {
                self.doing("Thinking");
            }
        }
    }

    fn note(&mut self, note: &str) {
        self.quiet();
        self.gap_before_step();
        let lead = self.lead();
        say!("{lead}{}", self.err.dim(&format!("↳ {note}")));
    }

    async fn approve(&mut self, call: &ToolCall) -> Approval {
        self.end_line();
        self.input.busy(None, std::time::Instant::now());
        self.step = None;
        let (question, always) = match call.name.as_str() {
            "bash" => ("Allow this command?", "Yes, and don't ask again for this command"),
            "edit" => ("Allow this edit?", "Yes, and don't ask again for file changes"),
            "write" => ("Allow writing this file?", "Yes, and don't ask again for file changes"),
            _ => ("Allow this?", "Yes, and don't ask again"),
        };
        if !self.input.pinned() {
            self.show_header(call);
            let prompt = format!("{}{question}  [y]es  [n]o  [a]lways › ", " ".repeat(steps::INDENT));
            return match self.input.read(&prompt, false).await {
                Read::Line(answer) => approval(&answer),
                // Ctrl-C at a question stops the whole turn.
                Read::Interrupted => {
                    self.interrupted.notify_one();
                    std::future::pending().await
                }
                // No answer (input has ended) means no.
                Read::Eof | Read::NextMode => Approval::No,
            };
        }
        // A command is asked about in full, in the box, and its line comes
        // once it's done. A file change shows its change above first, and
        // so does a command too long for the box.
        let command = if call.name == "bash" { steps::command(call) } else { None };
        let long = command.as_deref().is_some_and(|command| input::command_lines(command, self.err, self.input.text_width()).len() > input::COMMAND_LINES);
        if command.is_none() || long {
            self.show_header(call);
        }
        if long && let Some(command) = command.clone() {
            let s = self.err;
            self.show(move |width| {
                let pad = " ".repeat(steps::INDENT);
                input::command_lines(&command, s, width.saturating_sub(steps::INDENT)).into_iter().map(|line| format!("{pad}{line}")).collect()
            });
        }
        let answers = vec![("y", "Yes".to_owned()), ("a", always.to_owned()), ("n", "No".to_owned()), ("esc", "No, and tell rein what to do instead".to_owned())];
        let answer = match self.input.ask(Question { title: question.to_owned(), command, answers }).await {
            Read::Line(key) => approval(&key),
            _ => Approval::No,
        };
        if answer == Approval::Always {
            let what = if call.name == "bash" { "this command" } else { "file changes" };
            self.noted = Some(format!("won't ask again for {what} in this conversation"));
        }
        answer
    }

    fn tool_started(&mut self, call: &ToolCall) {
        self.end_line();
        let asked = self.step.as_ref().is_some_and(|step| step.call_id == call.call_id);
        if !asked {
            // Not asked about: the step's line shows once it's done, with
            // how it went.
            self.gap_before_step();
            let lead = self.lead();
            self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: false, done: false });
        }
        if self.input.pinned() {
            self.doing(&steps::doing(call));
            return;
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
        self.gap_before_step();
        self.step = None;
        let lead = self.lead();
        let s = self.err;
        let (call_, lead_, status) = (call.clone(), lead.clone(), s.red(&format!("blocked: {reason}")));
        self.show(move |width| vec![steps::line(&call_, &lead_, &s.red("●"), &status, s, width)]);
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
        let mut outcome = steps::outcome(call, output, s);
        if call.name == "plan" {
            // The same steps as before: just say where things stand.
            let names: Vec<String> = outcome.details.iter().map(|line| strip(line).chars().skip(2).collect()).collect();
            if names == self.plan {
                let now = outcome.details.iter().find(|line| strip(line).starts_with('▸')).map(|line| strip(line));
                outcome.status = s.dim(&now.map_or("all done".to_owned(), |now| format!("· {now}")));
                outcome.details.clear();
            } else {
                self.plan = names;
            }
        }
        if self.input.pinned() {
            let (name, target) = steps::describe(call);
            self.input.keep_output(format!("{name}  {target} · {}", strip(&outcome.status)), output);
        }
        let dot = match outcome.ok {
            Some(true) => s.green("●"),
            Some(false) => s.red("●"),
            None => s.dim("●"),
        };
        // Asked about first: its line is up there; the result goes under it.
        let lead = match step {
            Some(step) if step.header_shown => None,
            step => {
                // Never started, like a declined step: it's the first time it shows.
                if step.is_none() {
                    self.gap_before_step();
                }
                Some(step.map_or_else(|| self.lead(), |step| step.lead))
            }
        };
        let note = self.noted.take().map(|note| format!("{}{} {}", " ".repeat(steps::INDENT), s.green("✓"), s.dim(&note)));
        // Reads in a row share a line.
        if call.name == "read" && outcome.ok == Some(true) && note.is_none() && let Some(lead) = lead.clone() {
            self.show_read(call, outcome.status, lead);
            self.after_step = true;
            return;
        }
        // An edit made without asking shows what it changed; a question
        // showed it already. A new file's whole text is Ctrl-O away.
        let diff = if !asked && call.name == "edit" && outcome.ok == Some(true) { steps::diff_box(call, &self.root, s, WIDE, true) } else { Vec::new() };
        let (call_, viewer) = (call.clone(), self.input.pinned());
        self.show(move |width| {
            let mut lines = vec![match &lead {
                Some(lead) => steps::line(&call_, lead, &dot, &outcome.status, s, width),
                None => format!("{}{dot} {}", " ".repeat(steps::INDENT), outcome.status),
            }];
            lines.extend(steps::detail_lines(&outcome, s, width, viewer));
            lines.extend(note.clone());
            lines.extend(diff.iter().map(|line| ui::style::fit(line, width)));
            lines
        });
        self.after_step = true;
    }

    fn usage(&mut self, usage: &Usage) {
        self.turn.input += usage.input_tokens;
        self.turn.cached += usage.cached_input_tokens;
        self.context_used = usage.input_tokens;
        // The box's bar follows along during the turn.
        if self.input.pinned() {
            self.input.set_bar(self.bar());
        }
    }

    /// Lookups asked for together: one line over the box says so.
    fn tools_started(&mut self, calls: &[ToolCall]) {
        self.end_line();
        self.gap_before_step();
        self.step = None;
        let reads = calls.iter().all(|call| call.name == "read");
        self.doing(&if reads { format!("Reading {} files", calls.len()) } else { format!("Looking at {} things at once", calls.len()) });
    }

    /// What your message pointed at, under it: in rein's column, without
    /// rein's label, as you're the one who asked.
    fn attached(&mut self, call: &ToolCall, output: &str) {
        self.end_line();
        let s = self.err;
        let outcome = steps::outcome(call, output, s);
        if self.input.pinned() {
            let (name, target) = steps::describe(call);
            self.input.keep_output(format!("{name}  {target} · {}", strip(&outcome.status)), output);
        }
        let lead = " ".repeat(GUTTER);
        if call.name == "read" && outcome.ok == Some(true) {
            self.show_read(call, outcome.status, lead);
            return;
        }
        let dot = if outcome.ok == Some(false) { s.red("●") } else { s.green("●") };
        let (call_, viewer) = (call.clone(), self.input.pinned());
        self.show(move |width| {
            let mut lines = vec![steps::line(&call_, &lead, &dot, &outcome.status, s, width)];
            lines.extend(steps::detail_lines(&outcome, s, width, viewer));
            lines
        });
    }

    fn steering(&mut self) -> Vec<String> {
        self.input.steering()
    }

    /// Your message, where it went into the turn: between the steps it came
    /// between, so the screen reads as the model saw it.
    fn steered(&mut self, message: &str) {
        self.end_line();
        if self.fancy {
            let shown = self.input.shown(message);
            say!("\n{}", you_said(&shown, self.err, self.wrap_width().unwrap_or(80)));
        }
        // What rein does next starts under its label again, after a gap.
        self.labeled = false;
        self.after_you = true;
        self.after_step = false;
        self.after_text = false;
    }

    fn compacting(&mut self) {
        self.end_line();
        self.gap_before_step();
        let lead = self.lead();
        say!("{lead}{}", self.err.dim("compacting the conversation…"));
        self.compacting = true;
        self.doing("Compacting the conversation");
    }

    fn compacted(&mut self, report: &Compacted) {
        self.end_line();
        self.compacting = false;
        self.after_text = false;
        let retired = match report.retired {
            0 => String::new(),
            1 => ", 1 stale tool output retired".to_owned(),
            n => format!(", {n} stale tool outputs retired"),
        };
        let how = if report.summarized { "summarized" } else { "no summary needed" };
        let line = format!("compacted: about {} → {} tokens ({how}{retired})", thousands(report.before), thousands(report.after));
        say!("{}{}", " ".repeat(GUTTER), self.err.dim(&line));
        self.after_step = true;
        self.context_used = report.after;
        self.input.set_bar(self.bar());
    }
}

/// The finished `**headlines**` in a reasoning summary, which is how the
/// model titles each part: `Checking the tests`.
fn headlines(thoughts: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = thoughts;
    while let Some(start) = rest.find("**") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("**") else { break };
        let title = after[..end].trim();
        if !title.is_empty() {
            found.push(title.to_owned());
        }
        rest = &after[end + 2..];
    }
    found
}

/// What the requests cost on the wire: `412 KB as 128 KB (gzip, 3.2× fewer bytes)`.
fn traffic(t: rein_llm::Traffic) -> String {
    use rein_llm::Compression;
    if t.json == 0 {
        return "nothing yet".to_owned();
    }
    match t.compression {
        Compression::Gzip if t.sent < t.json => format!("{} as {} (gzip, {:.1}× fewer bytes)", bytes(t.json), bytes(t.sent), t.json as f64 / t.sent as f64),
        Compression::Gzip => format!("{} (too little to compress yet)", bytes(t.sent)),
        Compression::Off => format!("{}, uncompressed: compress is off in the settings", bytes(t.sent)),
        Compression::Refused => format!("{}, uncompressed: the server doesn't take compressed requests", bytes(t.sent)),
    }
}

/// `950 bytes`, `412 KB`, `3.1 MB`.
fn bytes(n: u64) -> String {
    match n {
        0..1000 => format!("{n} bytes"),
        1000..1_000_000 => format!("{} KB", n.div_ceil(1000)),
        _ => format!("{:.1} MB", n as f64 / 1e6),
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
        // Esc, in the box: no, and stop to say what to do instead.
        "esc" | "stop" => Approval::Stop,
        _ => Approval::No,
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
        (Some("refusal"), _) => format!("the model declined: {e}. Rephrase it, or try another model with /model"),
        (Some("authentication_error"), _) => format!("{e}. Check $ANTHROPIC_API_KEY, or save another key with `rein login anthropic`"),
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
    fn says_how_many_bytes() {
        assert_eq!(bytes(950), "950 bytes");
        assert_eq!(bytes(412_300), "413 KB");
        assert_eq!(bytes(3_140_000), "3.1 MB");
    }

    #[test]
    fn finds_the_headlines() {
        assert_eq!(headlines("**Deriving the list**\n\nsome text **Checking Pell solutions**"), ["Deriving the list", "Checking Pell solutions"]);
        assert!(headlines("**Half writ").is_empty());
        assert_eq!(headlines("**First** and **Sec"), ["First"]);
    }

    #[test]
    fn flags_win_over_the_settings() {
        let settings = Settings::parse("mode = edits\nmodel = from-file\neffort = low").unwrap();
        let parsed = AskArgs::parse(vec!["hi".into()], &settings).unwrap();
        assert_eq!((parsed.mode, parsed.effort.as_deref()), (Mode::Edits, Some("low")));
        let parsed = AskArgs::parse(["--mode", "trust", "-m", "flagged", "hi"].map(str::to_owned).to_vec(), &settings).unwrap();
        assert_eq!((parsed.mode, parsed.model.as_deref()), (Mode::Trust, Some("flagged")));
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
