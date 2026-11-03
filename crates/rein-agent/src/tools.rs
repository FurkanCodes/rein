//! The tools the model can call. Each takes its arguments as JSON text and
//! returns text for the model, also when something goes wrong: an error the
//! model can read is one it can work around.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rein_llm::{Tool, ToolCall};

use crate::sandbox::{self, Access, Run};
use crate::search;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The most a tool sends back. Everything the model sees is resent with
/// every later request, so long outputs get cut.
const MAX_OUTPUT: usize = 30_000;

pub struct Tools {
    /// The project directory: `read` can't see outside it, `bash` starts in it.
    root: PathBuf,
    /// How long a command may run before it's stopped.
    timeout: Duration,
    /// What files looked like before rein changed them, one list per turn,
    /// for `undo`.
    checkpoints: std::sync::Mutex<Vec<Checkpoint>>,
    /// What holds commands in: `bash` runs in it, when it's told to.
    sandbox: sandbox::Shared,
}

/// Each file a turn changed, as it was before: its bytes, or `None` if the
/// turn created it.
type Checkpoint = Vec<(PathBuf, Option<Vec<u8>>)>;

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

const SIGNAL_LINE: &str = "[stopped by a signal]";

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
    /// The other reins on this computer, and reaching them: the user's
    /// side of rein does these, see [`crate::peers`].
    Peers,
    Message,
    Delegate,
}

impl ToolName {
    pub const ALL: [ToolName; 11] = [Self::Read, Self::Grep, Self::Glob, Self::Write, Self::Edit, Self::Plan, Self::Bash, Self::Recall, Self::Peers, Self::Message, Self::Delegate];

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
            Self::Peers => "peers",
            Self::Message => "message",
            Self::Delegate => "delegate",
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
        matches!(self, Self::Peers | Self::Message | Self::Delegate)
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
        Tools { root: root.into(), timeout: Duration::from_secs(60), checkpoints: Default::default(), sandbox: Default::default() }
    }

    pub fn with_sandbox(mut self, sandbox: sandbox::Shared) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Starts a new turn: file changes from now on are undone together.
    pub fn checkpoint(&self) {
        self.saved().push(Vec::new());
    }

    /// Forgets the file changes there are to undo: they were another
    /// conversation's.
    pub fn forget_changes(&self) {
        self.saved().clear();
    }

    /// Puts back the files the latest turn that changed any wrote or edited,
    /// and returns their paths. Commands it ran aren't undone: rein can't
    /// know what they did.
    pub fn undo(&self) -> Result<Vec<String>, String> {
        let mut saved = self.saved();
        while saved.last().is_some_and(Vec::is_empty) {
            saved.pop();
        }
        let Some(checkpoint) = saved.pop() else { return Ok(Vec::new()) };
        // The paths were resolved, symlinks and all; so is the root here.
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        let mut undone = Vec::new();
        // Newest first, so a file changed twice ends up as it was at first.
        for (path, before) in checkpoint.into_iter().rev() {
            match &before {
                Some(bytes) => std::fs::write(&path, bytes),
                None => std::fs::remove_file(&path),
            }
            .map_err(|e| format!("can't put back {}: {e}", path.display()))?;
            undone.push(path.strip_prefix(&root).unwrap_or(&path).display().to_string());
        }
        undone.sort();
        undone.dedup();
        Ok(undone)
    }

    fn saved(&self) -> std::sync::MutexGuard<'_, Vec<Checkpoint>> {
        self.checkpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Keeps `path` as it is now, before it changes.
    fn remember(&self, path: &Path) {
        let before = std::fs::read(path).ok();
        if let Some(checkpoint) = self.saved().last_mut() {
            checkpoint.push((path.to_owned(), before));
        }
    }

    /// The project folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// What the model is told about `bash`. It's the same in every mode,
    /// so that changing modes doesn't change the start of every request
    /// and lose the prompt cache: what a mode allows, a refusal says.
    fn bash_description(&self) -> &'static str {
        if self.sandbox.available() {
            "Run a shell command in the project root and return its output and exit code. Commands run in a \
             sandbox, so say what each needs in access, and ask for no more: look for one that only reads, write \
             for one that changes files in the project (builds and tests usually do), network for one that also \
             uses the internet, and outside for one that must write elsewhere (installing packages into a cache in \
             the home folder), or use docker, ssh or a terminal. Depending on the mode, more access may need the \
             user's approval, or be refused. Even in the sandbox, secrets in the home folder can't be read and \
             files that run code later (.git/hooks, .git/config, .rein) can't be changed. Commands are stopped \
             after 60 seconds unless you ask for longer, up to 600, for slow builds and test suites."
        } else {
            "Run a shell command in the project root and return its output and exit code. The user approves \
             each command before it runs; say what it needs in access. Commands are stopped after 60 seconds \
             unless you ask for longer, up to 600, for slow builds and test suites."
        }
    }

    /// What the model is told about each tool.
    pub fn specs(&self) -> Vec<Tool> {
        vec![
            Tool {
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
                    "reason": { "type": ["string", "null"], "description": "For network or outside: why, in a few words the user will read. Null otherwise." }
                })),
            },
        ]
    }

    /// Runs one call and returns the text to send back to the model. `how`
    /// is how a command runs, as the permissions decided: in the sandbox,
    /// within its walls, or not. The other tools keep to the project
    /// themselves, and run as they always do.
    pub async fn run(&self, call: &ToolCall, how: Run) -> String {
        let result = match ToolName::of(call) {
            Some(ToolName::Read) => self.read(&call.arguments).await,
            Some(ToolName::Grep) => self.grep(&call.arguments).await,
            Some(ToolName::Glob) => self.glob(&call.arguments).await,
            Some(ToolName::Bash) => self.bash(&call.arguments, how).await,
            Some(ToolName::Write) => self.write(&call.arguments).await,
            Some(ToolName::Edit) => self.edit(&call.arguments).await,
            // The checklist is for showing; the model keeps it in its own words.
            Some(ToolName::Plan) => parse::<PlanArgs>(&call.arguments).map(|plan| {
                let done = plan.steps.iter().filter(|step| step.status == StepStatus::Done).count();
                format!("Plan updated: {done} of {} done.", plan.steps.len())
            }),
            // `recall` is the agent's own: it searches the conversation. The
            // peers are reached by whoever runs the agent.
            Some(ToolName::Recall | ToolName::Peers | ToolName::Message | ToolName::Delegate) | None => Err(format!("there is no tool called {}", call.name)),
        };
        truncate(result.unwrap_or_else(|e| format!("{ERROR}{e}")))
    }

    async fn read(&self, arguments: &str) -> Result<String, String> {
        let args: ReadArgs = parse(arguments)?;
        let path = self.inside_root(&args.path)?;
        let bytes = tokio::fs::read(&path).await.map_err(|e| format!("can't read {}: {e}", args.path))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{} isn't a UTF-8 text file", args.path))?;
        lines(&text, &args)
    }

    async fn grep(&self, arguments: &str) -> Result<String, String> {
        let args: GrepArgs = parse(arguments)?;
        let (root, start) = self.search_root(args.path.as_deref())?;
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
            search::grep(&root, &grep)
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
        tokio::task::spawn_blocking(move || search::glob(&root, &args.pattern, &start)).await.map_err(|e| format!("the search failed: {e}"))?
    }

    /// The project, resolved, and where in it to search: `path`, or all of it.
    fn search_root(&self, path: Option<&str>) -> Result<(PathBuf, PathBuf), String> {
        let root = self.root.canonicalize().map_err(|e| format!("can't open the project directory: {e}"))?;
        let start = match path.map(str::trim).filter(|p| !p.is_empty() && *p != ".") {
            Some(path) => self.inside_root(path)?,
            None => root.clone(),
        };
        Ok((root, start))
    }

    /// Resolves `path`, following symlinks, and refuses anything outside the
    /// project, so the model can't read, say, your credentials in ~/.config.
    fn inside_root(&self, path: &str) -> Result<PathBuf, String> {
        let root = self.root.canonicalize().map_err(|e| format!("can't open the project directory: {e}"))?;
        let full = root.join(path).canonicalize().map_err(|e| format!("can't read {path}: {e}"))?;
        if full.starts_with(&root) { Ok(full) } else { Err(format!("{path} is outside the project")) }
    }

    /// Where `path` may be written: inside the project, even for a file or
    /// folders that don't exist yet. The deepest part that does exist is
    /// resolved, following symlinks, and must be inside the project.
    fn writable(&self, path: &str) -> Result<PathBuf, String> {
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
        Ok(missing.iter().rev().fold(resolved, |dir, name| dir.join(name)))
    }

    async fn write(&self, arguments: &str) -> Result<String, String> {
        let args: WriteArgs = parse(arguments)?;
        let path = self.writable(&args.path)?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| format!("can't create the folders for {}: {e}", args.path))?;
        }
        let existed = path.exists();
        self.remember(&path);
        tokio::fs::write(&path, &args.content).await.map_err(|e| format!("can't write {}: {e}", args.path))?;
        let verb = if existed { "replaced" } else { "created" };
        let lines = args.content.lines().count();
        Ok(format!("{verb} {} ({lines} line{})", args.path, if lines == 1 { "" } else { "s" }))
    }

    async fn edit(&self, arguments: &str) -> Result<String, String> {
        let args: EditArgs = parse(arguments)?;
        let path = self.inside_root(&args.path)?;
        let bytes = tokio::fs::read(&path).await.map_err(|e| format!("can't read {}: {e}", args.path))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{} isn't a UTF-8 text file", args.path))?;
        let edited = replace(&text, &args.old_text, &args.new_text, args.replace_all).map_err(|e| format!("{e} in {}", args.path))?;
        self.remember(&path);
        tokio::fs::write(&path, edited.text).await.map_err(|e| format!("can't write {}: {e}", args.path))?;
        let times = if edited.count == 1 { "1 place".to_owned() } else { format!("{} places", edited.count) };
        Ok(format!("edited {}: replaced {times}", args.path))
    }

    async fn bash(&self, arguments: &str, how: Run) -> Result<String, String> {
        let args: BashCall = parse(arguments)?;
        let timeout = args.timeout_seconds.map_or(self.timeout, |s| Duration::from_secs(s).clamp(Duration::from_secs(1), MAX_TIMEOUT));
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
        let mut child = command.spawn().map_err(|e| format!("can't start sh: {e}"))?;
        let group = Group(child.id());
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let run = async { tokio::join!(read_capped(stdout), read_capped(stderr), child.wait()) };
        // Given up on, or the turn stopped: `group` is dropped, and with it
        // everything the command started.
        let Ok(((out, out_total), (err, err_total), status)) = tokio::time::timeout(timeout, run).await else {
            return Err(format!("the command took longer than {timeout:?} and was stopped"));
        };
        let status = status.map_err(|e| format!("the command failed to run: {e}"))?;
        // Done: what it put in the background on purpose, like a server,
        // is its own business.
        group.disarm();
        let mut text = String::from_utf8_lossy(&out).into_owned();
        text.push_str(&String::from_utf8_lossy(&err));
        let dropped = (out_total - out.len()) + (err_total - err.len());
        if dropped > 0 {
            text.push_str(&format!("\n[{dropped} more bytes of output weren't kept]\n"));
        }
        // A sandboxed command that failed says what its walls were: they may
        // be why, and the model can ask for more.
        let mut note = match how {
            Run::Sandboxed(walls) if !status.success() => format!("\n{}", walls.note()),
            _ => String::new(),
        };
        let restored = guard.map(sandbox::Guard::finish).unwrap_or_default();
        if !restored.is_empty() {
            note.push_str(&format!("\n[The sandbox here can't keep a command from files that run code later, so rein put them back as they were: {}.]", restored.join(", ")));
        }
        let status = match status.code() {
            Some(code) => format!("[exit code {code}]"),
            None => SIGNAL_LINE.to_owned(),
        };
        Ok(format!("{text}{status}{note}"))
    }
}

/// A command's process group, which stops when this is dropped before the
/// command is done: on a timeout, or when the turn's future is dropped
/// (Esc). Killing the shell alone would leave what it started running.
struct Group(Option<u32>);

impl Group {
    /// The command finished: nothing to stop.
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: killpg only sends a signal. The group is the one the
            // command was started in (`process_group(0)` makes its ID the
            // shell's), and the shell hasn't been waited for yet, so the ID
            // can't have been reused.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
}

/// Reads all of `pipe`, keeping the first `MAX_OUTPUT` bytes: a command
/// that prints gigabytes costs no more memory than one that prints enough.
/// Returns what was kept, and how much there was.
async fn read_capped(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> (Vec<u8>, usize) {
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

fn parse<T: DeserializeOwned>(arguments: &str) -> Result<T, String> {
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
    async fn a_command_stopped_by_its_timeout_takes_everything_it_started_with_it() {
        let tools = Tools::new(project("timeout-group", &[]));
        let output = run(&tools, &bash_call("sleep 41.17 & sleep 41.17", Some(1))).await;
        assert!(output.contains("took longer"), "{output}");
        assert!(!running("sleep 41.17"), "the sleeps were left running");
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
    async fn a_command_that_finishes_leaves_what_it_put_in_the_background() {
        let tools = Tools::new(project("background", &[]));
        let output = run(&tools, &bash_call("sleep 43.31 >/dev/null 2>&1 & echo started", None)).await;
        assert!(output.contains("started"), "{output}");
        assert!(running("sleep 43.31"), "a server started with & keeps running");
        let _ = std::process::Command::new("pkill").args(["-x", "-f", "sleep 43.31"]).status();
    }

    #[tokio::test]
    async fn a_flood_of_output_isnt_all_kept() {
        let tools = Tools::new(project("flood", &[]));
        let output = run(&tools, &bash_call("head -c 20000000 /dev/zero | tr '\\0' x", None)).await;
        assert!(output.len() < 100_000, "{} bytes kept", output.len());
        assert!(output.contains("more bytes"), "{}", &output[output.len().saturating_sub(200)..]);
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
    async fn undo_puts_back_the_last_turns_changes() {
        let dir = project("undo", &[("a.txt", "one\n")]);
        let tools = Tools::new(&dir);
        tools.checkpoint();
        run(&tools, &call("edit", json!({"path": "a.txt", "old_text": "one", "new_text": "two", "replace_all": false}))).await;
        tools.checkpoint();
        run(&tools, &call("edit", json!({"path": "a.txt", "old_text": "two", "new_text": "three", "replace_all": false}))).await;
        run(&tools, &call("edit", json!({"path": "a.txt", "old_text": "three", "new_text": "four", "replace_all": false}))).await;
        run(&tools, &call("write", json!({"path": "new/b.txt", "content": "made"}))).await;
        tools.checkpoint(); // a turn that changed nothing

        assert_eq!(tools.undo().unwrap(), ["a.txt", "new/b.txt"]);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n", "back to before that turn");
        assert!(!dir.join("new/b.txt").exists(), "a created file goes away");
        assert_eq!(tools.undo().unwrap(), ["a.txt"]);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(tools.undo().unwrap().is_empty());
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
        assert!(answer.contains("Operation not permitted") && answer.contains("[exit code 1]\n[It ran in the sandbox with look access"), "{answer}");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hi\n");
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
    async fn bash_stops_slow_commands() {
        let tools = Tools::new(project("slow", &[])).with_timeout(Duration::from_millis(200));
        let answer = run(&tools, &call("bash", json!({"command": "sleep 5"}))).await;
        assert_eq!(answer, "error: the command took longer than 200ms and was stopped");
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
