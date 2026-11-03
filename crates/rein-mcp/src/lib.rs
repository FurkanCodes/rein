//! MCP servers: programs on this computer that give the model more tools
//! (an issue tracker's, a browser's, a database's), started in rein's
//! sandbox and talked to over their standard input and output.
//!
//! A server is someone else's code, often fetched as it starts (`npx -y`),
//! so it gets the walls a command gets: by default the project and the
//! internet, but not your home folder, other programs' sockets, or rein's
//! keys. What it says is cleaned before you or the model see it, and the
//! tools it lists are offered under names of their own
//! (`mcp__github__get_issue`), fixed for the conversation unless you add or
//! remove a server, so a server that starts late or changes its list can't
//! change what the model was told.

mod clean;
pub mod config;
pub mod import;
pub mod registry;
mod rpc;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rein_sandbox::{Access, Group, Sandbox};
use serde::Deserialize;
use serde_json::{Value, json};

pub use clean::text as cleaned;
pub use config::{Named, Origin, Server, Skipped, Unfilled};
pub use rpc::Error;

/// What the names of servers' tools start with: they can't clash with
/// rein's own, and they say whose they are.
pub const PREFIX: &str = "mcp__";

/// How long a server may take to start and list its tools.
pub const START: Duration = Duration::from_secs(30);

/// How long a tool call may take.
pub const CALL: Duration = Duration::from_secs(300);

/// The protocol version rein asks for, and those it takes: they differ in
/// what rein doesn't use, not in listing and calling tools.
const VERSION: &str = "2025-11-25";
const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The most of a tool's description the model is sent: one server can't
/// fill the conversation with its own.
const LONGEST_DESCRIPTION: usize = 4_000;

/// The longest name providers take for a tool.
const LONGEST_NAME: usize = 64;

/// How many pages of tools rein asks a server for: one that never stops
/// sending more doesn't keep rein waiting.
const MOST_PAGES: usize = 50;

/// A small MCP server in `sh`, for tests, so it runs anywhere, the sandbox
/// too. It answers by what each line asks for: `echo` says its text back
/// (as an error if it starts with `!`),
/// `peek` whether it could read a path, `given` what `$GIVEN` is, `has`
/// whether a variable is set, and `quit` stops it.
#[cfg(any(test, feature = "test-support"))]
pub const FAKE_SERVER: &str = include_str!("fake_server.sh");

/// Whether `name` is a server's tool's, as the model knows it.
pub fn is_mcp(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// The server and the tool a tool's name stands for: `("github",
/// "get_issue")` for `mcp__github__get_issue`.
pub fn parts(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix(PREFIX)?.split_once("__")
}

/// How servers are started: in this sandbox, in this project, each with a
/// folder of its own under `caches` for what `npx` or `uvx` fetch, and the
/// variables they name filled in from your environment or `keys`.
#[derive(Clone)]
pub struct Launch {
    pub sandbox: Arc<Sandbox>,
    pub root: PathBuf,
    pub caches: PathBuf,
    pub keys: Arc<Keys>,
}

/// The values you gave rein for the variables servers name (`${NAME}`),
/// for when your environment hasn't one: so a server's key needn't be
/// exported in the shell rein starts from.
#[derive(Default)]
pub struct Keys(Mutex<BTreeMap<String, String>>);

impl Keys {
    pub fn new(given: BTreeMap<String, String>) -> Keys {
        Keys(Mutex::new(given))
    }

    /// Your variable `name`: your environment's value, or else the one you
    /// gave rein. An empty one isn't a value.
    pub fn get(&self, name: &str) -> Option<String> {
        rein_sandbox::secrets::get(name).filter(|value| !value.is_empty()).or_else(|| lock(&self.0).get(name).cloned())
    }

    pub fn set(&self, name: &str, value: &str) {
        lock(&self.0).insert(name.to_owned(), value.to_owned());
    }

    /// Every value you gave: each is hidden wherever it shows up.
    pub fn values(&self) -> Vec<String> {
        lock(&self.0).values().cloned().collect()
    }
}

/// Only which variables there are values for.
impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(lock(&self.0).keys()).finish()
    }
}

/// Why a server didn't start, as what can be done about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A variable of yours it names isn't set: its name. Given a value, it
    /// can start.
    Needs(String),
    /// It ran in the sandbox and failed, saying this: what it couldn't
    /// reach may be why, though it seldom says so.
    InWalls(String),
    /// It's meant for the sandbox, and there's none here.
    NoSandbox,
    /// Anything else, said in words.
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Needs(name) => write!(f, "it needs {name}, which isn't set"),
            Failure::InWalls(why) => write!(f, "{why} (it ran in the sandbox, which may be why)"),
            Failure::NoSandbox => f.write_str("there's no sandbox here to start it in"),
            Failure::Other(why) => f.write_str(why),
        }
    }
}

impl From<Unfilled> for Failure {
    fn from(unfilled: Unfilled) -> Failure {
        match unfilled {
            Unfilled::Needs(name) => Failure::Needs(name),
            unended @ Unfilled::Unended(_) => Failure::Other(unended.to_string()),
        }
    }
}

impl Launch {
    /// The command that starts `server`, in its walls.
    fn command(&self, server: &Server) -> Result<tokio::process::Command, Failure> {
        let filled = server.filled(&|name| self.keys.get(name))?;
        let mut command = match server.access.walls() {
            // Your whole environment, keys rein kept for itself included,
            // as a command outside the sandbox gets it.
            None => {
                let mut command = tokio::process::Command::new(&filled.command);
                command.args(&filled.args).envs(rein_sandbox::secrets::all().iter().map(|(name, value)| (name, value)));
                command
            }
            Some(walls) => {
                if !self.sandbox.available() {
                    return Err(Failure::NoSandbox);
                }
                let own = self.caches.join(&server.name);
                std::fs::create_dir_all(&own).map_err(|e| Failure::Other(format!("can't make its folder {}: {e}", own.display())))?;
                let line = format!("exec {}", std::iter::once(&filled.command).chain(&filled.args).map(|word| config::quote(word)).collect::<Vec<_>>().join(" "));
                let mut command = self.sandbox.with_own(&own).wrap(&line, walls).map_err(Failure::Other)?;
                // What `npx` and `uvx` fetch goes in its own folder: the
                // shared caches it may only read, as what's put in them
                // runs later, outside.
                command.env("npm_config_cache", own.join("npm")).env("UV_CACHE_DIR", own.join("uv"));
                command
            }
        };
        command.envs(&filled.env).current_dir(&self.root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        // A group of its own, so letting it go stops what it started too.
        #[cfg(unix)]
        command.process_group(0);
        Ok(command)
    }

    /// Whether `server` runs in the sandbox: then what it can't reach may
    /// be why it fails.
    fn walled(&self, server: &Server) -> bool {
        server.access.walls().is_some() && self.sandbox.available()
    }

    /// Starts `server`, says hello, and asks for its tools, within
    /// [`START`].
    async fn start(&self, server: &Server) -> Result<Connection, Failure> {
        let mut command = self.command(server)?;
        let started = async {
            let mut child = command.spawn().map_err(|e| Error::Start(format!("can't run {}: {e}", server.command)))?;
            let group = Group::new(child.id());
            let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
            let (Some(stdin), Some(stdout)) = (stdin, stdout) else { return Err(Error::Start("its input and output weren't there to talk over".into())) };
            let rpc = rpc::Rpc::start(stdout, stdin, child.stderr.take());
            Connection::open(rpc, Some(Process { _group: group, _child: child })).await
        };
        // Once it runs, what it can't reach may be why it fails.
        let failed = |why: String| if self.walled(server) { Failure::InWalls(why) } else { Failure::Other(why) };
        match tokio::time::timeout(START, started).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(Error::Start(why))) => Err(Failure::Other(why)),
            Ok(Err(e)) => Err(failed(e.to_string())),
            Err(_) => Err(failed(format!("it didn't start within {}s", START.as_secs()))),
        }
    }
}

/// A server's process, stopped as a whole when dropped. The group goes
/// first, while its leader isn't reaped and its ID can't be reused.
struct Process {
    _group: Group,
    _child: tokio::process::Child,
}

/// A server rein is talking to, and the tools it listed.
struct Connection {
    _process: Option<Process>,
    rpc: rpc::Rpc,
    tools: Vec<Listed>,
}

/// A tool as its server lists it, cleaned.
#[derive(Clone)]
struct Listed {
    name: String,
    description: String,
    schema: Value,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(rename = "nextCursor")]
    next: Option<String>,
}

impl Connection {
    /// Says hello over `rpc`, in a version both sides speak, and asks for
    /// the tools, page by page.
    async fn open(rpc: rpc::Rpc, process: Option<Process>) -> Result<Connection, Error> {
        let hello = json!({ "protocolVersion": VERSION, "capabilities": {}, "clientInfo": { "name": "rein", "version": env!("CARGO_PKG_VERSION") } });
        let hello = rpc.request("initialize", hello, START).await?;
        let version = hello.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        if !VERSIONS.contains(&version) {
            return Err(Error::Unreadable(format!("it speaks MCP version {}, which rein doesn't yet", clean::text(version))));
        }
        rpc.notify("notifications/initialized", None);
        let mut tools = Vec::new();
        // One with no tools to give says so by not saying it has them.
        if hello.get("capabilities").and_then(|c| c.get("tools")).is_some() {
            let mut cursor: Option<String> = None;
            for _ in 0..MOST_PAGES {
                let params = cursor.as_ref().map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
                let page: Page = serde_json::from_value(rpc.request("tools/list", params, START).await?).map_err(|e| Error::Unreadable(format!("its list of tools: {e}")))?;
                tools.extend(page.tools.iter().filter_map(listed));
                match page.next {
                    Some(next) if !next.is_empty() => cursor = Some(next),
                    _ => break,
                }
            }
        }
        Ok(Connection { _process: process, rpc, tools })
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<String, String> {
        let result = self.rpc.request("tools/call", json!({ "name": tool, "arguments": arguments }), CALL).await.map_err(|e| e.to_string())?;
        clean::result(result)
    }
}

/// A tool the server listed, cleaned: its description cut to a length,
/// and its schema one every provider takes. `None` without a name.
fn listed(tool: &Value) -> Option<Listed> {
    let name = tool.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())?.to_owned();
    let mut description = clean::text(tool.get("description").and_then(Value::as_str).unwrap_or_default());
    if let Some((cut, _)) = description.char_indices().nth(LONGEST_DESCRIPTION) {
        description.truncate(cut);
        description.push('…');
    }
    let mut schema = match tool.get("inputSchema") {
        Some(Value::Object(schema)) => schema.clone(),
        _ => serde_json::Map::new(),
    };
    // What providers want of a tool's parameters: an object, with its
    // properties, and no `$schema` some turn away.
    schema.remove("$schema");
    schema.insert("type".into(), json!("object"));
    schema.entry("properties").or_insert_with(|| json!({}));
    Some(Listed { name, description, schema: Value::Object(schema) })
}

/// How a server is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Starting,
    /// Ready, with these tools, by its own names for them.
    Ready(Vec<String>),
    /// It stopped after it started; it starts again on its next call.
    Stopped,
    /// It didn't start, and why.
    Failed(Failure),
}

enum State {
    Starting,
    Ready(Arc<Connection>),
    Failed(Failure),
}

impl State {
    /// How a server in this state is doing.
    fn status(&self) -> Status {
        match self {
            State::Starting => Status::Starting,
            State::Ready(connection) if connection.rpc.alive() => Status::Ready(connection.tools.iter().map(|tool| tool.name.clone()).collect()),
            State::Ready(_) => Status::Stopped,
            State::Failed(why) => Status::Failed(why.clone()),
        }
    }
}

struct Entry {
    server: Server,
    /// Held while it starts, so whoever wants it waits until it has.
    state: Arc<tokio::sync::Mutex<State>>,
    /// Its start, stopped if it's removed before it's done.
    start: tokio::task::AbortHandle,
}

/// The servers rein started, and the tools of theirs the model is offered.
pub struct Servers {
    /// Each lock is held only to look or change, never across an `.await`.
    entries: Mutex<Vec<Arc<Entry>>>,
    launch: Launch,
    /// The tools last offered, by the names the model knows them by: which
    /// server's, and its own name for it.
    offered: Mutex<HashMap<String, (Arc<Entry>, String)>>,
    /// The starts going on, stopped if rein lets the servers go first.
    starts: Mutex<tokio::task::JoinSet<()>>,
    /// The servers whose failed calls the model was told the sandbox may
    /// be why: once each, as most fail for reasons of their own.
    told: Mutex<HashSet<String>>,
}

/// Never what the servers' environments hold.
impl std::fmt::Debug for Servers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.entries().iter().map(|entry| entry.server.name.clone())).finish()
    }
}

/// A server's tool as the model is offered it: under rein's name for it,
/// with its description cleaned and cut, and its schema as the server
/// wrote it, but for what providers want of one.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// The tools the model is offered, and what it says about the servers.
#[derive(Debug, Default)]
pub struct Listing {
    pub tools: Vec<Tool>,
    /// The tools that run without asking, as your file allows, by the
    /// names the model knows them by.
    pub allowed: Vec<String>,
    /// How each server did, in a line for the conversation: `None` with
    /// no servers.
    pub note: Option<String>,
}

impl Servers {
    /// Starts each of `servers` at once, in the background. Inside a
    /// runtime: the starts are its tasks.
    pub fn start(servers: Vec<Server>, launch: Launch) -> Servers {
        let started = Servers { entries: Mutex::default(), launch, offered: Mutex::default(), starts: Mutex::default(), told: Mutex::default() };
        for server in servers {
            started.add(server);
        }
        started
    }

    /// Starts `server` in the background, in place of one of its name.
    /// Its tools are offered from the next [`Servers::list`].
    pub fn add(&self, server: Server) {
        self.remove(&server.name);
        let state = Arc::new(tokio::sync::Mutex::new(State::Starting));
        // Taken before the start runs, so nobody sees it before.
        let mut held = state.clone().try_lock_owned().expect("a new lock is free");
        let (launch, starting) = (self.launch.clone(), server.clone());
        let start = {
            let mut starts = lock(&self.starts);
            while starts.try_join_next().is_some() {}
            starts.spawn(async move {
                *held = match launch.start(&starting).await {
                    Ok(connection) => State::Ready(Arc::new(connection)),
                    Err(why) => State::Failed(why),
                };
            })
        };
        lock(&self.entries).push(Arc::new(Entry { server, state, start }));
    }

    /// Stops the server called `name`, and its tools aren't called from
    /// now on: whether there was one. A call to it going on finishes.
    pub fn remove(&self, name: &str) -> bool {
        let gone: Vec<Arc<Entry>> = {
            let mut entries = lock(&self.entries);
            let (gone, kept) = entries.drain(..).partition(|entry| entry.server.name == name);
            *entries = kept;
            gone
        };
        lock(&self.offered).retain(|_, (entry, _)| entry.server.name != name);
        lock(&self.told).remove(name);
        for entry in &gone {
            entry.start.abort();
        }
        !gone.is_empty()
    }

    /// Lets the tools `allow` names of the server called `name` run
    /// without asking, from the next [`Servers::list`].
    pub fn allow(&self, name: &str, allow: &[String]) {
        for entry in lock(&self.entries).iter_mut().filter(|entry| entry.server.name == name) {
            let server = Server { allow: allow.to_vec(), ..entry.server.clone() };
            *entry = Arc::new(Entry { server, state: entry.state.clone(), start: entry.start.clone() });
        }
    }

    /// The servers now, each for as long as it's needed: the lock isn't
    /// held while one is waited for.
    fn entries(&self) -> Vec<Arc<Entry>> {
        lock(&self.entries).clone()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.entries).is_empty()
    }

    /// The servers, as their files name them.
    pub fn servers(&self) -> Vec<Server> {
        self.entries().iter().map(|entry| entry.server.clone()).collect()
    }

    /// The server called `name`, as its file names it.
    pub fn server(&self, name: &str) -> Option<Server> {
        self.entries().into_iter().find(|entry| entry.server.name == name).map(|entry| entry.server.clone())
    }

    /// The values you gave rein for servers' variables.
    pub fn keys(&self) -> &Keys {
        &self.launch.keys
    }

    /// What rein hides wherever it shows up: what the servers'
    /// environments get from your variables, what's named like a key, and
    /// every value you gave rein.
    pub fn secrets(&self) -> Vec<String> {
        let keys = &self.launch.keys;
        self.entries().iter().flat_map(|entry| entry.server.secrets(&|name| keys.get(name))).chain(keys.values()).collect()
    }

    /// Whether a call to the server called `name` failed as it ran in the
    /// sandbox: then the sandbox may be why, and letting it out may help.
    /// Forgotten once it's started again, in other walls or the same.
    pub fn failed_in_walls(&self, name: &str) -> bool {
        lock(&self.told).contains(name)
    }

    /// Whether any is still starting.
    pub fn starting(&self) -> bool {
        self.entries().iter().any(|entry| !matches!(entry.state.try_lock().as_deref(), Ok(State::Ready(_) | State::Failed(_))))
    }

    /// How the server called `name` is doing once it has started, or failed
    /// to: `None` if there's none of that name.
    pub async fn started(&self, name: &str) -> Option<Status> {
        let entry = self.entries().into_iter().find(|entry| entry.server.name == name)?;
        let state = entry.state.lock().await;
        Some(state.status())
    }

    /// The tools to offer the model, once every server has started or
    /// failed to: each named for its server, and left out if that name is
    /// too long or taken. They're what the conversation offers from now on.
    pub async fn list(&self) -> Listing {
        let (mut listing, mut offered, mut said, mut failed) = (Listing::default(), HashMap::new(), Vec::new(), false);
        for entry in self.entries() {
            let state = entry.state.lock().await;
            let connection = match &*state {
                State::Ready(connection) => connection.clone(),
                State::Failed(why) => {
                    said.push(format!("{} didn't start: {why}", entry.server.name));
                    failed = true;
                    continue;
                }
                State::Starting => {
                    said.push(format!("{} didn't start", entry.server.name));
                    failed = true;
                    continue;
                }
            };
            drop(state);
            let mut left_out = 0;
            for tool in &connection.tools {
                let name = format!("{PREFIX}{}__{}", entry.server.name, tool.name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-', "_"));
                if name.len() > LONGEST_NAME || offered.contains_key(&name) {
                    left_out += 1;
                    continue;
                }
                let allow = &entry.server.allow;
                if allow.iter().any(|allowed| allowed == "*" || *allowed == tool.name) {
                    listing.allowed.push(name.clone());
                }
                listing.tools.push(Tool { name: name.clone(), description: tool.description.clone(), parameters: tool.schema.clone() });
                offered.insert(name, (entry.clone(), tool.name.clone()));
            }
            let count = connection.tools.len() - left_out;
            let tools = format!("{count} tool{}", if count == 1 { "" } else { "s" });
            let left_out = if left_out > 0 { format!(", {left_out} left out: their names are too long or taken") } else { String::new() };
            said.push(format!("{} ({tools}{left_out})", entry.server.name));
        }
        *lock(&self.offered) = offered;
        let fix = if failed { ". /mcp says what to do" } else { "" };
        listing.note = (!said.is_empty()).then(|| format!("MCP servers: {}{fix}", said.join("; ")));
        listing
    }

    /// Calls the tool the model knows as `name` with `arguments`: the text
    /// it gave, or why it failed. A server that stopped is started again
    /// first, and the model is told.
    pub async fn call(&self, name: &str, arguments: &str) -> Result<String, String> {
        let Some((entry, tool)) = lock(&self.offered).get(name).cloned() else { return Err(format!("there is no tool called {name}")) };
        let arguments: Value = match serde_json::from_str(arguments) {
            Ok(Value::Object(arguments)) => Value::Object(arguments),
            Ok(_) => return Err("bad arguments: they should be an object".into()),
            Err(e) => return Err(format!("bad arguments: {e}")),
        };
        let (connection, again) = {
            let mut state = entry.state.lock().await;
            let again = !matches!(&*state, State::Ready(connection) if connection.rpc.alive());
            if again {
                *state = match self.launch.start(&entry.server).await {
                    Ok(connection) => State::Ready(Arc::new(connection)),
                    Err(why) => State::Failed(why),
                };
            }
            match &*state {
                State::Ready(connection) => (connection.clone(), again),
                State::Failed(why) => return Err(format!("the {} server had stopped, and didn't start again: {why}", entry.server.name)),
                State::Starting => return Err(format!("the {} server didn't start", entry.server.name)),
            }
        };
        let said = connection.call(&tool, arguments).await;
        let name = &entry.server.name;
        let mut note = if again { format!("\n[rein: the {name} server had stopped, so rein started it again for this call.]") } else { String::new() };
        // A server that can't reach what it needs says so in its own
        // words, which may not say why: "the screen is locked".
        if said.is_err() && self.launch.walled(&entry.server) && lock(&self.told).insert(name.clone()) {
            note.push_str(&format!(
                "\n[rein: the {name} server runs {}. If that's why this failed, the user can let it out of the sandbox from /mcp: picking {name} there offers it.]",
                walls(entry.server.access)
            ));
        }
        said.map(|said| said + &note).map_err(|said| said + &note)
    }

    /// Each server, and how it's doing now, without waiting for any.
    pub fn statuses(&self) -> Vec<(Server, Status)> {
        let status = |entry: &Entry| entry.state.try_lock().map_or(Status::Starting, |state| state.status());
        self.entries().iter().map(|entry| (entry.server.clone(), status(entry))).collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The access a server's walls give, in words: for asking about one.
pub fn walls(access: Access) -> &'static str {
    match access {
        Access::Look => "in the sandbox: it reads the project, and not your home folder, other apps or the screen",
        Access::Write => "in the sandbox: it changes the project, without the internet, other apps or the screen",
        Access::Network => "in the sandbox: the project and the internet, not your home folder, keys, other apps or the screen",
        Access::Outside => "outside the sandbox, with your full access: your files, keys, apps and screen",
    }
}

#[cfg(test)]
mod tests;
