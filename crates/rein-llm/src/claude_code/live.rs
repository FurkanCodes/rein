//! A conversation with Claude Code kept in one process, as Claude Code
//! keeps its own: what the model said and called stays with it, with its
//! thinking and its prompt cache, and each request of rein's sends only
//! what's new. The model calls rein's tools as tools of its own (the
//! [`Bridge`]), and Claude Code's own are off, so rein runs every call.
//! Started each request with the whole conversation as one message, the
//! model answered in a JSON shape it was told of, and wrote calls as text.
//!
//! A request that doesn't follow on from what the session has (the model
//! or its settings changed, the conversation was compacted or resumed, a
//! turn was stopped midway) starts a new session, told the conversation so
//! far in one message.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::OwnedMutexGuard;

use super::bridge::Bridge;
use super::{Group, PASSED, Workdir, plan_meters, shown};
use crate::meter::{Meter, Standing};
use crate::{Effort, Error, Event, Item, Request, Result, ToolCall, Usage, found_text};

/// What Claude Code calls rein's tools: those of the MCP server `rein`.
const PREFIX: &str = "mcp__rein__";

/// What the CLI is told when a session starts over midway.
const STARTED_OVER: &str = "rein started its session with you over, so here is this conversation so far, oldest first: \
                            what the user said, what you said, each tool rein ran for you and what it gave. Carry on from \
                            where it ends: if it ends with what a tool gave, go on with the task.";

/// Hooks are rein's to run, as it runs them for every model: run by the
/// CLI as well, each would run twice.
const HOOKS_OFF: &str = r#"{"disableAllHooks":true}"#;

/// The most conversations kept going at once, by `rein serve` say: the
/// longest unused stops first.
const KEPT: usize = 4;

/// The longest line of the CLI's taken: a call can carry a whole file.
const LONGEST_LINE: u64 = 64 * 1024 * 1024;

/// What starts a session's CLI: `claude`, or in tests a fake.
#[derive(Clone)]
pub(super) enum Launcher {
    Cli(String),
    #[cfg(test)]
    Fake(Arc<dyn Fn(Launch) -> Spawned + Send + Sync>),
}

/// What a session's CLI is started with.
pub(super) struct Launch {
    pub(super) model: String,
    pub(super) effort: Option<Effort>,
    pub(super) instructions: Option<String>,
    /// Where rein's tools are, and the token for them: `--mcp-config`.
    pub(super) mcp: Value,
}

/// A started CLI: what it's told, what it prints, and its process, which
/// stops when this goes.
pub(super) struct Spawned {
    pub(super) stdin: Box<dyn AsyncWrite + Send + Unpin>,
    pub(super) stdout: Box<dyn AsyncBufRead + Send + Unpin>,
    pub(super) process: Option<Process>,
}

/// The CLI's process and what goes with it, stopped in this order: the
/// CLI, then all it started, then its folder.
pub(super) struct Process {
    _child: tokio::process::Child,
    _group: Group,
    _stderr: tokio::task::JoinSet<()>,
    _workdir: Workdir,
}

impl Launcher {
    fn launch(&self, launch: Launch, token: Option<&str>) -> Result<Spawned> {
        match self {
            Launcher::Cli(program) => cli(program, &launch, token),
            #[cfg(test)]
            Launcher::Fake(fake) => Ok(fake(launch)),
        }
    }
}

fn cli(program: &str, launch: &Launch, token: Option<&str>) -> Result<Spawned> {
    let workdir = Workdir::new()?;
    // A file only rein can read: the token for rein's tools is in it.
    let config = workdir.0.join("mcp.json");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&config)?, launch.mcp.to_string().as_bytes())?;
    let mut command = Command::new(program);
    command.current_dir(&workdir.0);
    command.env_clear();
    for name in PASSED {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    // A call waits as long as rein does on it: on your answer to a
    // question, say. And what a tool gives is rein's to cap, as it does.
    command.env("MCP_TOOL_TIMEOUT", "86400000");
    command.env("MAX_MCP_OUTPUT_TOKENS", "200000");
    #[cfg(unix)]
    if let Some(token) = token {
        super::hand_over(&mut command, token)?;
    }
    command
        .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--replay-user-messages"])
        // None of its own tools, only rein's, which it may call unasked:
        // rein asks you as its rules say. No `--safe-mode`: it turns off
        // every MCP server, rein's too.
        .args(["--tools", "", "--strict-mcp-config", "--mcp-config"])
        .arg(&config)
        .args(["--allowedTools", "mcp__rein", "--disable-slash-commands", "--settings", HOOKS_OFF, "--no-session-persistence"])
        // Only the user's own settings: a project's, even from another
        // folder, can set `env` and `apiKeyHelper`.
        .args(["--setting-sources", "user", "--model", &launch.model]);
    if let Some(effort) = launch.effort {
        let effort = if effort == Effort::Minimal { Effort::Low } else { effort };
        command.args(["--effort", effort.as_str()]);
    }
    if let Some(instructions) = &launch.instructions {
        command.arg("--append-system-prompt").arg(instructions);
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|e| Error::Auth(format!("couldn't start Claude Code: {e}; install the `claude` CLI and run `rein login claude-code`")))?;
    // The CLI has its own copy of the token's pipe now; this closes rein's.
    drop(command);
    let group = Group(child.id());
    let (Some(stdin), Some(stdout), Some(mut stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        return Err(Error::Incomplete("Claude Code's pipes are unavailable".into()));
    };
    // Read and let go: it may echo credentials, and a full pipe would
    // stop the CLI.
    let mut drain = tokio::task::JoinSet::new();
    drain.spawn(async move {
        let mut sink = [0; 8192];
        while matches!(stderr.read(&mut sink).await, Ok(n) if n > 0) {}
    });
    Ok(Spawned { stdin: Box::new(stdin), stdout: Box::new(BufReader::new(stdout)), process: Some(Process { _child: child, _group: group, _stderr: drain, _workdir: workdir }) })
}

type Slot = Arc<tokio::sync::Mutex<Option<Live>>>;

/// Each conversation's session, by its id, the longest unused first.
type Kept = VecDeque<(Option<String>, Slot)>;

/// The sessions kept going, by conversation.
#[derive(Clone, Default)]
pub(super) struct Sessions(Arc<Mutex<Kept>>);

impl Sessions {
    fn slot(&self, conversation: Option<&str>) -> Slot {
        let mut slots = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = match slots.iter().position(|(id, _)| id.as_deref() == conversation) {
            Some(at) => slots.remove(at).map(|(_, slot)| slot).unwrap_or_default(),
            None => Slot::default(),
        };
        if slots.len() >= KEPT {
            // Its CLI stops once no turn holds it.
            slots.pop_front();
        }
        slots.push_back((conversation.map(str::to_owned), slot.clone()));
        slot
    }
}

/// One session: the CLI, rein's tools as it reaches them, and what it has
/// been told and said, as rein keeps it.
pub(super) struct Live {
    key: Key,
    bridge: Bridge,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    stdout: Box<dyn AsyncBufRead + Send + Unpin>,
    /// Each item of rein's conversation the CLI has, by its digest.
    seen: Vec<u64>,
    /// The calls of the model's last step, which wait for what they gave.
    asked: Vec<String>,
    /// Stopped or gone wrong midway: the next request starts over.
    broken: bool,
    /// Messages written that the CLI hasn't echoed yet: one that came in
    /// as its turn ended starts another, which is this step's too. Counted
    /// only once it has echoed one: a CLI that doesn't would be waited on
    /// for good.
    unechoed: usize,
    echoes: bool,
    meters: Vec<Meter>,
    _process: Option<Process>,
}

/// What a session is started with that a request can change.
#[derive(PartialEq)]
struct Key {
    model: String,
    effort: Option<Effort>,
    instructions: Option<String>,
}

impl Key {
    fn of(request: &Request<'_>) -> Key {
        Key { model: request.model.to_owned(), effort: request.reasoning_effort, instructions: request.instructions.map(str::to_owned) }
    }
}

/// The answer to `request`, from the session it follows on from, or a new
/// one.
pub(super) async fn turn(sessions: &Sessions, launcher: &Launcher, token: Option<&str>, request: &Request<'_>, idle: Duration) -> Result<Turn> {
    let mut live = sessions.slot(request.session_id).lock_owned().await;
    let key = Key::of(request);
    let input: Vec<u64> = request.input.iter().map(digest).collect();
    let tools = offered(request);
    let schemas = tools.iter().filter_map(|tool| Some((tool["name"].as_str()?.to_owned(), tool["inputSchema"].clone()))).collect();
    let carried_on = match live.as_mut() {
        Some(session) if session.follows(&key, &input) => tokio::time::timeout(idle, session.carry_on(request, tools.clone())).await.unwrap_or(Ok(false)),
        _ => Ok(false),
    };
    if !matches!(carried_on, Ok(true)) {
        // The one before stops here, its calls answered as stopped.
        *live = None;
        let mut session = Live::start(launcher, token, key, tools).await?;
        let told = told(request.input, request);
        tokio::time::timeout(idle, session.say(&told)).await.map_err(|_| Error::Incomplete("Claude Code took nothing in".into()))??;
        *live = Some(session);
    }
    Ok(Turn { live, input, schemas, produced: Vec::new(), asked: Vec::new(), queued: VecDeque::new(), usage: Usage::default(), stop: None, streamed: false, idle, over: false })
}

impl Live {
    async fn start(launcher: &Launcher, token: Option<&str>, key: Key, tools: Vec<Value>) -> Result<Live> {
        let bridge = Bridge::start(tools).await?;
        let launch = Launch { model: key.model.clone(), effort: key.effort, instructions: key.instructions.clone(), mcp: bridge.config() };
        let spawned = launcher.launch(launch, token)?;
        Ok(Live {
            key,
            bridge,
            stdin: spawned.stdin,
            stdout: spawned.stdout,
            seen: Vec::new(),
            asked: Vec::new(),
            broken: false,
            unechoed: 0,
            echoes: false,
            meters: Vec::new(),
            _process: spawned.process,
        })
    }

    /// Whether a request with `key` and `input` follows on from what this
    /// session has, adding something to it.
    fn follows(&self, key: &Key, input: &[u64]) -> bool {
        !self.broken && self.key == *key && input.len() > self.seen.len() && input.starts_with(&self.seen)
    }

    /// Hands the CLI what's new in `request`: what the calls it waits on
    /// gave, or your next message. `false` when that isn't what's new.
    async fn carry_on(&mut self, request: &Request<'_>, tools: Vec<Value>) -> Result<bool> {
        let new = &request.input[self.seen.len()..];
        let changed = self.bridge.set_tools(tools);
        if self.asked.is_empty() {
            self.say(&told(new, request)).await?;
            return Ok(true);
        }
        let Some(Answers { gave, heard }) = answers(&self.asked, new, request) else { return Ok(false) };
        // What came in while the calls ran goes as your message, never in
        // what a tool gave: the model rightly takes words in a tool's
        // result for the tool's, and left a message sent there unread.
        // Before the answers, so the CLI hands it over with them.
        if !heard.is_empty() {
            let content: Vec<Value> = heard.into_iter().map(|text| json!({ "type": "text", "text": text })).collect();
            self.say(&json!({ "type": "user", "message": { "role": "user", "content": content }, "parent_tool_use_id": null })).await?;
        }
        // A search found tools: the model calls them next, so the CLI
        // reads the list again first.
        if changed {
            self.bridge.relisted().await;
        }
        for Gave { id, content } in gave {
            self.bridge.answer(&id, content);
        }
        self.asked.clear();
        Ok(true)
    }

    async fn say(&mut self, message: &Value) -> Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        self.unechoed += 1;
        Ok(())
    }
}

/// What the calls the CLI waits on gave, and what came in meanwhile.
struct Answers<'a> {
    /// In the order the model made them.
    gave: Vec<Gave>,
    heard: Vec<&'a str>,
}

/// What the call `id` gave, as MCP content.
struct Gave {
    id: String,
    content: Vec<Value>,
}

/// What each of the `asked` calls gave, in `new`, and the messages that
/// came in meanwhile: `None` unless `new` is that and nothing else.
fn answers<'a>(asked: &[String], new: &'a [Item], request: &Request<'_>) -> Option<Answers<'a>> {
    let images = crate::images_of(new);
    let (mut gave, mut heard) = (Vec::new(), Vec::new());
    for item in new {
        match item {
            Item::ToolResult { call_id, output } if asked.contains(call_id) => {
                let (said, shown) = crate::split_images(output, images.get(call_id.as_str()).map(Vec::as_slice).unwrap_or_default(), request.images);
                let mut content = vec![json!({ "type": "text", "text": said })];
                content.extend(shown.into_iter().filter_map(|image| Some(json!({ "type": "image", "data": image.base64()?, "mimeType": image.kind.media_type() }))));
                gave.push(Gave { id: call_id.clone(), content });
            }
            Item::Found { call_id, names } if asked.contains(call_id) => {
                gave.push(Gave { id: call_id.clone(), content: vec![json!({ "type": "text", "text": found_text(names) })] })
            }
            Item::Image { call_id, .. } if asked.contains(call_id) => {}
            Item::User(text) => heard.push(text.as_str()),
            _ => return None,
        }
    }
    gave.sort_by_key(|gave| asked.iter().position(|asked| *asked == gave.id));
    gave.dedup_by(|a, b| a.id == b.id);
    if gave.len() != asked.len() {
        return None;
    }
    Some(Answers { gave, heard })
}

/// What the CLI hasn't been told, as one message of the user's: their
/// words as they are, or, when a session starts over midway, the
/// conversation so far, item by item.
fn told(new: &[Item], request: &Request<'_>) -> Value {
    let over = new.iter().any(|item| matches!(item, Item::Assistant(_) | Item::Opaque(_)));
    let images = crate::images_of(new);
    let text = |text: String| json!({ "type": "text", "text": text });
    let mut content = Vec::new();
    if over {
        content.push(text(STARTED_OVER.to_owned()));
    }
    for item in new {
        match item {
            Item::User(said) if over => content.push(text(format!("[the user]\n{said}"))),
            Item::User(said) => content.push(text(said.clone())),
            Item::Assistant(said) => content.push(text(format!("[you]\n{said}"))),
            Item::ToolCall(call) => content.push(text(format!("[rein ran {} for call {}]\n{}", call.name, call.call_id, call.arguments))),
            Item::ToolResult { call_id, output } => {
                let (said, shown) = crate::split_images(output, images.get(call_id.as_str()).map(Vec::as_slice).unwrap_or_default(), request.images);
                content.push(text(format!("[call {call_id} gave]\n{said}")));
                content.extend(
                    shown
                        .into_iter()
                        .filter_map(|image| Some(json!({ "type": "image", "source": { "type": "base64", "media_type": image.kind.media_type(), "data": image.base64()? } }))),
                );
            }
            Item::Found { call_id, names } => content.push(text(format!("[call {call_id} gave]\n{}", found_text(names)))),
            Item::Image { .. } | Item::Opaque(_) => {}
        }
    }
    json!({ "type": "user", "message": { "role": "user", "content": content }, "parent_tool_use_id": null })
}

/// rein's tools as an MCP server lists them: those the model is told of.
fn offered(request: &Request<'_>) -> Vec<Value> {
    shown(request).into_iter().map(|tool| json!({ "name": tool.name, "description": tool.description, "inputSchema": tool.parameters })).collect()
}

/// A call's `input`, with each argument the CLI sent as text read as what
/// `schema` says it is. The CLI doesn't hold the model to rein's schemas,
/// and Claude sent `""` for a line number it meant to leave out, which
/// failed the call: `""` for what may be null is null, and `"20"` or
/// `"true"` for what can't be text is the number or the yes it says.
fn as_schema_says(input: &Value, schema: Option<&Value>) -> Value {
    let (Some(arguments), Some(properties)) = (input.as_object(), schema.and_then(|schema| schema["properties"].as_object())) else { return input.clone() };
    let read = |name: &String, value: &Value| {
        let (Some(text), Some(property)) = (value.as_str(), properties.get(name)) else { return value.clone() };
        let types: Vec<&str> = match &property["type"] {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if types.contains(&"string") {
            return value.clone();
        }
        let text = text.trim();
        let number = || {
            types
                .contains(&"integer")
                .then(|| text.parse::<i64>().ok().map(Value::from))
                .flatten()
                .or_else(|| types.contains(&"number").then(|| text.parse::<f64>().ok().map(Value::from)).flatten())
        };
        let yes = || types.contains(&"boolean").then(|| text.parse::<bool>().ok().map(Value::Bool)).flatten();
        let none = || (text.is_empty() && types.contains(&"null")).then_some(Value::Null);
        none().or_else(number).or_else(yes).unwrap_or_else(|| value.clone())
    };
    Value::Object(arguments.iter().map(|(name, value)| (name.clone(), read(name, value))).collect())
}

/// An item, told apart from any other: rein's conversation is checked
/// against the session's this way, without a copy of it kept.
fn digest(item: &Item) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    match item {
        Item::User(text) => (0u8, text).hash(&mut hasher),
        Item::Assistant(text) => (1u8, text).hash(&mut hasher),
        Item::ToolCall(call) => (2u8, &call.call_id, &call.name, &call.arguments).hash(&mut hasher),
        Item::ToolResult { call_id, output } => (3u8, call_id, output).hash(&mut hasher),
        Item::Found { call_id, names } => (4u8, call_id, names).hash(&mut hasher),
        Item::Image { call_id, image } => (5u8, call_id, &image.file, &image.about).hash(&mut hasher),
        Item::Opaque(_) => 6u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// One step of the model's, as the CLI prints it: what it writes and
/// thinks as it comes, then what it said and called, once its message is
/// done. It holds the session, which goes on with the next request; let go
/// before the step is done (a stopped turn), the session starts over.
pub(crate) struct Turn {
    live: OwnedMutexGuard<Option<Live>>,
    input: Vec<u64>,
    /// Each tool's arguments, by its name, which a call is read against.
    schemas: HashMap<String, Value>,
    produced: Vec<u64>,
    asked: Vec<String>,
    queued: VecDeque<Result<Event>>,
    usage: Usage,
    /// Why the model's last message stopped, once it says.
    stop: Option<String>,
    /// Whether the text block being written came as it was written: a
    /// block that didn't is shown whole.
    streamed: bool,
    idle: Duration,
    over: bool,
}

impl Turn {
    pub(crate) async fn next(&mut self) -> Option<Result<Event>> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Some(event);
            }
            if self.over {
                return None;
            }
            let live = self.live.as_mut()?;
            let line = match tokio::time::timeout(self.idle, next_line(&mut live.stdout)).await {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => return Some(self.fail(Error::Auth("Claude Code stopped; check `claude auth status` or run `claude setup-token`".into()))),
                Ok(Err(e)) => return Some(self.fail(e.into())),
                Err(_) => return Some(self.fail(Error::Incomplete(format!("Claude Code sent nothing for {} seconds", self.idle.as_secs())))),
            };
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                self.read(&value);
            }
        }
    }

    fn read(&mut self, value: &Value) {
        // A sub-agent of the CLI's own: there are none, with no tools of its own.
        if !value["parent_tool_use_id"].is_null() {
            return;
        }
        match value["type"].as_str() {
            Some("stream_event") => self.streamed(&value["event"]),
            Some("assistant") => {
                for block in value["message"]["content"].as_array().into_iter().flatten() {
                    self.block(block);
                }
            }
            // A message of rein's, taken in.
            Some("user") if value["isReplay"] == true => {
                if let Some(live) = self.live.as_mut() {
                    live.unechoed = live.unechoed.saturating_sub(1);
                    live.echoes = true;
                }
            }
            Some("rate_limit_event") => {
                if let Some(live) = self.live.as_mut() {
                    crate::meter::merge(&mut live.meters, plan_meters(&value["rate_limit_info"]));
                    self.queued.push_back(Ok(Event::Meters(live.meters.clone())));
                }
            }
            Some("result") => self.ended(value),
            _ => {}
        }
    }

    fn streamed(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("message_start") => {
                let usage = &event["message"]["usage"];
                let count = |name: &str| usage[name].as_u64().unwrap_or_default();
                let cached = count("cache_read_input_tokens");
                self.usage = Usage {
                    input_tokens: count("input_tokens").saturating_add(cached).saturating_add(count("cache_creation_input_tokens")),
                    cached_input_tokens: cached,
                    ..Usage::default()
                };
                self.stop = None;
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                match (delta["type"].as_str(), delta["text"].as_str(), delta["thinking"].as_str()) {
                    (Some("text_delta"), Some(text), _) => {
                        self.streamed = true;
                        self.queued.push_back(Ok(Event::TextDelta(text.to_owned())));
                    }
                    (Some("thinking_delta"), _, Some(thinking)) => self.queued.push_back(Ok(Event::Thought(thinking.to_owned()))),
                    _ => {}
                }
            }
            Some("message_delta") => {
                self.stop = event["delta"]["stop_reason"].as_str().map(str::to_owned);
                if let Some(output) = event["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = output;
                }
            }
            // The message is whole: the CLI calls rein's tools next.
            Some("message_stop") if self.stop.as_deref() == Some("tool_use") && !self.asked.is_empty() => self.finish(),
            _ => {}
        }
    }

    fn block(&mut self, block: &Value) {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                    if !std::mem::take(&mut self.streamed) {
                        self.queued.push_back(Ok(Event::TextDelta(text.to_owned())));
                    }
                    self.item(Item::Assistant(text.to_owned()));
                }
            }
            Some("tool_use") => {
                let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) else { return };
                let Some(name) = name.strip_prefix(PREFIX) else {
                    let failed = self.fail(Error::Incomplete("Claude Code called a tool that isn't rein's".into()));
                    self.queued.push_back(failed);
                    return;
                };
                self.asked.push(id.to_owned());
                let arguments = as_schema_says(&block["input"], self.schemas.get(name)).to_string();
                self.item(Item::ToolCall(ToolCall { call_id: id.to_owned(), name: name.to_owned(), arguments }));
            }
            _ => {}
        }
    }

    fn item(&mut self, item: Item) {
        self.produced.push(digest(&item));
        self.queued.push_back(Ok(Event::Item(item)));
    }

    /// The CLI's turn ended: its answer, or why there's none. Neither its
    /// error nor stderr is shown, as they may echo credentials.
    fn ended(&mut self, result: &Value) {
        let failed = result["is_error"] == true || result["subtype"].as_str().is_some_and(|subtype| subtype != "success");
        if !failed {
            // A message came in too late for this turn: the CLI answers it
            // in the next, which goes on this step.
            if self.live.as_ref().is_some_and(|live| live.echoes && live.unechoed > 0) {
                return;
            }
            return self.finish();
        }
        let reached = self.live.as_ref().and_then(|live| live.meters.iter().find(|meter| meter.standing == Standing::Reached).map(Meter::label));
        let error = match reached {
            Some(window) => Error::usage_limit(format!("your Claude plan's {window} limit is reached")),
            None => Error::Auth("Claude Code reported an error; check `claude auth status` and your subscription limits".into()),
        };
        let failed = self.fail(error);
        self.queued.push_back(failed);
    }

    /// The step is done: what the session has now is the request and the
    /// answer.
    fn finish(&mut self) {
        self.queued.push_back(Ok(Event::Completed { usage: std::mem::take(&mut self.usage) }));
        self.over = true;
        if let Some(live) = self.live.as_mut() {
            live.seen = std::mem::take(&mut self.input);
            live.seen.append(&mut self.produced);
            live.asked = std::mem::take(&mut self.asked);
        }
    }

    fn fail(&mut self, error: Error) -> Result<Event> {
        self.over = true;
        if let Some(live) = self.live.as_mut() {
            live.broken = true;
        }
        Err(error)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if !self.over
            && let Some(live) = self.live.as_mut()
        {
            live.broken = true;
        }
    }
}

/// The CLI's next line, without its end; `None` once it's done.
async fn next_line(stdout: &mut (dyn AsyncBufRead + Send + Unpin)) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = stdout.take(LONGEST_LINE).read_until(b'\n', &mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') && read as u64 >= LONGEST_LINE {
        return Err(std::io::Error::other("Claude Code printed a line over 64 MiB"));
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{held_back_tools, request};
    use crate::{Loading, TOOL_SEARCH, Tool};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// What the fake CLI does with one message of the user's.
    #[derive(Clone)]
    enum Act {
        /// Writes this, as it streams and then whole.
        Say(&'static str),
        /// Calls this tool of rein's (by id, name and arguments).
        Call(&'static str, &'static str, Value),
        /// Ends the message with its calls, then makes each, in turn.
        Ran,
        /// Reads rein's tools again, as after `list_changed`.
        Listed,
        /// Ends the message, and the turn.
        Done,
    }

    /// What the fake CLI saw: how often it was started, and with what;
    /// each message it was given; what each call gave; each list it read.
    #[derive(Clone, Default)]
    struct Seen {
        launches: Arc<AtomicUsize>,
        models: Arc<Mutex<Vec<String>>>,
        told: Arc<Mutex<Vec<Value>>>,
        gave: Arc<Mutex<Vec<Value>>>,
        listed: Arc<Mutex<Vec<Vec<String>>>>,
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap()
    }

    /// A CLI that plays `script`, one list of acts per message it's given,
    /// calling rein's tools over HTTP as Claude Code does.
    fn fake(script: Vec<Vec<Act>>, seen: &Seen) -> Launcher {
        // One script, whichever session plays the next of it.
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let seen = seen.clone();
        Launcher::Fake(Arc::new(move |launch: Launch| {
            seen.launches.fetch_add(1, Ordering::SeqCst);
            lock(&seen.models).push(launch.model.clone());
            let rein = &launch.mcp["mcpServers"]["rein"];
            let (url, auth) = (rein["url"].as_str().unwrap().to_owned(), rein["headers"]["Authorization"].as_str().unwrap().to_owned());
            let (ours, theirs) = tokio::io::duplex(1 << 20);
            tokio::spawn(play(script.clone(), seen.clone(), theirs, url, auth));
            let (read, write) = tokio::io::split(ours);
            Spawned { stdin: Box::new(write), stdout: Box::new(BufReader::new(read)), process: None }
        }))
    }

    async fn play(script: Arc<Mutex<VecDeque<Vec<Act>>>>, seen: Seen, io: tokio::io::DuplexStream, url: String, auth: String) {
        let (read, mut write) = tokio::io::split(io);
        let mut lines = BufReader::new(read).lines();
        let http = reqwest::Client::new();
        let post = |message: Value| {
            let request = http.post(&url).header("Authorization", &auth).json(&message);
            async move { request.send().await.unwrap().json::<Value>().await.unwrap() }
        };
        let event = |event: Value| json!({ "type": "stream_event", "event": event, "parent_tool_use_id": null });
        let start = event(json!({ "type": "message_start", "message": { "usage": { "input_tokens": 3, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 2 } } }));
        loop {
            let Ok(Some(line)) = lines.next_line().await else { return };
            let Some(acts) = lock(&script).pop_front() else { return };
            let mut replay: Value = serde_json::from_str(&line).unwrap();
            replay["isReplay"] = json!(true);
            write.write_all(format!("{replay}\n").as_bytes()).await.unwrap();
            lock(&seen.told).push(serde_json::from_str(&line).unwrap());
            let mut printed = vec![start.clone()];
            let mut calls = Vec::new();
            for act in acts {
                match act {
                    Act::Say(text) => {
                        printed.push(event(json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": text } })));
                        printed.push(json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": text }] }, "parent_tool_use_id": null }));
                    }
                    Act::Call(id, name, input) => {
                        printed.push(json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "id": id, "name": format!("{PREFIX}{name}"), "input": input }] }, "parent_tool_use_id": null }));
                        calls.push((id, name, input));
                    }
                    Act::Ran => {
                        printed.push(event(json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 7 } })));
                        printed.push(event(json!({ "type": "message_stop" })));
                        for line in printed.drain(..) {
                            write.write_all(format!("{line}\n").as_bytes()).await.unwrap();
                        }
                        for (id, name, input) in calls.drain(..) {
                            let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": input, "_meta": { "claudecode/toolUseId": id } } });
                            let answered = post(call).await;
                            lock(&seen.gave).push(answered["result"].clone());
                        }
                        printed.push(start.clone());
                    }
                    Act::Listed => {
                        let listed = post(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
                        let names = listed["result"]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap().to_owned()).collect();
                        lock(&seen.listed).push(names);
                    }
                    Act::Done => {
                        printed.push(event(json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 5 } })));
                        printed.push(event(json!({ "type": "message_stop" })));
                        printed.push(json!({ "type": "result", "subtype": "success", "is_error": false, "result": "done" }));
                    }
                }
            }
            for line in printed {
                write.write_all(format!("{line}\n").as_bytes()).await.unwrap();
            }
        }
    }

    fn read_tool() -> Vec<Tool> {
        vec![Tool { grammar: None, loading: Loading::UpFront, strict: true, name: "read".into(), description: "Read a file.".into(), parameters: json!({ "type": "object" }) }]
    }

    /// Every event of a step, the error ending it if one does.
    async fn step(sessions: &Sessions, launcher: &Launcher, request: &Request<'_>) -> Vec<Result<Event>> {
        let mut turn = turn(sessions, launcher, None, request, Duration::from_secs(5)).await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = turn.next().await {
            events.push(event);
        }
        events
    }

    fn items(events: &[Result<Event>]) -> Vec<Item> {
        events.iter().filter_map(|event| if let Ok(Event::Item(item)) = event { Some(item.clone()) } else { None }).collect()
    }

    fn texts(message: &Value) -> Vec<String> {
        message["message"]["content"].as_array().unwrap().iter().filter_map(|block| block["text"].as_str().map(str::to_owned)).collect()
    }

    fn call(id: &str, name: &str, arguments: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: name.into(), arguments: arguments.into() })
    }

    #[tokio::test]
    async fn a_conversation_is_one_session_in_which_the_model_calls_reins_tools_as_its_own() {
        let seen = Seen::default();
        let script = vec![
            vec![
                Act::Say("Reading."),
                Act::Call("toolu_1", "read", json!({ "path": "a.rs" })),
                Act::Call("toolu_2", "read", json!({ "path": "b.rs" })),
                Act::Ran,
                Act::Say("Both empty."),
                Act::Done,
            ],
            vec![Act::Say("You're welcome."), Act::Done],
        ];
        let (sessions, launcher, tools) = (Sessions::default(), fake(script, &seen), read_tool());

        let mut input = vec![Item::User("read a.rs and b.rs".into())];
        let first = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        assert!(matches!(&first[0], Ok(Event::TextDelta(text)) if text == "Reading."), "{first:?}");
        let said = items(&first);
        assert_eq!(said, [Item::Assistant("Reading.".into()), call("toolu_1", "read", r#"{"path":"a.rs"}"#), call("toolu_2", "read", r#"{"path":"b.rs"}"#)]);
        let Some(Ok(Event::Completed { usage })) = first.last() else { panic!("{first:?}") };
        assert_eq!((usage.input_tokens, usage.cached_input_tokens, usage.output_tokens), (15, 10, 7), "this step's message alone");

        input.extend(said);
        input.push(Item::ToolResult { call_id: "toolu_1".into(), output: "(empty a)".into() });
        input.push(Item::ToolResult { call_id: "toolu_2".into(), output: "(empty b)".into() });
        let second = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        assert_eq!(items(&second), [Item::Assistant("Both empty.".into())]);
        let gave: Vec<Value> = lock(&seen.gave).iter().map(|result| result["content"][0]["text"].clone()).collect();
        assert_eq!(gave, [json!("(empty a)"), json!("(empty b)")], "each call gets what it gave, as the CLI makes them");

        input.extend(items(&second));
        input.push(Item::User("thanks".into()));
        let third = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        assert_eq!(items(&third), [Item::Assistant("You're welcome.".into())]);
        assert_eq!(seen.launches.load(Ordering::SeqCst), 1, "one session for the whole conversation");
        let told: Vec<Vec<String>> = lock(&seen.told).iter().map(texts).collect();
        assert_eq!(told, [vec!["read a.rs and b.rs".to_owned()], vec!["thanks".to_owned()]], "only what's new is sent");
    }

    /// Claude, through the CLI, sent `""` for a skill's line number it
    /// meant to leave out, and the call failed: what it sends as text is
    /// read as the tool's schema says.
    #[tokio::test]
    async fn arguments_sent_as_text_are_read_as_the_schema_says() {
        let seen = Seen::default();
        let sent = json!({ "path": "a.rs", "start_line": "", "line_count": " 20 ", "ratio": "0.5", "all": "true", "note": "", "kept": "", "odd": "x" });
        let script = vec![vec![Act::Call("toolu_1", "read", sent), Act::Ran, Act::Done]];
        let parameters = json!({ "type": "object", "properties": {
            "path": { "type": "string" },
            "start_line": { "type": ["integer", "null"] },
            "line_count": { "type": ["integer", "null"] },
            "ratio": { "type": "number" },
            "all": { "type": "boolean" },
            "note": { "type": ["string", "null"] },
            "kept": { "type": "integer" },
            "odd": { "type": ["integer", "null"] }
        } });
        let tools = vec![Tool { grammar: None, loading: Loading::UpFront, strict: true, name: "read".into(), description: "Read a file.".into(), parameters }];
        let (sessions, launcher) = (Sessions::default(), fake(script, &seen));
        let input = vec![Item::User("read a.rs".into())];
        let events = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        let said = items(&events);
        let [Item::ToolCall(call)] = said.as_slice() else { panic!("{events:?}") };
        let read: Value = serde_json::from_str(&call.arguments).unwrap();
        let wanted = json!({ "path": "a.rs", "start_line": null, "line_count": 20, "ratio": 0.5, "all": true, "note": "", "kept": "", "odd": "x" });
        assert_eq!(read, wanted, "text is left as it is where the tool takes text, or where it says nothing else");
    }

    #[tokio::test]
    async fn a_conversation_that_doesnt_follow_on_starts_a_session_told_it_so_far() {
        let seen = Seen::default();
        let script = vec![vec![Act::Say("Hi."), Act::Done], vec![Act::Say("Still here."), Act::Done], vec![Act::Say("Opus here."), Act::Done]];
        let (sessions, launcher, tools) = (Sessions::default(), fake(script, &seen), read_tool());
        let mut input = vec![Item::User("hello".into())];
        let first = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        input.extend(items(&first));
        // Compacted, say: what the session has isn't what rein has now.
        let compacted = [
            Item::User("a summary".into()),
            call("r1", "read", "{}"),
            Item::ToolResult { call_id: "r1".into(), output: "kept".into() },
            Item::Assistant("Noted.".into()),
            Item::User("go on".into()),
        ];
        let second = step(&sessions, &launcher, &request(&compacted, &tools, false)).await;
        assert_eq!(items(&second), [Item::Assistant("Still here.".into())]);
        assert_eq!(seen.launches.load(Ordering::SeqCst), 2);
        let told = texts(&lock(&seen.told)[1]);
        assert_eq!(told[0], STARTED_OVER);
        assert_eq!(told[1..], ["[the user]\na summary", "[rein ran read for call r1]\n{}", "[call r1 gave]\nkept", "[you]\nNoted.", "[the user]\ngo on"]);
        // Another model is another session.
        let mut more = compacted.to_vec();
        more.extend(items(&second));
        more.push(Item::User("and you?".into()));
        let opus = Request { model: "opus", ..request(&more, &tools, false) };
        step(&sessions, &launcher, &opus).await;
        assert_eq!(*lock(&seen.models), ["m", "m", "opus"]);
    }

    #[tokio::test]
    async fn a_step_let_go_midway_starts_over_at_the_next_request() {
        let seen = Seen::default();
        let script = vec![vec![Act::Say("Working on it."), Act::Done], vec![Act::Say("Again."), Act::Done]];
        let (sessions, launcher, tools) = (Sessions::default(), fake(script, &seen), read_tool());
        let input = vec![Item::User("do it".into())];
        let mut stopped = turn(&sessions, &launcher, None, &request(&input, &tools, false), Duration::from_secs(5)).await.unwrap();
        assert!(matches!(stopped.next().await, Some(Ok(Event::TextDelta(_)))));
        drop(stopped);
        let mut input = input;
        input.push(Item::User("try again".into()));
        let again = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        assert_eq!(items(&again), [Item::Assistant("Again.".into())]);
        assert_eq!(seen.launches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn found_tools_are_listed_again_before_the_model_hears_of_them() {
        let seen = Seen::default();
        let script = vec![vec![Act::Call("toolu_s", TOOL_SEARCH, json!({ "query": "linear issue" })), Act::Ran, Act::Listed, Act::Say("Found it."), Act::Done]];
        let (sessions, launcher, tools) = (Sessions::default(), fake(script, &seen), held_back_tools());
        let mut input = vec![Item::User("file an issue".into())];
        let first = step(&sessions, &launcher, &request(&input, &tools, true)).await;
        input.extend(items(&first));
        input.push(Item::Found { call_id: "toolu_s".into(), names: vec!["mcp__linear__create_issue".into()] });
        // The fake reads the list only once the search is answered: the
        // wait for it ends at the timeout, and the answer still comes.
        let second = step(&sessions, &launcher, &request(&input, &tools, true)).await;
        assert_eq!(items(&second), [Item::Assistant("Found it.".into())]);
        assert_eq!(lock(&seen.gave)[0]["content"][0]["text"], "Found: mcp__linear__create_issue. Call them as any other tool.");
        assert_eq!(*lock(&seen.listed), [vec!["read".to_owned(), "mcp__linear__create_issue".into(), TOOL_SEARCH.into()]]);
    }

    #[tokio::test]
    async fn a_message_that_comes_in_midway_goes_as_yours_never_in_what_a_tool_gave() {
        let seen = Seen::default();
        // The CLI takes it in only once its turn is over: it answers it in
        // the next, and that's the same step for rein.
        let script = vec![vec![Act::Call("toolu_1", "read", json!({})), Act::Ran, Act::Say("Read it."), Act::Done], vec![Act::Say("Switching to b.rs."), Act::Done]];
        let (sessions, launcher, tools) = (Sessions::default(), fake(script, &seen), read_tool());
        let mut input = vec![Item::User("read".into())];
        let first = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        input.extend(items(&first));
        input.push(Item::ToolResult { call_id: "toolu_1".into(), output: "text".into() });
        input.push(Item::User("use b.rs instead".into()));
        let second = step(&sessions, &launcher, &request(&input, &tools, false)).await;
        assert_eq!(items(&second), [Item::Assistant("Read it.".into()), Item::Assistant("Switching to b.rs.".into())]);
        assert_eq!(lock(&seen.gave)[0]["content"], json!([{ "type": "text", "text": "text" }]), "the tool's result alone");
        assert_eq!(texts(&lock(&seen.told)[1]), ["use b.rs instead"], "a message of the user's");
        assert_eq!(seen.launches.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn what_a_call_gave_is_needed_for_each_call_and_nothing_else_but_messages() {
        let asked = vec!["a".to_owned(), "b".to_owned()];
        let tools = read_tool();
        let only_a = [Item::ToolResult { call_id: "a".into(), output: "1".into() }];
        assert!(answers(&asked, &only_a, &request(&only_a, &tools, false)).is_none(), "b's is missing");
        let stray = [Item::ToolResult { call_id: "a".into(), output: "1".into() }, Item::ToolResult { call_id: "b".into(), output: "2".into() }, Item::Assistant("?".into())];
        assert!(answers(&asked, &stray, &request(&stray, &tools, false)).is_none());
        let pictured = crate::testing::pictured();
        let answered = answers(&["r1".to_owned()], &pictured[2..4], &request(&pictured, &tools, false)).unwrap();
        assert_eq!(answered.gave[0].content[1], json!({ "type": "image", "data": "cG5nIQ==", "mimeType": "image/png" }), "an image goes as MCP content");
    }
}
