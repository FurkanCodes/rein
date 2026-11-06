//! The tools the model can call. Each takes its arguments as JSON text and
//! returns text for the model, also when something goes wrong: an error the
//! model can read is one it can work around.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rein_llm::{Loading, Tool, ToolCall};

use crate::background::{self, Foreground, Jobs, Started};
use crate::redact::{self, Secrets};
use crate::sandbox::{self, Access, Run};
use crate::undo::{Listed, Plan, Undo, Watched};
use crate::{files, search};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The most a tool sends back. Everything the model sees is resent with
/// every later request, so long outputs get cut.
pub(crate) const MAX_OUTPUT: usize = 30_000;

pub struct Tools {
    /// The project directory: `read` can't see outside it, `bash` starts in it.
    root: PathBuf,
    /// How long a command may run before it's stopped.
    timeout: Duration,
    /// What rein changed in the project, turn by turn, for `/undo`.
    undo: std::sync::Arc<std::sync::Mutex<Undo>>,
    /// What the last command did to the project's files, for showing.
    watched: std::sync::Mutex<Option<Watched>>,
    /// What holds commands in: `bash` runs in it, when it's told to.
    sandbox: sandbox::Shared,
    /// The keys rein knows, hidden from what every tool says.
    secrets: Secrets,
    /// The MCP servers rein started, whose tools run through them.
    mcp: Option<std::sync::Arc<rein_mcp::Servers>>,
    /// Where the images tools give are kept, once each.
    images: PathBuf,
    /// The notes that outlast a conversation, for the project; `None` with
    /// memory off.
    memory: Option<crate::memory::Memory>,
    /// The images each call gave, by its ID, until the agent takes them
    /// into the conversation after its result.
    pictures: std::sync::Mutex<std::collections::HashMap<String, Vec<rein_llm::Image>>>,
    /// The commands `bash` started, the ones in the background too: they
    /// stop when these tools go.
    jobs: std::sync::Arc<Jobs>,
}

/// A file the model named, found inside the project: the project, resolved,
/// and the way to the file from it, also resolved, to be opened a folder at
/// a time ([`files`]) so nothing swapped in since leads out of it.
struct Spot {
    root: PathBuf,
    rel: PathBuf,
}

impl Spot {
    fn full(&self) -> PathBuf {
        self.root.join(&self.rel)
    }

    /// What it says, read off the loop's thread.
    async fn read(&self) -> std::io::Result<Vec<u8>> {
        let (root, rel) = (self.root.clone(), self.rel.clone());
        tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut files::open(&root, &rel)?, &mut bytes)?;
            Ok(bytes)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    /// Writes `bytes` there, making the folders on the way, off the loop's thread.
    async fn write(&self, bytes: Vec<u8>) -> std::io::Result<()> {
        let (root, rel) = (self.root.clone(), self.rel.clone());
        tokio::task::spawn_blocking(move || std::io::Write::write_all(&mut files::create(&root, &rel)?, &bytes)).await.map_err(std::io::Error::other)?
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    /// The first line to return, counting from 1.
    start_line: Option<usize>,
    /// How many lines to return; all the rest when missing.
    line_count: Option<usize>,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    output: Option<GrepOutput>,
    context: Option<usize>,
}

/// What a search gives back. A value the model misspells is an error it's
/// told about, not quietly the default.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrepOutput {
    Lines,
    Files,
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct PlanArgs {
    steps: Vec<PlanStep>,
}

#[derive(Deserialize)]
struct PlanStep {
    status: StepStatus,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StepStatus {
    Pending,
    InProgress,
    Done,
}

/// A `bash` call: the command, and what it needs.
#[derive(Debug, Clone, Deserialize)]
pub struct BashCall {
    command: String,
    /// How long it may run, if longer than the default.
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// Missing or null is look, the least there is.
    #[serde(default)]
    access: Option<Access>,
    /// Why it needs network or outside access, for the user.
    #[serde(default)]
    reason: Option<String>,
    /// Run it in the background: rein answers at once.
    #[serde(default)]
    background: Option<bool>,
}

impl BashCall {
    /// The `bash` call `call` is: `None` for another tool, or arguments
    /// that don't read as one.
    pub fn of(call: &ToolCall) -> Option<BashCall> {
        (ToolName::of(call) == Some(ToolName::Bash)).then(|| parse(&call.arguments).ok()).flatten()
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn access(&self) -> Access {
        self.access.unwrap_or_default()
    }

    /// Whether it's to run in the background.
    pub fn background(&self) -> bool {
        self.background == Some(true)
    }

    /// The reason given, if it says anything.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref().map(str::trim).filter(|reason| !reason.is_empty())
    }
}

/// The longest a command may ask to run.
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
    replace_all: bool,
}

/// How a tool's output starts when it failed. Compaction's record tells
/// what worked by it.
pub const ERROR: &str = "error: ";

/// How a command ended, as the line `bash` writes after its output says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended<'a> {
    /// It exited with this code.
    Code(&'a str),
    /// A signal stopped it.
    Signal,
}

pub(crate) const SIGNAL_LINE: &str = "[stopped by a signal]";

/// How the command whose result is `output` ended, and which of its lines
/// says so: everything before that line is what the command printed. Notes
/// rein adds (the sandbox's walls, a step limit) come after it, so it's the
/// last such line, not the last line. `None` for a result without one, from
/// a command that never ran.
pub fn ended(output: &str) -> Option<(usize, Ended<'_>)> {
    let lines: Vec<&str> = output.lines().collect();
    lines.iter().enumerate().rev().find_map(|(i, line)| {
        let line = *line;
        if line == SIGNAL_LINE {
            return Some((i, Ended::Signal));
        }
        line.strip_prefix("[exit code ")?.strip_suffix(']').map(|code| (i, Ended::Code(code)))
    })
}

/// rein's tools. The model names one in each call; [`ToolName::of`] reads
/// that name once, and everything after works with this, so a misspelt
/// tool is a compile error, not a call that quietly does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolName {
    Read,
    Grep,
    Glob,
    Write,
    Edit,
    Plan,
    Bash,
    /// Searching the conversation: rein's own, see `compact::recall`.
    Recall,
    /// Finding the tools held back from the model: rein's own, see
    /// [`crate::finding`].
    ToolSearch,
    /// Searching the web, with the model chosen for it: see [`crate::web`].
    WebSearch,
    /// Loading a skill: see [`crate::skills`].
    Skill,
    /// The other reins on this computer, and reaching them: the user's
    /// side of rein does these, see [`crate::peers`].
    Peers,
    Message,
    Delegate,
    /// A new rein in a tab of its own, for a task: a sub-agent you can see.
    Spawn,
    /// Notes that outlast the conversation: see [`crate::memory`].
    Memory,
    /// Reading what a background command prints, or stopping it: see
    /// [`crate::background`].
    Background,
}

impl ToolName {
    pub const ALL: [ToolName; 17] = [
        Self::Read,
        Self::Grep,
        Self::Glob,
        Self::Write,
        Self::Edit,
        Self::Plan,
        Self::Bash,
        Self::Recall,
        Self::ToolSearch,
        Self::WebSearch,
        Self::Skill,
        Self::Peers,
        Self::Message,
        Self::Delegate,
        Self::Spawn,
        Self::Memory,
        Self::Background,
    ];

    /// The name the model knows it by.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Grep => "grep",
            Self::Glob => "glob",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Plan => "plan",
            Self::Bash => "bash",
            Self::Recall => "recall",
            Self::ToolSearch => rein_llm::TOOL_SEARCH,
            Self::WebSearch => "web_search",
            Self::Skill => "skill",
            Self::Peers => "peers",
            Self::Message => "message",
            Self::Delegate => "delegate",
            Self::Spawn => "spawn",
            Self::Memory => "memory",
            Self::Background => "background",
        }
    }

    /// The tool `call` asks for; `None` for one rein doesn't have.
    pub fn of(call: &ToolCall) -> Option<ToolName> {
        call.name.parse().ok()
    }

    /// Only looks, inside the project: needs no permission, and runs at the
    /// same time as other looks.
    pub const fn only_looks(self) -> bool {
        matches!(self, Self::Read | Self::Grep | Self::Glob)
    }

    /// Changes files: undone with `/undo`, and allowed by file-change modes.
    pub const fn changes_files(self) -> bool {
        matches!(self, Self::Write | Self::Edit)
    }

    /// Reaches another rein, which the user's side of rein does.
    pub const fn reaches_peers(self) -> bool {
        matches!(self, Self::Peers | Self::Message | Self::Delegate | Self::Spawn)
    }
}

impl std::str::FromStr for ToolName {
    type Err = ();

    fn from_str(name: &str) -> Result<Self, ()> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == name).ok_or(())
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Tools {
    /// Tools for the project at `root`, with no sandbox: commands told to
    /// run in one are refused until [`Tools::with_sandbox`] gives one.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let undo = std::sync::Arc::new(std::sync::Mutex::new(Undo::new(root.clone())));
        Tools {
            root,
            timeout: Duration::from_secs(60),
            undo,
            watched: Default::default(),
            sandbox: Default::default(),
            secrets: Secrets::default(),
            mcp: None,
            images: std::env::temp_dir().join(format!("rein-images-{}", std::process::id())),
            pictures: Default::default(),
            memory: None,
            jobs: Jobs::new(),
        }
    }

    /// Keeps the images tools give in `folder`, beside the project's
    /// conversations, which name them.
    pub fn with_images(mut self, folder: PathBuf) -> Self {
        self.images = folder;
        self
    }

    /// Keeps notes across conversations in `memory`, with the tool to.
    pub fn with_memory(mut self, memory: crate::memory::Memory) -> Self {
        self.memory = Some(memory);
        self
    }

    /// The project's notes, with memory on.
    pub fn memory(&self) -> Option<&crate::memory::Memory> {
        self.memory.as_ref()
    }

    /// The images the call `call_id` gave, taken: they go into the
    /// conversation after its result.
    pub fn take_images(&self, call_id: &str) -> Vec<rein_llm::Image> {
        self.pictures.lock().map(|mut pictures| pictures.remove(call_id)).ok().flatten().unwrap_or_default()
    }

    /// Readies `bytes`, said to be `what`, to show the model with the
    /// call `call_id`'s result, off the loop's thread: what's said of it in
    /// the result, or why it can't be shown.
    async fn picture(&self, call_id: &str, bytes: Vec<u8>, what: String) -> Result<String, String> {
        let folder = self.images.clone();
        let image = tokio::task::spawn_blocking(move || crate::image::prepare(&bytes, &what, &folder)).await.unwrap_or_else(|e| Err(e.to_string()))?;
        let said = format!("[{}]", image.about);
        if let Ok(mut pictures) = self.pictures.lock() {
            pictures.entry(call_id.to_owned()).or_default().push(image);
        }
        Ok(said)
    }

    /// Keeps the copies `/undo` puts back in `folder`, rein's folder for
    /// this project, so what commands do can be undone too, and after
    /// rein starts again. Without it, only `write` and `edit` are undone,
    /// and only while rein runs.
    pub fn with_undo(mut self, folder: PathBuf) -> Self {
        let mut undo = Undo::new(self.root.clone());
        undo.keep_in(folder);
        self.undo = std::sync::Arc::new(std::sync::Mutex::new(undo));
        self
    }

    /// Runs the tools of these MCP servers too: see [`rein_mcp`].
    pub fn with_mcp(mut self, servers: std::sync::Arc<rein_mcp::Servers>) -> Self {
        self.mcp = Some(servers);
        self
    }

    /// The MCP servers rein started, if any.
    pub fn mcp(&self) -> Option<&rein_mcp::Servers> {
        self.mcp.as_deref()
    }

    /// Hides these values too, from now on: a server's keys, once you've
    /// added it.
    pub fn hide_too(&mut self, values: impl IntoIterator<Item = String>) {
        self.secrets = self.secrets.and(values);
        self.jobs.hide(self.secrets.clone());
    }

    /// Hides `secrets` in what the tools say: see [`redact`].
    pub fn with_secrets(mut self, secrets: Secrets) -> Self {
        self.secrets = secrets;
        self.jobs.hide(self.secrets.clone());
        self
    }

    /// Runs the project's `check`, as `how` says: see [`crate::check`].
    pub(crate) async fn check(&self, check: &crate::check::Check, how: Run) -> crate::check::Ran {
        crate::check::run(check, &self.root, &self.sandbox, how).await
    }

    /// `text` with the keys rein knows hidden.
    pub(crate) fn hidden(&self, text: String) -> String {
        self.secrets.hide(text)
    }

    /// The commands `bash` started, those in the background among them.
    pub fn jobs(&self) -> &std::sync::Arc<Jobs> {
        &self.jobs
    }

    /// The background commands that ended since the model was told, now
    /// told: what `/undo` didn't see them change is kept in the turn first.
    pub async fn ended(&self) -> Vec<background::Ended> {
        let ended = self.jobs.untold();
        if !ended.is_empty() {
            self.on_undo(|undo, hides| undo.settle(hides)).await;
        }
        ended
    }

    pub fn with_sandbox(mut self, sandbox: sandbox::Shared) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Follows the conversation `session` for `/undo`: the turns it kept,
    /// if it's one continued. What's from the last one goes.
    pub fn open_undo(&self, session: &str) {
        locked(&self.undo).open(session);
    }

    /// Starts a new turn, started by `said`: what changes from now on is
    /// undone together.
    pub async fn turn(&self, said: &str) {
        let said = said.to_owned();
        self.on_undo(move |undo, hides| undo.turn(&said, hides)).await;
    }

    /// The turns there are to undo, the latest first.
    pub async fn undo_list(&self) -> Vec<Listed> {
        self.on_undo(|undo, hides| undo.listed(hides)).await
    }

    /// What undoing back to before `turn` (from [`Tools::undo_list`]) would do.
    pub async fn undo_plan(&self, turn: usize) -> Plan {
        self.on_undo(move |undo, hides| undo.plan(turn, hides)).await
    }

    /// Undoes `turn` and every one after it: what it did.
    pub async fn undo(&self, turn: usize) -> Plan {
        self.on_undo(move |undo, hides| undo.undo(turn, hides)).await
    }

    /// What the last command did to the project's files, once.
    pub fn take_watched(&self) -> Option<Watched> {
        locked(&self.watched).take()
    }

    /// Runs `work` on what `/undo` keeps, off the loop's thread: it reads
    /// and copies files.
    async fn on_undo<T: Default + Send + 'static>(&self, work: impl FnOnce(&mut Undo, search::Hides) -> T + Send + 'static) -> T {
        let (undo, sandbox) = (self.undo.clone(), self.sandbox.clone());
        tokio::task::spawn_blocking(move || work(&mut locked(&undo), &|path: &Path| sandbox.places().hides(path))).await.unwrap_or_default()
    }

    /// The project folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The sandbox commands run in, where there's one.
    pub fn sandbox(&self) -> &sandbox::Shared {
        &self.sandbox
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// What the model is told about `bash`. It's the same in every mode,
    /// with the sandbox on or off, so that changing either doesn't change
    /// the start of every request and lose the prompt cache: what a mode
    /// allows, a refusal says, and what the walls held, a failure.
    fn bash_description(&self) -> &'static str {
        if self.sandbox.available() {
            "Run a shell command in the project root and return its output and exit code. Say what each command \
             needs in access, and ask for no more: look for one that only reads, write for one that changes files \
             in the project (builds and tests usually do), network for one that also uses the internet, and outside \
             for one that must write elsewhere (installing packages into a cache in the home folder), or use docker, \
             ssh or a terminal. When the user has the sandbox on, commands run in it, within that access: secrets in \
             the home folder can't be read and files that run code later (.git/hooks, .git/config, .rein) can't be \
             changed. Depending on the mode, a command, or more access, may need the user's approval, or be refused. \
             A command still running after 60 seconds, or the timeout you ask for (up to 600, for slow builds and \
             test suites), goes on in the background. Start servers, watchers and anything else that keeps running \
             with background: rein answers at once with its name, and tells you when it ends."
        } else {
            "Run a shell command in the project root and return its output and exit code. Say what it needs in \
             access. Depending on the mode, the user may approve each command before it runs. A command still \
             running after 60 seconds, or the timeout you ask for (up to 600, for slow builds and test suites), goes \
             on in the background. Start servers, watchers and anything else that keeps running with background: \
             rein answers at once with its name, and tells you when it ends."
        }
    }

    /// What the model is told about each tool.
    pub fn specs(&self) -> Vec<Tool> {
        let mut specs = vec![
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Read.as_str().into(),
                description: "Read a UTF-8 text file in the project. A long file comes back in parts: the \
                              reply ends by saying which lines it holds and where to continue."
                    .into(),
                parameters: object(json!({
                    "path": { "type": "string", "description": "Path relative to the project root." },
                    "start_line": { "type": ["integer", "null"], "description": "The first line to read, counting from 1. Null for the start." },
                    "line_count": { "type": ["integer", "null"], "description": "How many lines to read. Null for as many as fit." }
                })),
            },
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Grep.as_str().into(),
                description: "Search what the project's files say with a regular expression (Rust syntax), and \
                              get the matching lines with their numbers, grouped by file. Files that .gitignore \
                              leaves out, binary files and .git aren't searched. Use this rather than grep or rg \
                              in bash: it needs no approval."
                    .into(),
                parameters: object(json!({
                    "pattern": { "type": "string", "description": "A regular expression, like `fn parse_\\w+` or `TODO|FIXME`. ^ and $ are a line's start and end. Escape ( . [ and the like to match them as they are." },
                    "path": { "type": ["string", "null"], "description": "A file or folder to search, relative to the project root. Null for the whole project." },
                    "glob": { "type": ["string", "null"], "description": "Only files matching this glob: `*.rs` or `*.{ts,tsx}` match names in any folder; with a slash, like `src/**/*.rs`, the path. Null for every file." },
                    "ignore_case": { "type": "boolean", "description": "Match upper and lower case alike (ASCII letters)." },
                    "output": { "type": "string", "enum": ["lines", "files"], "description": "lines: the matching lines. files: only which files match, and how many lines in each; cheaper for a first look." },
                    "context": { "type": ["integer", "null"], "description": "How many lines to show before and after each match, at most 5. Null for none." }
                })),
            },
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Glob.as_str().into(),
                description: "Find files in the project by name or path, like `*.rs`, `src/**/test_*.py` or \
                              `Cargo.toml`. Returns their paths relative to the project root, sorted. Files that \
                              .gitignore leaves out aren't listed."
                    .into(),
                parameters: object(json!({
                    "pattern": { "type": "string", "description": "A glob: * and ? within one folder, ** across folders, {a,b} for either. Without a slash it matches file names in any folder." },
                    "path": { "type": ["string", "null"], "description": "A folder to look in, relative to the project root. Null for the whole project." }
                })),
            },
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Write.as_str().into(),
                description: "Create a text file, or replace an existing file's whole contents. Missing folders \
                              are created. To change part of a file, use edit instead."
                    .into(),
                parameters: object(json!({
                    "path": { "type": "string", "description": "Path relative to the project root." },
                    "content": { "type": "string", "description": "The complete new contents of the file." }
                })),
            },
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Edit.as_str().into(),
                description: "Replace text in a file. old_text must match the file exactly, spaces and \
                              indentation included, and appear exactly once unless replace_all is true: add \
                              surrounding lines to make it unique. Read the file first if you haven't."
                    .into(),
                parameters: object(json!({
                    "path": { "type": "string", "description": "Path relative to the project root." },
                    "old_text": { "type": "string", "description": "The exact text to replace." },
                    "new_text": { "type": "string", "description": "The text to put in its place." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of exactly one." }
                })),
            },
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Plan.as_str().into(),
                description: "Keep a short checklist for work with several steps, so you and the user can see \
                              where things stand. Send the whole list each time, with one step in_progress; mark \
                              steps done as you finish them. Skip it for small tasks."
                    .into(),
                parameters: object(json!({
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string", "description": "What to do, in a few words." },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "done"] }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                })),
            },
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Bash.as_str().into(),
                description: self.bash_description().into(),
                parameters: object(json!({
                    "command": { "type": "string", "description": "The command line, run with `sh -c`." },
                    "timeout_seconds": { "type": ["integer", "null"], "description": "How long it may run, at most 600. Null for 60." },
                    "access": {
                        "type": "string",
                        "enum": Access::ALL.map(Access::as_str),
                        "description": "What the command needs. look: only reads. write: also changes files in the project. network: also uses the internet. outside: runs without the sandbox."
                    },
                    "reason": { "type": ["string", "null"], "description": "For network or outside: why, in a few words the user will read. Null otherwise." },
                    "background": { "type": ["boolean", "null"], "description": "True to run it in the background, for a server, a watcher or a long run: rein answers at once with its name (b1). Null for no." }
                })),
            },
            Tool {
                loading: Loading::UpFront,
                strict: true,
                name: ToolName::Background.as_str().into(),
                description: "Check on a command running in the background, or stop it. You're told when one ends \
                              without asking, so never poll in a loop or sleep. read: what it printed since you last \
                              read, and whether it still runs; with until, it first waits for a line matching that \
                              regular expression (a server's \"ready\" line, say), or for the command to end. stop: \
                              stops it and everything it started. Commands stop when the conversation ends."
                    .into(),
                parameters: object(json!({
                    "id": { "type": "string", "description": "Its name, like b1." },
                    "action": { "type": "string", "enum": ["read", "stop"], "description": "read or stop." },
                    "until": { "type": ["string", "null"], "description": "For read: a regular expression (Rust syntax) to wait for in a line it prints, like `ready|listening`. Null to read at once." },
                    "seconds": { "type": ["integer", "null"], "description": "For read with until: how long to wait, at most 600. Null for 30." }
                })),
            },
        ];
        if self.memory.is_some() {
            specs.push(crate::memory::spec());
        }
        specs
    }

    /// Runs one call and returns the text to send back to the model. `how`
    /// is how a command runs, as the permissions decided: in the sandbox,
    /// within its walls, or not. The other tools keep to the project
    /// themselves, and run as they always do.
    pub async fn run(&self, call: &ToolCall, how: Run) -> String {
        let result = match ToolName::of(call) {
            Some(ToolName::Read) => self.read(&call.call_id, &call.arguments).await,
            Some(ToolName::Grep) => self.grep(&call.arguments).await,
            Some(ToolName::Glob) => self.glob(&call.arguments).await,
            Some(ToolName::Bash) => self.bash(&call.arguments, how).await,
            Some(ToolName::Background) => self.jobs.call(&call.arguments).await,
            Some(ToolName::Write) => self.write(&call.arguments).await,
            Some(ToolName::Edit) => self.edit(&call.arguments).await,
            // The checklist is for showing; the model keeps it in its own words.
            Some(ToolName::Plan) => parse::<PlanArgs>(&call.arguments).map(|plan| {
                let done = plan.steps.iter().filter(|step| step.status == StepStatus::Done).count();
                format!("Plan updated: {done} of {} done.", plan.steps.len())
            }),
            Some(ToolName::Memory) => match &self.memory {
                Some(memory) => memory.run(&call.arguments, &crate::memory::today(), &|text| self.secrets.hide(text)),
                None => Err(format!("there is no tool called {}", call.name)),
            },
            // `recall` is the agent's own: it searches the conversation. The
            // peers are reached by whoever runs the agent.
            Some(ToolName::Recall | ToolName::ToolSearch | ToolName::WebSearch | ToolName::Skill | ToolName::Peers | ToolName::Message | ToolName::Delegate | ToolName::Spawn) => {
                Err(format!("there is no tool called {}", call.name))
            }
            None => match &self.mcp {
                Some(servers) if rein_mcp::is_mcp(&call.name) => {
                    let said = servers.call(&call.name, &call.arguments).await;
                    let (server, tool) = rein_mcp::parts(&call.name).unwrap_or_default();
                    let mut text = Vec::new();
                    let said = match said {
                        Ok(said) => {
                            for picture in said.images {
                                let said = self.picture(&call.call_id, picture.data, format!("an image {server}'s {tool} gave")).await;
                                text.push(said.unwrap_or_else(|why| format!("[an image rein can't show: {why}]")));
                            }
                            Ok(said.text)
                        }
                        Err(why) => Err(why),
                    };
                    let mut output = self.servers_said(said);
                    for line in text {
                        output.push_str(if output.is_empty() { "" } else { "\n" });
                        output.push_str(&line);
                    }
                    return output;
                }
                _ => Err(format!("there is no tool called {}", call.name)),
            },
        };
        truncate(self.secrets.hide(result.unwrap_or_else(|e| format!("{ERROR}{e}"))))
    }

    /// What an MCP server's tool gave, with the keys rein knows hidden. One
    /// too long to send whole is cut, and kept whole in a file of rein's in
    /// the temp folder, for the model to read in parts with a command.
    fn servers_said(&self, result: Result<String, String>) -> String {
        let text = self.secrets.hide(result.unwrap_or_else(|e| format!("{ERROR}{e}")));
        if text.len() <= MAX_OUTPUT {
            return text;
        }
        static KEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = KEPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join("rein-mcp").join(format!("result-{}-{n}.txt", std::process::id()));
        let note = match rein_llm::store::write_private(&path, text.as_bytes()) {
            Ok(()) => format!("\n[rein: it's all in {}, {} bytes: read it in parts with a command, like sed -n '1,200p']", path.display(), text.len()),
            Err(_) => String::new(),
        };
        truncate(text) + &note
    }

    async fn read(&self, call_id: &str, arguments: &str) -> Result<String, String> {
        let args: ReadArgs = parse(arguments)?;
        let spot = self.inside_root(&args.path)?;
        let bytes = spot.read().await.map_err(|e| format!("can't read {}: {e}", args.path))?;
        // An image is shown, not read as text.
        if crate::image::kind_of(&bytes).is_some() {
            return self.picture(call_id, bytes, format!("image {}", args.path)).await;
        }
        let text = String::from_utf8(bytes).map_err(|_| format!("{} isn't a UTF-8 text file", args.path))?;
        lines(&text, &args)
    }

    async fn grep(&self, arguments: &str) -> Result<String, String> {
        let args: GrepArgs = parse(arguments)?;
        let (root, start) = self.search_root(args.path.as_deref())?;
        let sandbox = self.sandbox.clone();
        // A search of a big project takes a while: off the loop's thread,
        // so Esc can still stop the turn.
        tokio::task::spawn_blocking(move || {
            let grep = search::Grep {
                pattern: &args.pattern,
                start: &start,
                glob: args.glob.as_deref(),
                ignore_case: args.ignore_case,
                files_only: args.output == Some(GrepOutput::Files),
                context: args.context.unwrap_or(0),
            };
            search::grep(&root, &grep, &|path: &Path| sandbox.places().hides(path))
        })
        .await
        .map_err(|e| format!("the search failed: {e}"))?
    }

    async fn glob(&self, arguments: &str) -> Result<String, String> {
        let args: GlobArgs = parse(arguments)?;
        let (root, start) = self.search_root(args.path.as_deref())?;
        if !start.is_dir() {
            return Err(format!("{} isn't a folder", args.path.unwrap_or_default()));
        }
        let sandbox = self.sandbox.clone();
        tokio::task::spawn_blocking(move || search::glob(&root, &args.pattern, &start, &|path: &Path| sandbox.places().hides(path)))
            .await
            .map_err(|e| format!("the search failed: {e}"))?
    }

    /// The project, resolved, and where in it to search: `path`, or all of it.
    fn search_root(&self, path: Option<&str>) -> Result<(PathBuf, PathBuf), String> {
        let root = self.root.canonicalize().map_err(|e| format!("can't open the project directory: {e}"))?;
        let start = match path.map(str::trim).filter(|p| !p.is_empty() && *p != ".") {
            Some(path) => self.inside_root(path)?.full(),
            None => root.clone(),
        };
        Ok((root, start))
    }

    /// Resolves `path`, following symlinks, and refuses anything outside the
    /// project, so the model can't read, say, your credentials in ~/.config,
    /// or what the sandbox hides inside it: a project that is your home
    /// folder still has your keys in it.
    fn inside_root(&self, path: &str) -> Result<Spot, String> {
        let root = self.root.canonicalize().map_err(|e| format!("can't open the project directory: {e}"))?;
        let full = root.join(path).canonicalize().map_err(|e| format!("can't read {path}: {e}"))?;
        self.spot(path, root, &full)
    }

    /// Where `full`, resolved, is in `root`, unless it's outside it or hidden.
    fn spot(&self, path: &str, root: PathBuf, full: &Path) -> Result<Spot, String> {
        let Ok(rel) = full.strip_prefix(&root) else { return Err(format!("{path} is outside the project")) };
        if self.sandbox.places().hides(full) {
            return Err(format!("{path} holds keys or logins, which rein keeps out of reach: leave it"));
        }
        Ok(Spot { rel: rel.to_owned(), root })
    }

    /// Where `path` may be written: inside the project, even for a file or
    /// folders that don't exist yet. The deepest part that does exist is
    /// resolved, following symlinks, and must be inside the project.
    fn writable(&self, path: &str) -> Result<Spot, String> {
        let root = self.root.canonicalize().map_err(|e| format!("can't open the project directory: {e}"))?;
        if Path::new(path).components().any(|c| c == Component::ParentDir) {
            return Err(format!("{path} contains `..`; use a path inside the project"));
        }
        let full = root.join(path);
        // Walk up to the deepest part that exists. `symlink_metadata` doesn't
        // follow links, so a link pointing nowhere still counts as existing
        // and gets resolved (and refused) below.
        let mut existing = full.as_path();
        let mut missing = Vec::new();
        while existing.symlink_metadata().is_err() {
            missing.push(existing.file_name().ok_or_else(|| format!("{path} isn't a file path"))?);
            existing = existing.parent().ok_or_else(|| format!("{path} isn't a file path"))?;
        }
        let resolved = existing.canonicalize().map_err(|e| format!("can't write {path}: {e}"))?;
        if !resolved.starts_with(&root) {
            return Err(format!("{path} is outside the project"));
        }
        let full = missing.iter().rev().fold(resolved, |dir, name| dir.join(name));
        self.spot(path, root, &full)
    }

    async fn write(&self, arguments: &str) -> Result<String, String> {
        let args: WriteArgs = parse(arguments)?;
        no_hidden_key(&args.content)?;
        let spot = self.writable(&args.path)?;
        let existed = spot.full().exists();
        self.changing(&spot, spot.write(args.content.clone().into_bytes())).await.map_err(|e| format!("can't write {}: {e}", args.path))?;
        let verb = if existed { "replaced" } else { "created" };
        let lines = args.content.lines().count();
        Ok(format!("{verb} {} ({lines} line{})", args.path, if lines == 1 { "" } else { "s" }))
    }

    async fn edit(&self, arguments: &str) -> Result<String, String> {
        let args: EditArgs = parse(arguments)?;
        no_hidden_key(&args.new_text)?;
        let spot = self.inside_root(&args.path)?;
        let bytes = spot.read().await.map_err(|e| format!("can't read {}: {e}", args.path))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{} isn't a UTF-8 text file", args.path))?;
        let edited = replace(&text, &args.old_text, &args.new_text, args.replace_all).map_err(|e| format!("{e} in {}", args.path))?;
        self.changing(&spot, spot.write(edited.text.into_bytes())).await.map_err(|e| format!("can't write {}: {e}", args.path))?;
        let times = if edited.count == 1 { "1 place".to_owned() } else { format!("{} places", edited.count) };
        Ok(format!("edited {}: replaced {times}", args.path))
    }

    /// Does `change` to the file at `spot`, kept for `/undo` before and after.
    async fn changing<T>(&self, spot: &Spot, change: impl std::future::Future<Output = T>) -> T {
        let rel = spot.rel.to_string_lossy().into_owned();
        let before = rel.clone();
        self.on_undo(move |undo, _| undo.before_one(&before)).await;
        let done = change.await;
        self.on_undo(move |undo, _| undo.after_one(&rel)).await;
        done
    }

    async fn bash(&self, arguments: &str, how: Run) -> Result<String, String> {
        let args: BashCall = parse(arguments)?;
        // What it does to the project's files is followed for `/undo`,
        // unless its walls keep it from changing any.
        let follows = !matches!(how, Run::Sandboxed(walls) if !walls.write);
        *locked(&self.watched) = None;
        if follows && let Some(off) = self.on_undo(|undo, hides| undo.before_all(hides)).await {
            *locked(&self.watched) = Some(Watched::NotHere(off));
        }
        let ran = self.command(&args, how).await;
        if follows {
            let changed = self.on_undo(|undo, hides| undo.after_all(hides)).await;
            if !changed.is_empty() {
                *locked(&self.watched) = Some(Watched::Changed(changed));
            }
        }
        // What one in the background changes is rein's doing too: the next
        // look keeps it in the turn it's seen in.
        if self.jobs.writing() {
            self.on_undo(|undo, _| undo.still_changing()).await;
        }
        ran
    }

    async fn command(&self, args: &BashCall, how: Run) -> Result<String, String> {
        let timeout = args.timeout_seconds.map_or(self.timeout, |s| Duration::from_secs(s).clamp(Duration::from_secs(1), MAX_TIMEOUT));
        if args.background() && self.jobs.running() >= background::MOST {
            return Err(self.too_many());
        }
        // Where the sandbox can't keep a command from what runs code later,
        // that's noted now and put back once it's done, or dropped (Esc).
        // Before the command, so it's dropped after its group is stopped.
        let guard = match how {
            Run::Sandboxed(walls) => self.sandbox.guard(walls)?,
            Run::Unsandboxed => None,
        };
        let mut command = match how {
            // Never run unconfined what was meant to be confined.
            Run::Sandboxed(walls) => self.sandbox.wrap(&args.command, walls)?,
            Run::Unsandboxed => {
                let mut command = tokio::process::Command::new("sh");
                // Your whole environment, keys rein kept for itself included.
                command.arg("-c").arg(&args.command).envs(sandbox::secrets::all().iter().map(|(name, value)| (name, value)));
                command
            }
        };
        command.current_dir(&self.root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        // A group of its own, so stopping it stops what it started too.
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(|e| format!("can't start sh: {e}"))?;
        let group = sandbox::Group::new(child.id());
        let id = self.jobs.follow(Started { child, group, guard, command: args.command.clone(), how });
        // Given up on, or the turn stopped: dropped, and with it everything
        // the command started.
        let waiting = Foreground::new(&self.jobs, id);
        if args.background() {
            waiting.keep();
            self.jobs.to_background(id, 0);
            return Ok(format!(
                "{STARTED}{id}. You'll be told when it ends, so don't poll or sleep: read what it prints, or wait for a line like a \
                 server's \"ready\", with background."
            ));
        }
        let budget = MAX_OUTPUT - 2000;
        let moved = tokio::select! {
            settled = self.jobs.settled(id) => (settled == background::Settled::Left).then_some(Moved::Left),
            () = tokio::time::sleep(timeout) => Some(Moved::Timeout),
            () = self.jobs.detached() => Some(Moved::You),
        };
        let Some(moved) = moved else {
            waiting.keep();
            let Some((text, exit, restored)) = self.jobs.finish(id, budget) else { return Err("the command was lost".to_owned()) };
            return Ok(result_of(text, exit, how, &restored));
        };
        if moved == Moved::Timeout && self.jobs.running() >= background::MOST {
            drop(waiting);
            return Err(format!("the command took longer than {timeout:?} and was stopped: {}", self.too_many()));
        }
        waiting.keep();
        let (text, exit) = self.jobs.to_background(id, budget);
        let status = exit.map(|exit| exit.line() + "\n").unwrap_or_default();
        let why = match moved {
            Moved::Left => "it ended, but what it started still runs (a server behind &, say), so rein keeps it. It stops when the \
                            conversation ends, or when you stop it with background; you'll be told if it ends before."
                .to_owned(),
            Moved::Timeout => format!(
                "it's still running after {timeout:?}, so it goes on there rather than being stopped. You'll be told when it ends; \
                 read what it prints, or wait for a line, with background, or stop it."
            ),
            Moved::You => "the user moved it there. You'll be told when it ends; read what it prints, or wait for a line, with \
                           background, or stop it."
                .to_owned(),
        };
        let note = format!("{MOVED}{id}: {why}]");
        Ok(format!("{text}{}{status}{note}", if text.is_empty() || text.ends_with('\n') { "" } else { "\n" }))
    }

    /// Why no more commands may go to the background, with what's there.
    fn too_many(&self) -> String {
        let running: Vec<String> = self.jobs.list().into_iter().filter(|listed| listed.ended.is_none()).map(|listed| format!("{} ({})", listed.id, listed.command)).collect();
        format!("{} commands already run in the background, the most there may be: stop one first. They are: {}", background::MOST, running.join(", "))
    }
}

/// How `bash` answers a command started in the background, before its name.
pub const STARTED: &str = "Started in the background as ";

/// How the note starts that says a command went on in the background part
/// way through, before its name.
pub const MOVED: &str = "[In the background as ";

/// The name of the background command that `output`, a `bash` result,
/// says it started or moved there; `None` for one that ran in the
/// foreground.
pub fn in_background(output: &str) -> Option<&str> {
    let rest = output.strip_prefix(STARTED).or_else(|| output.lines().find_map(|line| line.strip_prefix(MOVED)))?;
    let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Why a command waited for in the foreground went on in the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moved {
    /// It ended, but what it started runs on.
    Left,
    /// It ran past its timeout.
    Timeout,
    /// You pressed Ctrl-B.
    You,
}

/// A finished command's result: what it printed, how it ended, and notes
/// of its walls, where they may be why it failed, and of what the sandbox
/// put back.
fn result_of(text: String, exit: background::Exit, how: Run, restored: &[String]) -> String {
    let mut text = text;
    let mut note = match how {
        Run::Sandboxed(walls) if exit.failed() => format!("\n{}", walls.note()),
        _ => String::new(),
    };
    if !restored.is_empty() {
        note.push_str(&format!("\n[The sandbox here can't keep a command from files that run code later, so rein put them back as they were: {}.]", restored.join(", ")));
    }
    text.push_str(&exit.line());
    text + &note
}

/// `mutex`'s guard: one a panic left behind still holds what it held.
fn locked<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reads all of `pipe`, keeping the first `MAX_OUTPUT` bytes: a command
/// that prints gigabytes costs no more memory than one that prints enough.
/// Returns what was kept, and how much there was.
pub(crate) async fn read_capped(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> (Vec<u8>, usize) {
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else { return (Vec::new(), 0) };
    let (mut kept, mut total, mut buf) = (Vec::new(), 0, [0u8; 8192]);
    while let Ok(n @ 1..) = pipe.read(&mut buf).await {
        total += n;
        let room = MAX_OUTPUT.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    (kept, total)
}

struct Edited {
    text: String,
    count: usize,
}

/// Replaces `old` with `new` in a file's text. Matching ignores the file's
/// line endings and byte-order mark, and the result keeps both, so the
/// model never has to know about either.
fn replace(file: &str, old: &str, new: &str, replace_all: bool) -> Result<Edited, String> {
    let (bom, body) = match file.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", file),
    };
    let crlf = body.contains("\r\n");
    let lf = |s: &str| s.replace("\r\n", "\n");
    let (body, old, new) = (lf(body), lf(old), lf(new));
    if old.is_empty() {
        return Err("old_text is empty; to create a file, use write".into());
    }
    if old == new {
        return Err("old_text and new_text are the same".into());
    }
    let count = body.matches(&old).count();
    match count {
        0 => return Err("old_text wasn't found; it must match exactly, spaces and indentation included. Read the file and copy the text".into()),
        1 => {}
        n if !replace_all => return Err(format!("old_text appears {n} times; add surrounding lines to make it unique, or set replace_all")),
        _ => {}
    }
    let mut text = body.replace(&old, &new);
    if crlf {
        text = text.replace('\n', "\r\n");
    }
    Ok(Edited { text: format!("{bom}{text}"), count })
}

pub(crate) fn parse<T: DeserializeOwned>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("bad arguments: {e}"))
}

/// A JSON Schema object in the form strict mode wants: every property
/// required, nothing extra allowed.
pub(crate) fn object(properties: Value) -> Value {
    let required: Vec<String> = properties.as_object().map(|p| p.keys().cloned().collect()).unwrap_or_default();
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// The lines of `text` that `args` asks for, as many whole lines as fit in
/// one output. Anything short of the whole file ends with a note saying
/// which lines these are and where to continue, so the model can page
/// through a long file instead of losing its end.
fn lines(text: &str, args: &ReadArgs) -> Result<String, String> {
    let all: Vec<&str> = text.split_inclusive('\n').collect();
    let total = all.len();
    let start = args.start_line.unwrap_or(1).max(1);
    if total == 0 {
        return Ok(String::new());
    }
    if start > total {
        return Err(format!("{} has only {total} lines", args.path));
    }
    let last = args.line_count.map_or(total, |n| (start - 1 + n.max(1)).min(total));
    // Leave room for the note, so `truncate` never cuts it off.
    let budget = MAX_OUTPUT - 200;
    let mut out = String::new();
    let mut end = start - 1;
    for line in &all[start - 1..last] {
        // A single line longer than the budget is still returned; `truncate`
        // cuts it.
        if !out.is_empty() && out.len() + line.len() > budget {
            break;
        }
        out.push_str(line);
        end += 1;
    }
    if start == 1 && end == total {
        return Ok(out);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let more = if end < total { format!("; read again with start_line {} for more", end + 1) } else { String::new() };
    out.push_str(&format!("[lines {start}-{end} of {total}{more}]"));
    Ok(out)
}

/// Cuts `text` to `MAX_OUTPUT` bytes, on a character boundary, and says so.
fn truncate(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    let mut cut = MAX_OUTPUT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = text.len() - cut;
    text.truncate(cut);
    text.push_str(&format!("\n[{dropped} more bytes cut]"));
    text
}

/// Refuses `text` that has a hidden key's stand-in in it: written into a
/// file, it would take the place of the key there.
fn no_hidden_key(text: &str) -> Result<(), String> {
    if text.contains(redact::HIDDEN) {
        return Err(format!(
            "the text has {} in it, which stands for a key rein kept from you; written, it would replace the key. Change the file around it, leaving that part as it is",
            redact::HIDDEN
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `call` as rein's tools did before the sandbox: commands unconfined.
    async fn run(tools: &Tools, call: &ToolCall) -> String {
        tools.run(call, Run::Unsandboxed).await
    }

    /// Whether a process whose whole command is `command` is alive (not
    /// just waiting to be reaped). Whole, so a shell that merely mentions it
    /// doesn't count.
    fn running(command: &str) -> bool {
        // A moment for SIGKILL to land.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let ps = std::process::Command::new("ps").args(["-axo", "stat=,command="]).output().unwrap();
        String::from_utf8_lossy(&ps.stdout).lines().any(|line| {
            let (stat, rest) = line.trim_start().split_once(' ').unwrap_or_default();
            !stat.starts_with('Z') && rest.trim() == command
        })
    }

    /// A server's long result is cut, and kept whole, keys hidden, where a
    /// command can read it in parts; a short one is as it was.
    #[test]
    fn a_servers_long_result_is_cut_and_kept_whole_with_keys_hidden() {
        let tools = Tools::new(".").with_secrets(Secrets::new(["FAKE-mcp-long-key".to_owned()]));
        let long = format!("{}FAKE-mcp-long-key end", "x".repeat(MAX_OUTPUT));
        let said = tools.servers_said(Ok(long));
        let (shown, note) = said.split_once("\n[rein: it's all in ").expect("a note says where");
        assert!(shown.len() < MAX_OUTPUT + 100 && shown.ends_with("more bytes cut]"), "{}", &shown[shown.len() - 40..]);
        let path = note.split(", ").next().unwrap();
        let kept = std::fs::read_to_string(path).unwrap();
        assert!(kept.ends_with(&format!("{}{} end", "x", redact::HIDDEN)) && !kept.contains("FAKE-mcp-long-key"));
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions()) & 0o777, 0o600, "only yours to read");
        let _ = std::fs::remove_file(path);
        assert_eq!(tools.servers_said(Err("no such repo".into())), format!("{ERROR}no such repo"));
    }

    fn bash_call(command: &str, timeout: Option<u64>) -> ToolCall {
        ToolCall { call_id: "b".into(), name: "bash".into(), arguments: json!({ "command": command, "timeout_seconds": timeout }).to_string() }
    }

    #[tokio::test]
    async fn a_misspelt_choice_is_an_error_the_model_hears_about() {
        let tools = Tools::new(project("misspelt", &[("a.rs", "fn main() {}\n")]));
        let call = |name: &str, arguments: Value| ToolCall { call_id: "c".into(), name: name.into(), arguments: arguments.to_string() };
        let grep = run(&tools, &call("grep", json!({ "pattern": "main", "path": null, "glob": null, "ignore_case": false, "output": "filez", "context": null }))).await;
        assert!(grep.starts_with("error: bad arguments") && grep.contains("filez"), "{grep}");
        let plan = run(&tools, &call("plan", json!({ "steps": [{ "step": "a", "status": "finished" }] }))).await;
        assert!(plan.starts_with("error: bad arguments") && plan.contains("finished"), "{plan}");
        assert_eq!(run(&tools, &call("plan", json!({ "steps": [{ "step": "a", "status": "in_progress" }] }))).await, "Plan updated: 0 of 1 done.");
    }

    #[tokio::test]
    async fn a_command_past_its_timeout_with_no_room_left_takes_everything_it_started_with_it() {
        let tools = Tools::new(project("timeout-group", &[]));
        for _ in 0..background::MOST {
            let started = run(&tools, &call("bash", json!({ "command": "sleep 41.19", "background": true }))).await;
            assert!(started.starts_with("Started in the background as b"), "{started}");
        }
        let refused = run(&tools, &call("bash", json!({ "command": "sleep 41.19", "background": true }))).await;
        assert!(refused.starts_with("error: 8 commands already run in the background") && refused.contains("b8 (sleep 41.19)"), "{refused}");
        let output = run(&tools, &bash_call("sleep 41.17 & sleep 41.17", Some(1))).await;
        assert!(output.contains("took longer") && output.contains("stop one first"), "{output}");
        assert!(!running("sleep 41.17"), "the sleeps were left running");
        drop(tools);
        assert!(!running("sleep 41.19"), "the background ones were left running");
    }

    #[tokio::test]
    async fn a_command_past_its_timeout_goes_on_in_the_background() {
        let tools = Tools::new(project("timeout-moves", &[])).with_timeout(Duration::from_millis(300));
        let output = run(&tools, &call("bash", json!({ "command": "echo first; sleep 0.8; echo second" }))).await;
        assert!(output.starts_with("first\n[In the background as b1: it's still running after 300ms, so"), "{output}");
        assert_eq!(in_background(&output), Some("b1"));
        assert_eq!(ended(&output), None, "no exit line: it hasn't ended");
        let read = run(&tools, &call("background", json!({ "id": "b1", "action": "read", "until": "sec.nd", "seconds": 5 }))).await;
        assert!(read.starts_with("A line matched: second\n"), "{read}");
        assert!(!read.contains("\nfirst\n"), "what the model was given is not given again: {read}");
        // Ended while the model waited and read: it's not told again.
        let read = run(&tools, &call("background", json!({ "id": "b1", "action": "read", "until": "never", "seconds": 5 }))).await;
        assert!(read.starts_with("It ended before a line matched never.\nb1 (echo first; sleep 0.8; echo second) ended: [exit code 0]\nNothing new"), "{read}");
        assert!(tools.ended().await.is_empty());
    }

    #[tokio::test]
    async fn a_background_command_is_read_waited_for_and_stopped() {
        let tools = Tools::new(project("server", &[]));
        let server = "trap 'echo bye; exit 0' TERM; echo starting; sleep 0.3; echo ready on 3000; while true; do sleep 0.1; done";
        let started = run(&tools, &call("bash", json!({ "command": server, "background": true }))).await;
        assert!(started.starts_with("Started in the background as b1."), "{started}");
        assert_eq!(in_background(&started), Some("b1"));
        assert_eq!(in_background("Started in the foreground\n[exit code 0]"), None);
        let ready = run(&tools, &call("background", json!({ "id": "b1", "action": "read", "until": "ready|listening", "seconds": 5 }))).await;
        assert!(ready.starts_with("A line matched: ready on 3000\nb1 ("), "{ready}");
        assert!(ready.contains("is still running.\nstarting\nready on 3000\n"), "{ready}");
        let nothing = run(&tools, &call("background", json!({ "id": "b1", "action": "read", "until": null, "seconds": null }))).await;
        assert!(nothing.ends_with("is still running.\nNothing new since you last read."), "{nothing}");
        let waited = std::time::Instant::now();
        let none = run(&tools, &call("background", json!({ "id": "b1", "action": "read", "until": "never", "seconds": 1 }))).await;
        assert!(none.starts_with("No line matched never in 1s."), "{none}");
        assert!(waited.elapsed() >= Duration::from_millis(900), "it waited");
        let stopped = run(&tools, &call("background", json!({ "id": "b1", "action": "stop", "until": null, "seconds": null }))).await;
        // Not last: dash goes on to say "Terminated" of the sleep it was in.
        assert!(stopped.contains("was stopped.") && stopped.contains("\nbye\n"), "asked first, so it said goodbye: {stopped}");
        assert!(tools.ended().await.is_empty(), "the model stopped it, so it knows");
        let unknown = run(&tools, &call("background", json!({ "id": "b9", "action": "read", "until": null, "seconds": null }))).await;
        assert_eq!(unknown, "error: there's no background command called b9; there are b1");
        let bad = run(&tools, &call("background", json!({ "id": "b1", "action": "read", "until": "(", "seconds": null }))).await;
        assert!(bad.starts_with("error: until isn't a regular expression"), "{bad}");
    }

    #[tokio::test]
    async fn a_background_command_that_ends_is_told_of_once_with_its_last_lines() {
        let tools = Tools::new(project("told", &[]));
        run(&tools, &call("bash", json!({ "command": "echo one; echo two; exit 3", "background": true }))).await;
        let jobs = tools.jobs().clone();
        tokio::time::timeout(Duration::from_secs(5), jobs.ended()).await.expect("it ended");
        let ended = tools.ended().await;
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].exit, background::Exit::Code(3));
        assert_eq!(ended[0].note, "[rein: the background command b1 (echo one; echo two; exit 3) ended: [exit code 3]. What it printed since you last read:\none\ntwo]");
        assert!(background::is_note(&ended[0].note));
        assert!(tools.ended().await.is_empty(), "told once");
        tokio::time::sleep(Duration::from_millis(600)).await;
        let listed = tools.jobs().list();
        assert_eq!((listed[0].ended, listed[0].last.as_deref()), (Some(background::Exit::Code(3)), Some("two")));
        assert!(listed[0].took < Duration::from_millis(500), "how long it ran, not how long ago it started: {:?}", listed[0].took);
    }

    #[tokio::test]
    async fn what_a_finished_command_leaves_running_is_kept_and_stops_with_the_tools() {
        let tools = Tools::new(project("left", &[]));
        let output = run(&tools, &bash_call("sleep 43.31 >/dev/null 2>&1 & echo started", None)).await;
        assert!(output.starts_with("started\n[exit code 0]\n[In the background as b1: it ended, but what it started still runs"), "{output}");
        assert!(running("sleep 43.31"), "a server started with & keeps running");
        assert_eq!(tools.jobs().running(), 1);
        drop(tools);
        assert!(!running("sleep 43.31"), "it stops with the conversation");
    }

    #[tokio::test]
    async fn a_server_holding_the_output_open_doesnt_hold_up_the_turn() {
        let tools = Tools::new(project("held", &[]));
        let began = std::time::Instant::now();
        let output = run(&tools, &bash_call("sleep 44.41 & echo started", Some(30))).await;
        assert!(began.elapsed() < Duration::from_secs(5), "it waited {:?}", began.elapsed());
        assert!(output.starts_with("started\n[exit code 0]\n"), "{output}");
        assert_eq!(in_background(&output), Some("b1"));
        drop(tools);
        assert!(!running("sleep 44.41"));
    }

    #[tokio::test]
    async fn ctrl_b_moves_the_command_to_the_background() {
        let tools = Tools::new(project("ctrl-b", &[]));
        let jobs = tools.jobs().clone();
        let press = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            jobs.detach();
            std::future::pending::<String>().await
        };
        let command = bash_call("echo going; sleep 45.51", None);
        let output = tokio::select! {
            output = run(&tools, &command) => output,
            never = press => never,
        };
        assert!(output.starts_with("going\n[In the background as b1: the user moved it there."), "{output}");
        assert_eq!(tools.jobs().running(), 1);
        // You stop it: the model hears of it, with your next message.
        tools.jobs().stop_yours("b1").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(300), jobs.ended()).await.is_err(), "it doesn't wake the model");
        let ended = tools.ended().await;
        assert!(ended[0].note.contains("b1 (echo going; sleep 45.51) was stopped by the user."), "{:?}", ended);
        assert!(!running("sleep 45.51"));
    }

    #[tokio::test]
    async fn a_command_whose_turn_is_stopped_takes_everything_it_started_with_it() {
        let tools = Tools::new(project("dropped-group", &[]));
        // The turn's future is dropped mid-command, as Esc does.
        let stopped = tokio::time::timeout(std::time::Duration::from_millis(300), run(&tools, &bash_call("sleep 42.23 & sleep 42.23", None))).await;
        assert!(stopped.is_err());
        assert!(!running("sleep 42.23"), "the sleeps were left running");
    }

    #[tokio::test]
    async fn a_flood_of_output_isnt_all_kept() {
        let tools = Tools::new(project("flood", &[]));
        let output = run(&tools, &bash_call("head -c 20000000 /dev/zero | tr '\\0' x", None)).await;
        assert!(output.len() < 100_000, "{} bytes kept", output.len());
        assert!(output.contains("bytes in between not shown"), "{}", &output[output.len().saturating_sub(200)..]);
        assert!(output.ends_with("x[exit code 0]"), "its end, with how it went, is kept: {}", &output[output.len().saturating_sub(200)..]);
    }

    /// A fresh directory for one test, with `files` written into it.
    fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-tools-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (path, text) in files {
            std::fs::write(dir.join(path), text).unwrap();
        }
        dir
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall { call_id: "call_1".into(), name: name.into(), arguments: arguments.to_string() }
    }

    #[tokio::test]
    async fn read_returns_the_file() {
        let tools = Tools::new(project("read", &[("a.txt", "hello")]));
        assert_eq!(run(&tools, &call("read", json!({"path": "a.txt"}))).await, "hello");
        let missing = run(&tools, &call("read", json!({"path": "nope.txt"}))).await;
        assert!(missing.starts_with("error: can't read nope.txt"), "{missing}");
    }

    #[tokio::test]
    async fn read_pages_through_long_files() {
        let text: String = (1..=5000).map(|n| format!("line {n}\n")).collect();
        let tools = Tools::new(project("pages", &[("long.txt", &text), ("short.txt", "a\nb\nc\n")]));

        let first = run(&tools, &call("read", json!({"path": "long.txt", "start_line": null, "line_count": null}))).await;
        assert!(first.starts_with("line 1\n"));
        assert!(first.len() <= MAX_OUTPUT);
        let note = first.lines().last().unwrap();
        assert!(note.starts_with("[lines 1-") && note.contains(" of 5000; read again with start_line ") && note.ends_with(" for more]"), "{note}");

        let middle = run(&tools, &call("read", json!({"path": "long.txt", "start_line": 4000, "line_count": 3}))).await;
        assert_eq!(middle, "line 4000\nline 4001\nline 4002\n[lines 4000-4002 of 5000; read again with start_line 4003 for more]");

        let end = run(&tools, &call("read", json!({"path": "long.txt", "start_line": 4999, "line_count": null}))).await;
        assert_eq!(end, "line 4999\nline 5000\n[lines 4999-5000 of 5000]");

        assert_eq!(run(&tools, &call("read", json!({"path": "short.txt"}))).await, "a\nb\nc\n");
        let past = run(&tools, &call("read", json!({"path": "short.txt", "start_line": 9, "line_count": null}))).await;
        assert_eq!(past, "error: short.txt has only 3 lines");
    }

    #[tokio::test]
    async fn read_refuses_paths_outside_the_project() {
        let tools = Tools::new(project("outside", &[]));
        // Enough `..` to reach / from any temp dir; extra ones stay at /.
        let climb = format!("{}etc/hosts", "../".repeat(20));
        for path in [climb.as_str(), "/etc/hosts"] {
            let answer = run(&tools, &call("read", json!({ "path": path }))).await;
            assert_eq!(answer, format!("error: {path} is outside the project"));
        }
    }

    #[tokio::test]
    async fn searches_stay_in_the_project() {
        let tools = Tools::new(project("search", &[("a.rs", "fn main() {}\n"), ("b.md", "main ideas\n")]));
        let grep = |path: Value, output: &str| call("grep", json!({"pattern": "main", "path": path, "glob": null, "ignore_case": false, "output": output, "context": null}));
        assert_eq!(run(&tools, &grep(Value::Null, "files")).await, "2 files with matches (2 matching lines)\na.rs (1 match)\nb.md (1 match)");
        assert_eq!(run(&tools, &grep(json!("a.rs"), "lines")).await, "1 match in 1 file\na.rs\n1:fn main() {}");
        assert_eq!(run(&tools, &call("glob", json!({"pattern": "*.md", "path": null}))).await, "1 file\nb.md");
        let climb = format!("{}etc", "../".repeat(20));
        for path in ["/etc", climb.as_str()] {
            assert_eq!(run(&tools, &grep(json!(path), "lines")).await, format!("error: {path} is outside the project"));
        }
        assert_eq!(run(&tools, &call("glob", json!({"pattern": "*", "path": "a.rs"}))).await, "error: a.rs isn't a folder");
    }

    #[tokio::test]
    async fn no_file_tool_reaches_what_the_sandbox_hides_even_in_your_home_folder() {
        // The project is your home folder: your keys are inside it.
        let home = project("home", &[("notes.txt", "mine\n")]);
        for (dir, file) in [(".config/rein", "keys.json"), (".ssh", "id_test")] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
            std::fs::write(home.join(dir).join(file), "sk-test-FAKE\n").unwrap();
        }
        std::os::unix::fs::symlink(home.join(".ssh/id_test"), home.join("innocent.txt")).unwrap();
        let places = crate::sandbox::Places::new(&home, Some(&home), Some(&home.join(".config/rein")));
        let tools = Tools::new(&home).with_sandbox(std::sync::Arc::new(crate::sandbox::Sandbox::assume(places)));
        for path in [".config/rein/keys.json", ".ssh/id_test", "innocent.txt"] {
            let read = run(&tools, &call("read", json!({ "path": path }))).await;
            assert!(read.starts_with("error: ") && read.contains("keys or logins") && !read.contains("FAKE"), "{path}: {read}");
            let edit = run(&tools, &call("edit", json!({"path": path, "old_text": "sk", "new_text": "x", "replace_all": false}))).await;
            assert!(edit.contains("keys or logins"), "{path}: {edit}");
        }
        let planted = run(&tools, &call("write", json!({"path": ".config/rein/providers", "content": "[anthropic]\nurl = http://attacker\n"}))).await;
        assert!(planted.contains("keys or logins"), "{planted}");
        assert!(!home.join(".config/rein/providers").exists());
        let grep = run(&tools, &call("grep", json!({"pattern": "FAKE", "path": null, "glob": null, "ignore_case": false, "output": "files", "context": null}))).await;
        assert!(grep.starts_with("No matches"), "{grep}");
        let glob = run(&tools, &call("glob", json!({"pattern": "*", "path": null}))).await;
        assert!(!glob.contains("keys.json") && !glob.contains("id_test") && glob.contains("notes.txt"), "{glob}");
        assert_eq!(run(&tools, &call("read", json!({"path": "notes.txt"}))).await, "mine\n", "the rest of it is still yours to work in");
    }

    #[tokio::test]
    async fn a_key_in_what_a_tool_says_is_hidden_and_never_written_back() {
        let dir = project("secrets", &[("deploy.env", "TOKEN=sk-test-FAKE-0123\nREGION=eu\n")]);
        let tools = Tools::new(&dir).with_secrets(Secrets::new(["sk-test-FAKE-0123".to_owned()]));
        let read = run(&tools, &call("read", json!({"path": "deploy.env"}))).await;
        assert_eq!(read, format!("TOKEN={}\nREGION=eu\n", redact::HIDDEN));
        // Written back as it was read, the stand-in would replace the key.
        let write = run(&tools, &call("write", json!({"path": "deploy.env", "content": read}))).await;
        assert!(write.starts_with("error: the text has"), "{write}");
        let edit = json!({"path": "deploy.env", "old_text": "REGION=eu", "new_text": format!("TOKEN={}\nREGION=us", redact::HIDDEN), "replace_all": false});
        let edit = run(&tools, &call("edit", edit)).await;
        assert!(edit.starts_with("error: the text has"), "{edit}");
        // A change around it is fine, and the key stays.
        let around = json!({"path": "deploy.env", "old_text": "REGION=eu", "new_text": "REGION=us", "replace_all": false});
        assert_eq!(run(&tools, &call("edit", around)).await, "edited deploy.env: replaced 1 place");
        assert_eq!(std::fs::read_to_string(dir.join("deploy.env")).unwrap(), "TOKEN=sk-test-FAKE-0123\nREGION=us\n");
    }

    #[tokio::test]
    async fn undo_puts_back_the_last_turns_changes() {
        let dir = project("undo", &[("a.txt", "one\n")]);
        let tools = Tools::new(&dir);
        tools.turn("first").await;
        run(&tools, &call("edit", json!({"path": "a.txt", "old_text": "one", "new_text": "two", "replace_all": false}))).await;
        tools.turn("second").await;
        run(&tools, &call("edit", json!({"path": "a.txt", "old_text": "two", "new_text": "three", "replace_all": false}))).await;
        run(&tools, &call("edit", json!({"path": "a.txt", "old_text": "three", "new_text": "four", "replace_all": false}))).await;
        run(&tools, &call("write", json!({"path": "new/b.txt", "content": "made"}))).await;
        tools.turn("a turn that changed nothing").await;

        let listed = tools.undo_list().await;
        assert_eq!(listed.iter().map(|turn| turn.said.as_str()).collect::<Vec<_>>(), ["second", "first"], "the latest first");
        let done = tools.undo(listed[0].turn).await;
        assert_eq!((done.put_back, done.take_away), (vec!["a.txt".to_owned()], vec!["new/b.txt".to_owned()]));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n", "back to before that turn");
        assert!(!dir.join("new/b.txt").exists(), "a created file goes away");
        let listed = tools.undo_list().await;
        assert_eq!(tools.undo(listed[0].turn).await.put_back, ["a.txt"]);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(tools.undo_list().await.is_empty());
    }

    #[tokio::test]
    async fn what_a_command_did_is_undone_too_with_a_folder_for_copies() {
        let dir = project("undo-command", &[("a.txt", "one\n"), ("b.txt", "keep me\n")]);
        let folder = std::env::temp_dir().join(format!("rein-tools-{}-undo-command-copies", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let tools = Tools::new(&dir).with_undo(folder.clone());
        tools.open_undo("s1");
        tools.turn("tidy up").await;
        run(&tools, &call("bash", json!({"command": "rm b.txt; echo two > a.txt; echo new > c.txt"}))).await;
        let Some(Watched::Changed(changed)) = tools.take_watched() else { panic!("the command's changes weren't seen") };
        assert_eq!((changed.changed, changed.made, changed.removed), (vec!["a.txt".to_owned()], vec!["c.txt".to_owned()], vec!["b.txt".to_owned()]));
        assert_eq!(tools.take_watched(), None, "said once");
        // Without a folder for copies, a command isn't followed.
        let bare = Tools::new(&dir);
        bare.turn("x").await;
        run(&bare, &call("bash", json!({"command": "echo three > a.txt"}))).await;
        assert_eq!(bare.take_watched(), None);
        run(&tools, &call("bash", json!({"command": "echo two > a.txt"}))).await;

        let listed = tools.undo_list().await;
        let done = tools.undo(listed[0].turn).await;
        assert_eq!((done.put_back.len(), done.take_away, done.left), (2, vec!["c.txt".to_owned()], Vec::<String>::new()));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "keep me\n");
        assert!(!dir.join("c.txt").exists());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn bash_returns_output_and_exit_code() {
        let tools = Tools::new(project("bash", &[]));
        let answer = run(&tools, &call("bash", json!({"command": "echo hi; echo oops >&2; exit 3"}))).await;
        assert_eq!(answer, "hi\noops\n[exit code 3]");
    }

    #[tokio::test]
    async fn a_command_meant_for_the_sandbox_never_runs_without_one() {
        let root = project("no-sandbox", &[]);
        let tools = Tools::new(&root);
        let look = Run::Sandboxed(Access::Look.walls().unwrap());
        let answer = tools.run(&bash_call("touch ran.txt", None), look).await;
        assert_eq!(answer, "error: there's no sandbox here to run it in");
        assert!(!root.join("ran.txt").exists());
    }

    #[tokio::test]
    async fn a_sandboxed_command_that_fails_says_what_its_walls_were() {
        let root = project("walls", &[("a.txt", "hi\n")]);
        let sandbox = sandbox::Sandbox::detect(sandbox::Places::new(&root, None, None)).await;
        if !sandbox.available() {
            return;
        }
        let tools = Tools::new(&root).with_sandbox(std::sync::Arc::new(sandbox));
        let look = Run::Sandboxed(Access::Look.walls().unwrap());
        assert_eq!(tools.run(&bash_call("cat a.txt", None), look).await, "hi\n[exit code 0]", "a command that worked says nothing more");
        let answer = tools.run(&bash_call("echo x > a.txt", None), look).await;
        // Seatbelt and Landlock refuse in different words, and sh's exit code
        // differs too: what's rein's is the note.
        assert!(!answer.contains("[exit code 0]") && answer.contains("]\n[It ran in the sandbox with look access"), "{answer}");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hi\n");
    }

    #[tokio::test]
    async fn a_command_its_walls_keep_from_writing_isnt_looked_at() {
        let root = project("undo-walls", &[("a.txt", "hi\n")]);
        let sandbox = sandbox::Sandbox::detect(sandbox::Places::new(&root, None, None)).await;
        if !sandbox.available() {
            return;
        }
        let folder = std::env::temp_dir().join(format!("rein-tools-{}-undo-walls-copies", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let tools = Tools::new(&root).with_sandbox(std::sync::Arc::new(sandbox)).with_undo(folder.clone());
        tools.open_undo("s1");
        tools.turn("look").await;
        tools.run(&bash_call("cat a.txt", None), Run::Sandboxed(Access::Look.walls().unwrap())).await;
        assert!(!folder.exists(), "nothing kept for a command that can only look");
        tools.run(&bash_call("echo changed > a.txt", None), Run::Sandboxed(Access::Write.walls().unwrap())).await;
        assert!(matches!(tools.take_watched(), Some(Watched::Changed(changed)) if changed.changed == ["a.txt"]));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn how_a_command_ended_is_found_past_reins_notes() {
        assert_eq!(ended("ok\n[exit code 0]"), Some((1, Ended::Code("0"))));
        assert_eq!(ended("x\n[exit code 1]\n[It ran in the sandbox with look access.]\n[rein: that was the last step.]"), Some((1, Ended::Code("1"))));
        assert_eq!(ended("[stopped by a signal]"), Some((0, Ended::Signal)));
        // What the command printed doesn't count once its own line follows.
        assert_eq!(ended("[exit code 0]\nthen it failed\n[exit code 2]"), Some((2, Ended::Code("2"))));
        assert_eq!(ended("error: there's no sandbox here to run it in"), None);
    }

    #[test]
    fn a_bash_call_says_what_it_needs() {
        let call = |arguments: Value| ToolCall { call_id: "c".into(), name: "bash".into(), arguments: arguments.to_string() };
        let fetch = BashCall::of(&call(json!({ "command": "cargo fetch", "access": "network", "reason": "  to download the crates " }))).unwrap();
        assert_eq!((fetch.command(), fetch.access(), fetch.reason()), ("cargo fetch", Access::Network, Some("to download the crates")));
        // Saying nothing, or null, is the least there is.
        let plain = BashCall::of(&call(json!({ "command": "ls", "access": null, "reason": "" }))).unwrap();
        assert_eq!((plain.access(), plain.reason()), (Access::Look, None));
        assert!(BashCall::of(&call(json!({ "command": "ls", "access": "all" }))).is_none());
        assert!(BashCall::of(&ToolCall { name: "read".into(), ..call(json!({ "command": "ls" })) }).is_none());
    }

    #[tokio::test]
    async fn bash_waits_as_long_as_its_asked_to() {
        // Asking for longer works, within the limit.
        let tools = Tools::new(project("slow-ok", &[])).with_timeout(Duration::from_millis(200));
        let answer = run(&tools, &call("bash", json!({"command": "sleep 1.2; echo done", "timeout_seconds": 3}))).await;
        assert_eq!(answer, "done\n[exit code 0]");
    }

    #[tokio::test]
    async fn mistakes_come_back_as_text() {
        let tools = Tools::new(project("mistakes", &[]));
        assert_eq!(run(&tools, &call("rm", json!({}))).await, "error: there is no tool called rm");
        let bad = run(&tools, &call("read", json!({"file": "a.txt"}))).await;
        assert!(bad.starts_with("error: bad arguments: missing field `path`"), "{bad}");
    }

    #[tokio::test]
    async fn write_creates_files_and_folders_inside_the_project() {
        let dir = project("write", &[]);
        let tools = Tools::new(&dir);
        let created = run(&tools, &call("write", json!({"path": "src/new/a.txt", "content": "one\ntwo\n"}))).await;
        assert_eq!(created, "created src/new/a.txt (2 lines)");
        assert_eq!(std::fs::read_to_string(dir.join("src/new/a.txt")).unwrap(), "one\ntwo\n");
        let replaced = run(&tools, &call("write", json!({"path": "src/new/a.txt", "content": "three"}))).await;
        assert_eq!(replaced, "replaced src/new/a.txt (1 line)");
    }

    #[tokio::test]
    async fn write_refuses_paths_outside_the_project() {
        let dir = project("write-outside", &[]);
        let outside = std::env::temp_dir().join(format!("rein-tools-{}-elsewhere", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let tools = Tools::new(&dir);
        for (path, error) in [
            ("../escape.txt", "error: ../escape.txt contains `..`; use a path inside the project"),
            ("/tmp/escape.txt", "error: /tmp/escape.txt is outside the project"),
            ("link/escape.txt", "error: link/escape.txt is outside the project"),
        ] {
            assert_eq!(run(&tools, &call("write", json!({"path": path, "content": "x"}))).await, error);
        }
        assert!(!outside.join("escape.txt").exists());
    }

    #[tokio::test]
    async fn edit_replaces_exactly_one_match() {
        let dir = project("edit", &[("a.rs", "fn a() {}\nfn b() {}\n")]);
        let tools = Tools::new(&dir);
        let edit = |old: &str, new: &str, all: bool| call("edit", json!({"path": "a.rs", "old_text": old, "new_text": new, "replace_all": all}));
        assert_eq!(run(&tools, &edit("fn b() {}", "fn c() {}", false)).await, "edited a.rs: replaced 1 place");
        assert_eq!(std::fs::read_to_string(dir.join("a.rs")).unwrap(), "fn a() {}\nfn c() {}\n");

        let twice = run(&tools, &edit("() {}", "() { todo!() }", false)).await;
        assert_eq!(twice, "error: old_text appears 2 times; add surrounding lines to make it unique, or set replace_all in a.rs");
        assert_eq!(run(&tools, &edit("() {}", "() { todo!() }", true)).await, "edited a.rs: replaced 2 places");

        let missing = run(&tools, &edit("fn z() {}", "fn y() {}", false)).await;
        assert!(missing.starts_with("error: old_text wasn't found"), "{missing}");
    }

    #[test]
    fn edits_keep_line_endings_and_the_byte_order_mark() {
        let edited = replace("\u{feff}one\r\ntwo\r\n", "one\ntwo", "uno\ndos", false).unwrap();
        assert_eq!(edited.text, "\u{feff}uno\r\ndos\r\n");
        assert_eq!(replace("a\n", "a", "a", false).err().as_deref(), Some("old_text and new_text are the same"));
    }

    #[test]
    fn long_output_is_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_OUTPUT); // two bytes each
        let cut = truncate(text);
        assert!(cut.ends_with(&format!("\n[{} more bytes cut]", MAX_OUTPUT)));
        assert!(cut.len() < MAX_OUTPUT + 40);
    }
}
