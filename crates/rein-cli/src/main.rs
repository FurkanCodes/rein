//! `rein`: a tiny, fast agent harness. You hold the reins.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rein_llm::openai::signin::{self, Credentials};
use rein_agent::agent::{Agent, Approval, MAX_TURNS, OpenAi, Outcome, Ui};
use rein_agent::permissions::{Mode, Permissions};
use rein_agent::session::{self, Header, Session};
use rein_agent::tools::Tools;
use rein_agent::{context, prompt};
use rein_llm::Item;
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
  rein [options]                 Start a conversation in this folder
  rein [options] <prompt...>     Do one task and exit
  rein -c [<prompt...>]          Continue the latest conversation in this folder
  rein -r [<prompt...>]          Choose a saved conversation to continue
  rein login                     Continue with ChatGPT (use your ChatGPT plan)
  rein models                    List the models you can use

rein can read, write and edit files in this folder and run commands. In a
conversation, type /exit or press Ctrl-D to leave. Conversations are saved in
~/.config/rein/sessions, readable only by you.

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
        ["-h" | "--help"] => {
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
    fn parse(args: Vec<String>) -> CliResult<Self> {
        let mut model = std::env::var("REIN_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
        let (mut effort, mut system, mut record, mut words) = (None, None, None, Vec::new());
        let mut mode = Mode::Ask;
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

async fn ask(http: &rein_llm::reqwest::Client, args: AskArgs) -> CliResult {
    let client = client(http).await?;
    let record = match &args.record {
        Some(path) => Some(File::create(path).map_err(|e| format!("can't create {}: {e}", path.display()))?),
        None => None,
    };
    let root = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
    let permissions = load_permissions(&root, args.mode)?;
    let mut terminal = Terminal { mid_line: false, wrap: None, input: Input::start(), interrupted: Default::default() };
    let dir = config_dir()?;
    let folder = session::folder(&dir.join("sessions"), &root);

    let saved = match args.resume {
        Resume::New => None,
        Resume::Latest => match session::list(&folder, 1).into_iter().next() {
            Some(latest) => Some(latest.path),
            None => return Err("no saved conversation in this folder yet".into()),
        },
        Resume::Pick => match pick(&folder, &mut terminal).await? {
            Some(path) => Some(path),
            None => return Ok(()),
        },
    };
    let (session, history, context) = match saved {
        Some(path) => {
            if args.system.is_some() {
                return Err("-s can't change a saved conversation's system prompt; start a new one instead".into());
            }
            let (session, history) = Session::load(&path).map_err(describe)?;
            say_where_we_were(session.header(), &history);
            (session, history, None)
        }
        None => {
            let now = local_time(SystemTime::now());
            let header = Header {
                id: rein_llm::openai::new_session_id().map_err(describe)?,
                cwd: root.clone(),
                started: format!("{} {:02}:{:02}", now.date(), now.hour, now.minute),
                model: args.model.clone(),
                instructions: Some(prompt::system(args.system.as_deref())),
            };
            let stamp = format!("{}T{:02}-{:02}-{:02}", now.date(), now.hour, now.minute, now.second);
            let files = context::find(&root, Some(&dir.join("AGENTS.md")));
            if !files.is_empty() {
                let names: Vec<String> = files.iter().map(|f| shown_path(&f.path, &root)).collect();
                eprintln!("Instructions: {}", names.join(", "));
            }
            let context = context::message(&root, std::env::consts::OS, &now.date(), &files);
            (Session::new(&folder, &stamp, header), Vec::new(), Some(context))
        }
    };
    let header = session.header().clone();
    let mut agent = Agent {
        model: OpenAi { client, record },
        tools: Tools::new(root),
        permissions,
        // A continued conversation keeps its model unless -m says otherwise.
        model_name: if args.model_given || args.resume == Resume::New { args.model } else { header.model },
        instructions: header.instructions,
        reasoning_effort: args.effort,
        session_id: Some(header.id),
        history,
        session: Some(session),
        context,
    };
    match args.prompt {
        Some(prompt) => turn(&mut agent, prompt, &mut terminal).await,
        None => conversation(&mut agent, &mut terminal).await,
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
        eprintln!("(interrupted)");
        return Ok(());
    };
    match outcome.map_err(describe)? {
        Outcome::Done => Ok(()),
        Outcome::TurnLimit => Err(format!("stopped after {MAX_TURNS} turns of tool calls; ask again to continue")),
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
        eprintln!("{:>2}  {} {:02}:{:02}  {messages:<12}  {first}", i + 1, when.date(), when.hour, when.minute);
    }
    loop {
        match terminal.input.read("Continue which one? (number, or Enter to cancel) ", false).await {
            Read::Line(line) if line.trim().is_empty() => return Ok(None),
            Read::Line(line) => match line.trim().parse::<usize>() {
                Ok(n) if (1..=saved.len()).contains(&n) => return Ok(Some(saved[n - 1].path.clone())),
                _ => eprintln!("Type a number from 1 to {}.", saved.len()),
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
    eprintln!("Continuing the conversation from {} ({} message{} so far).", header.started, typed.len(), if typed.len() == 1 { "" } else { "s" });
    if let Some(last) = typed.last() {
        eprintln!("Last message: {}", shorten(last, 70));
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
async fn conversation(agent: &mut Agent<OpenAi>, terminal: &mut Terminal) -> CliResult {
    eprintln!("Type a message. End a line with \\ to keep typing on the next.");
    eprintln!("Ctrl-C stops an answer; /exit or Ctrl-D leaves.");
    let mut last_interrupt: Option<std::time::Instant> = None;
    loop {
        eprintln!();
        let Some(line) = message(terminal, &mut last_interrupt).await else {
            return Ok(());
        };
        match line.trim() {
            "" => continue,
            "/exit" | "/quit" => return Ok(()),
            message => {
                if let Err(e) = turn(agent, message.to_owned(), terminal).await {
                    eprintln!("rein: {e}");
                }
            }
        }
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

/// Reads your next message. A line ending in `\` continues on the next.
/// Ctrl-C clears what you typed; a second one within two seconds, or
/// Ctrl-D, ends the conversation (`None`).
async fn message(terminal: &mut Terminal, last_interrupt: &mut Option<std::time::Instant>) -> Option<String> {
    let mut text = String::new();
    loop {
        let prompt = if text.is_empty() { "> " } else { "… " };
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
                eprintln!("(press Ctrl-C again to leave)");
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

/// Wraps streamed text at word boundaries. Words and the spaces before
/// them are held until the word is complete: printed text can't be taken
/// back, so a space is only printed once the next word is known to fit on
/// the same line. Indentation at the start of a line is kept; the space
/// where a line wraps is dropped. A word longer than a whole line is
/// printed as is, so the terminal breaks it rather than rein.
struct Wrap {
    width: usize,
    column: usize,
    spaces: String,
    word: String,
}

impl Wrap {
    fn new(width: usize) -> Self {
        Wrap { width, column: 0, spaces: String::new(), word: String::new() }
    }

    /// Takes the next piece of text and returns what can be printed now.
    fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            match c {
                '\n' => {
                    self.flush(&mut out);
                    self.spaces.clear(); // trailing spaces show nothing
                    out.push('\n');
                    self.column = 0;
                }
                ' ' | '\t' => {
                    self.flush(&mut out);
                    self.spaces.push(c);
                }
                c => self.word.push(c),
            }
        }
        out
    }

    /// Whatever is still held back, at the end of an answer.
    fn finish(&mut self) -> String {
        let mut out = String::new();
        self.flush(&mut out);
        self.spaces.clear();
        out
    }

    fn flush(&mut self, out: &mut String) {
        if self.word.is_empty() {
            return;
        }
        let mut column = self.column;
        for c in self.spaces.chars() {
            column += if c == '\t' { 8 - column % 8 } else { 1 };
        }
        let width = unicode_width::UnicodeWidthStr::width(self.word.as_str());
        if self.column > 0 && column + width > self.width {
            out.push('\n'); // the spaces before the word are dropped
            column = 0;
        } else {
            out.push_str(&self.spaces);
        }
        self.spaces.clear();
        out.push_str(&self.word);
        self.word.clear();
        self.column = column + width;
        if self.column > self.width {
            // Longer than a line: the terminal breaks it; follow along.
            self.column %= self.width.max(1);
        }
    }
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

/// Lines typed at the keyboard, read by a thread so that waiting for one
/// never stops rein from noticing Ctrl-C. In a real terminal the thread
/// runs a line editor (arrow keys, history); otherwise it reads plain
/// lines, so piping answers into rein keeps working.
struct Input {
    /// Prompts to show; the thread reads one line for each.
    requests: std::sync::mpsc::Sender<(String, bool)>,
    replies: tokio::sync::mpsc::UnboundedReceiver<Read>,
    /// A read asked for but not answered yet, kept if the asker gave up.
    pending: bool,
}

impl Input {
    fn start() -> Self {
        let (requests, asked) = std::sync::mpsc::channel::<(String, bool)>();
        let (tx, replies) = tokio::sync::mpsc::unbounded_channel();
        let interactive = std::io::stdin().is_terminal();
        std::thread::spawn(move || {
            let mut editor = if interactive { rustyline::DefaultEditor::new().ok() } else { None };
            for (prompt, remember) in asked {
                let read = match &mut editor {
                    Some(editor) => match editor.readline(&prompt) {
                        Ok(line) => {
                            if remember {
                                let _ = editor.add_history_entry(line.as_str());
                            }
                            Read::Line(line)
                        }
                        Err(rustyline::error::ReadlineError::Interrupted) => Read::Interrupted,
                        Err(_) => Read::Eof,
                    },
                    None => {
                        eprint!("{prompt}");
                        let mut line = String::new();
                        match std::io::stdin().read_line(&mut line) {
                            Ok(0) | Err(_) => Read::Eof,
                            Ok(_) => Read::Line(line.trim_end_matches(['\n', '\r']).to_owned()),
                        }
                    }
                };
                if tx.send(read).is_err() {
                    break;
                }
            }
        });
        Input { requests, replies, pending: false }
    }

    /// Shows `prompt` and reads one line. `remember` puts it in the
    /// history you reach with the up arrow: your messages, not answers to
    /// questions.
    async fn read(&mut self, prompt: &str, remember: bool) -> Read {
        if !self.pending {
            if self.requests.send((prompt.to_owned(), remember)).is_err() {
                return Read::Eof;
            }
            self.pending = true;
        }
        let read = self.replies.recv().await.unwrap_or(Read::Eof);
        self.pending = false;
        read
    }
}

/// The terminal as rein's [`Ui`]: answer text on stdout, everything else
/// (tool activity, questions, token counts) on stderr.
struct Terminal {
    mid_line: bool,
    /// Wraps answer text at the terminal's edge; `None` when output isn't a
    /// terminal, so piped answers come out exactly as the model wrote them.
    wrap: Option<Wrap>,
    input: Input,
    /// Ctrl-C pressed at a question; the turn waits on this to stop.
    interrupted: std::sync::Arc<tokio::sync::Notify>,
}

impl Terminal {
    fn end_line(&mut self) {
        if let Some(wrap) = &mut self.wrap {
            print!("{}", wrap.finish());
        }
        // Measured again for the next answer, in case the window changed.
        self.wrap = None;
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
    }
}

impl Ui for Terminal {
    fn text(&mut self, delta: &str) {
        if self.wrap.is_none() {
            self.wrap = terminal_width().map(Wrap::new);
        }
        match &mut self.wrap {
            Some(wrap) => print!("{}", wrap.push(delta)),
            None => print!("{delta}"),
        }
        std::io::stdout().flush().ok();
        self.mid_line = !delta.ends_with('\n');
    }

    async fn approve(&mut self, call: &ToolCall) -> Approval {
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
        match self.input.read(&format!("{question} [y/N/a] ({always}) "), false).await {
            Read::Line(answer) => approval(&answer),
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
        eprintln!("• {}", show(call));
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
        ("read", Some(path), _) => match (args["start_line"].as_u64(), args["line_count"].as_u64()) {
            (None, None) => format!("read {path}"),
            (start, Some(count)) => format!("read {path}, lines {}-{}", start.unwrap_or(1), start.unwrap_or(1) + count.max(1) - 1),
            (Some(start), None) => format!("read {path}, from line {start}"),
        },
        ("write" | "edit", Some(path), _) => format!("{} {path}", call.name),
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
        assert_eq!((parsed.mode, parsed.prompt.as_deref()), (Mode::Edits, Some("fix the bug")));
        assert_eq!(args("--mode edits").unwrap().prompt, None, "no task: a conversation");
        assert_eq!(args("--mode yolo hi").err().as_deref(), Some("unknown mode yolo; use ask, edits, read-only or trust"));
    }

    /// Feeds `text` to a wrapper in uneven pieces, the way it streams in.
    fn wrapped(width: usize, text: &str) -> String {
        let mut wrap = Wrap::new(width);
        let chars: Vec<char> = text.chars().collect();
        let mut out: String = chars.chunks(3).map(|piece| wrap.push(&piece.iter().collect::<String>())).collect();
        out.push_str(&wrap.finish());
        out
    }

    #[test]
    fn wraps_at_words_not_inside_them() {
        assert_eq!(wrapped(20, "manages its workflow and memory"), "manages its workflow\nand memory");
        assert_eq!(wrapped(16, "manages its workflow and memory"), "manages its\nworkflow and\nmemory");
        // Lines the model ended itself stay as they are, indentation too.
        assert_eq!(wrapped(20, "fn main() {\n    let x = 1;\n}"), "fn main() {\n    let x = 1;\n}");
        // A word longer than the line isn't broken by rein.
        assert_eq!(wrapped(10, "see https://example.com/long/path now"), "see\nhttps://example.com/long/path\nnow");
        // Wide characters count double.
        assert_eq!(wrapped(10, "日本語 日本語"), "日本語\n日本語");
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
        assert_eq!(parsed.prompt.as_deref(), Some("what is a harness"));

        assert_eq!(args("what is a harness").unwrap().system, None);
        assert_eq!(args("--system").err().as_deref(), Some("--system needs a value"));
    }
}
