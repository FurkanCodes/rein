//! The MCP servers rein starts: yours, from `mcp.json` in rein's folder,
//! and a project's, from its `.mcp.json`, each only once you've said yes to
//! it as it is. The files have the shape Claude Code's and Cursor's do (see
//! [`rein_mcp::config`]).
//!
//! `/mcp` says how each server is doing, then lets you pick one and change
//! it: its tools that run without asking, its walls, on or off. One that
//! didn't start is offered what would fix it there and then, a key it
//! needs or room the sandbox keeps from it, rather than told a command to
//! type. `/mcp add`, `remove` and `allow` do it typed. Each writes your
//! file, which stays the record, so you needn't.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rein_agent::agent::{Agent, Llm};
use rein_agent::sandbox::{Access, Sandbox};
use rein_mcp::{Failure, Keys, Named, Origin, Server, Servers, Skipped, Status};
use serde_json::{Value, json};

use crate::commands::Mcp as Command;
use crate::input::{Question, Read};
use crate::terminal::Terminal;
use crate::ui::style::{GUTTER, Style};
use crate::{CliResult, describe};

mod panel;

/// Your own servers, in rein's folder.
pub(crate) const FILE: &str = "mcp.json";

/// A project's, as Claude Code and others name it.
pub(crate) const PROJECT_FILE: &str = ".mcp.json";

/// The project servers you said yes to, as each was then, by project.
const APPROVED_FILE: &str = "mcp-approved.json";

/// The values you gave rein for servers' variables, by name. Only you can
/// read it, and the sandbox hides it with the rest of rein's folder.
const KEYS_FILE: &str = "mcp-keys.json";

/// The servers rein started, and those it didn't, with why.
pub(crate) struct Mcp {
    /// Those started, which `/mcp add` adds to.
    pub(crate) servers: Arc<Servers>,
    /// Yours that you turned off: in your file, not started.
    off: Vec<Server>,
    /// The project's you haven't said yes to as they are now.
    waiting: Vec<Server>,
    /// Those rein can't start, and why.
    skipped: Vec<Skipped>,
    /// rein's folder, which has your file.
    dir: PathBuf,
    /// The project's, which has its file, and by which your yes to its
    /// servers is kept.
    root: PathBuf,
}

impl Mcp {
    /// What the servers' environments hold that rein hides wherever it
    /// shows up: the values of your variables they get, and keys.
    pub(crate) fn secrets(&self) -> Vec<String> {
        self.servers.secrets()
    }

    /// The key your yes to this project's servers is kept under.
    fn project(&self) -> String {
        self.root.display().to_string()
    }
}

/// Reads your servers and the project at `root`'s, asks about each of the
/// project's you haven't said yes to as it is now, and for any variable a
/// server needs that isn't set (when `asks`: there's someone to ask), and
/// starts them all in the background.
pub(crate) async fn open(dir: &Path, root: &Path, sandbox: Arc<Sandbox>, terminal: &mut Terminal, asks: bool) -> Mcp {
    let named = read(&dir.join(FILE), Origin::Yours).and(read(&root.join(PROJECT_FILE), Origin::Project));
    let approved = approved(dir);
    let key = root.display().to_string();
    let keys = Arc::new(Keys::new(given_keys(dir)));
    let (mut servers, mut waiting) = (Vec::new(), Vec::new());
    for server in named.servers {
        let said_yes = server.origin == Origin::Yours || approved.get(&key).is_some_and(|yes| yes.contains(&server.fingerprint()));
        if said_yes || (asks && say_yes(&server, dir, &key, terminal).await) {
            servers.push(server);
        } else {
            waiting.push(server);
        }
    }
    if asks {
        for server in &servers {
            give_keys(server, &keys, dir, terminal).await;
        }
    }
    let not: Vec<&str> = named.skipped.iter().map(|skipped| skipped.name.as_str()).chain(waiting.iter().map(|server| server.name.as_str())).collect();
    if !asks && !not.is_empty() {
        say!("{}", terminal.err.dim(&format!("(MCP servers not started: {}; rein says why in a conversation, with /mcp)", not.join(", "))));
    } else if !not.is_empty() {
        let count = not.len();
        say!("{}", terminal.err.dim(&format!("({count} MCP server{} not started: /mcp says why)", if count == 1 { "" } else { "s" })));
    }
    let launch = rein_mcp::Launch { sandbox, root: root.to_owned(), caches: caches(), keys };
    Mcp { servers: Arc::new(Servers::start(servers, launch)), off: named.off, waiting, skipped: named.skipped, dir: dir.to_owned(), root: root.to_owned() }
}

/// `/mcp`, and what follows it.
pub(crate) async fn command(command: Command, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    match command {
        Command::Show => panel::panel(mcp, agent, terminal).await,
        Command::Add(named) => add(named, mcp, agent, terminal).await,
        Command::Remove(name) => remove(&name, mcp, agent, terminal),
        Command::Allow(name, tools) => allow(&name, &tools, mcp, agent, terminal),
    }
}

/// Puts `named`'s servers in your file, each in place of one of its name,
/// and starts them, waiting to say how each did: whether it works is what
/// you'd want to know next, and what would fix it if it doesn't.
async fn add(named: Named, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    for skipped in &named.skipped {
        say!("{pad}{}", s.red(&format!("✗ {} isn't added: {}", skipped.name, skipped.why)));
    }
    // One a snippet turns off is added all the same: adding it is saying to.
    let added: Vec<Server> = named.servers.into_iter().chain(named.off).collect();
    if added.is_empty() {
        return if named.skipped.is_empty() { Err("there's no server in that to add".into()) } else { Ok(()) };
    }
    let replaced = edit(&mcp.dir, |servers| Ok(added.iter().map(|server| servers.insert(server.name.clone(), server.entry()).is_some()).collect::<Vec<_>>()))?;
    let file = shown_file(&mcp.dir);
    for (server, replaced) in added.iter().zip(replaced) {
        let name = &server.name;
        // Yours is the one you meant, as when both files name it.
        let running = mcp.servers.server(name).filter(|running| running.origin == Origin::Project);
        let waiting = mcp.waiting.iter().position(|theirs| theirs.name == *name).map(|at| mcp.waiting.remove(at));
        if let Some(theirs) = running.or(waiting) {
            mcp.skipped.push(Skipped { name: theirs.name, origin: theirs.origin, why: "your own mcp.json has a server of that name".into() });
        }
        mcp.off.retain(|off| off.name != *name);
        mcp.skipped.retain(|skipped| !(skipped.name == *name && skipped.origin == Origin::Yours));
        say!("{pad}{} {name} {} {file}", s.green("✓"), if replaced { "replaced the one in" } else { "added to" });
    }
    for server in added {
        restart(server, mcp, agent, terminal).await?;
    }
    say!("{pad}{}", s.dim("The model is offered its tools from your next message. Each asks before it runs, unless you let it in /mcp."));
    Ok(())
}

/// Starts `server`, in place of one of its name, and waits to say how it
/// did, offering what would fix it if it didn't.
async fn restart(server: Server, mcp: &Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let name = server.name.clone();
    agent.tools.hide_too(server.secrets(&|variable| mcp.servers.keys().get(variable)));
    mcp.servers.add(server);
    agent.servers_changed();
    settle(&name, mcp, agent, terminal).await
}

/// What would let a server that didn't start, start.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fix {
    /// The variable it needs, given.
    Give(String),
    /// Room the sandbox keeps from it: running it outside.
    Outside,
}

impl Fix {
    /// The fix for `failure` of `server`, if there's one rein can offer. A
    /// project's server is never let out of the sandbox: the project
    /// doesn't decide that, and you can only change your own.
    fn of(server: &Server, failure: &Failure) -> Option<Fix> {
        match failure {
            Failure::Needs(name) => Some(Fix::Give(name.clone())),
            Failure::InWalls(_) | Failure::NoSandbox if server.origin == Origin::Yours && server.access != Access::Outside => Some(Fix::Outside),
            Failure::InWalls(_) | Failure::NoSandbox | Failure::Other(_) => None,
        }
    }
}

/// Waits for the server called `name` to start, and says how it did. One
/// that didn't is offered its fix, if there's one, and started again: each
/// fix once, so one that didn't help isn't offered again and again.
async fn settle(name: &str, mcp: &Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let mut offered = Vec::new();
    loop {
        let servers = mcp.servers.clone();
        let started = crate::conversation::waiting(terminal, &format!("Starting {name}"), async { Ok(servers.started(name).await) }).await?;
        let status = match started {
            Some(Some(status)) => status,
            Some(None) => return Ok(()),
            None => {
                say!("{pad}{name}  {}", s.dim("still starting: /mcp says how it's doing"));
                return Ok(());
            }
        };
        say!("{pad}{name}  {}", how(&status, s));
        let (Some(server), Status::Failed(failure)) = (mcp.servers.server(name), status) else { return Ok(()) };
        let Some(fix) = Fix::of(&server, &failure).filter(|fix| !offered.contains(fix)) else { return Ok(()) };
        offered.push(fix.clone());
        let fixed = match fix {
            Fix::Give(_) => {
                let given = give_keys(&server, mcp.servers.keys(), &mcp.dir, terminal).await;
                (!given.is_empty()).then(|| agent.tools.hide_too(given)).map(|()| server)
            }
            Fix::Outside if let_out(&server, terminal).await => Some(set_access(&server, Access::Outside, mcp)?),
            Fix::Outside => None,
        };
        let Some(server) = fixed else { return Ok(()) };
        mcp.servers.add(server);
        agent.servers_changed();
    }
}

/// Asks whether to run your `server`, which didn't start in the sandbox,
/// outside it, saying what that gives it. Only yes is yes.
async fn let_out(server: &Server, terminal: &mut Terminal) -> bool {
    let question = Question {
        title: format!("Run {} outside the sandbox?", server.name),
        detail: Some(
            "It didn't start in the sandbox, which keeps it from your home folder, other apps and the screen: that may be why. Outside, it has your full access: your files, keys, apps and screen."
                .into(),
        ),
        command: None,
        answers: vec![("y", "Yes, run it outside".to_owned()), ("n", "No, leave it in the sandbox".to_owned())],
    };
    yes(question, terminal).await
}

/// Asks for each variable `server` needs that isn't set, in dots, and
/// keeps what's given in rein's folder for next time: the values given,
/// which are to be hidden. Esc, or nothing typed, stops asking.
async fn give_keys(server: &Server, keys: &Keys, dir: &Path, terminal: &mut Terminal) -> Vec<String> {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let (mut given, mut asked) = (Vec::new(), Vec::new());
    while let Some(name) = server.needs(&|variable| keys.get(variable)) {
        // One given that it still needs isn't asked for again and again.
        if asked.contains(&name) {
            break;
        }
        asked.push(name.clone());
        say!("{pad}{} needs {name} to start, and it isn't set.", server.name);
        let value = match terminal.input.read_secret(&format!("Paste {name} (kept in rein's folder, never shown; Esc skips)")).await {
            Some(Read::Line(value)) if !value.trim().is_empty() => value.trim().to_owned(),
            Some(_) => break,
            None => {
                say!("{pad}{}", s.dim(&format!("Set {name} in the shell you start rein from, or start rein in a terminal to paste it here.")));
                break;
            }
        };
        keys.set(&name, &value);
        match keep_key(dir, &name, &value) {
            Ok(()) => say!("{pad}{} {name} is kept for {}", s.green("✓"), server.name),
            Err(e) => say!("{pad}{}", s.red(&format!("✗ can't keep {name} for next time: {}", describe(e)))),
        }
        given.push(value);
    }
    given
}

/// The values you gave rein for servers' variables.
fn given_keys(dir: &Path) -> BTreeMap<String, String> {
    rein_llm::store::read_json(&dir.join(KEYS_FILE)).ok().flatten().unwrap_or_default()
}

/// Keeps `value` for the variable `name`, with the others, in a file only
/// you can read.
fn keep_key(dir: &Path, name: &str, value: &str) -> rein_llm::Result<()> {
    let mut keys = given_keys(dir);
    keys.insert(name.to_owned(), value.to_owned());
    rein_llm::store::write_json_private(&dir.join(KEYS_FILE), &keys)
}

/// Your server's entry in your file's `servers`, to change.
fn entry<'a>(servers: &'a mut serde_json::Map<String, Value>, name: &str) -> CliResult<&'a mut serde_json::Map<String, Value>> {
    servers.get_mut(name).and_then(Value::as_object_mut).ok_or_else(|| format!("{name} isn't in your mcp.json any more"))
}

/// Gives your `server` other walls, in your file: the server as it is now.
fn set_access(server: &Server, access: Access, mcp: &Mcp) -> CliResult<Server> {
    edit(&mcp.dir, |servers| {
        let entry = entry(servers, &server.name)?;
        if access == Access::Network {
            entry.remove("access");
        } else {
            entry.insert("access".into(), json!(access.as_str()));
        }
        Ok(())
    })?;
    Ok(Server { access, ..server.clone() })
}

/// Stops your `server`, keeping it in your file, turned off.
fn turn_off(server: Server, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    edit(&mcp.dir, |servers| {
        entry(servers, &server.name)?.insert("disabled".into(), json!(true));
        Ok(())
    })?;
    mcp.servers.remove(&server.name);
    agent.servers_changed();
    say!("{}{} {} is off: it stays in {}, and /mcp turns it on", " ".repeat(GUTTER), terminal.err.green("✓"), server.name, shown_file(&mcp.dir));
    mcp.off.push(server);
    Ok(())
}

/// Stops the server called `name` and takes it out of your file; one of
/// the project's, rein can't take out of its file, so your yes to it goes.
fn remove(name: &str, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let running = mcp.servers.server(name);
    if let Some(theirs) = running.clone().filter(|server| server.origin == Origin::Project) {
        forget(&mcp.dir, &mcp.project(), name).map_err(describe)?;
        mcp.servers.remove(name);
        mcp.waiting.push(theirs);
        say!("{pad}{} {name} is stopped, and your yes to it taken back: it's this project's, so rein asks again when it starts here", s.green("✓"));
    } else if running.is_none() && mcp.waiting.iter().any(|theirs| theirs.name == name) {
        return Err(format!("{name} is this project's, and isn't started: rein can't take it out of its .mcp.json"));
    } else {
        let in_file = edit(&mcp.dir, |servers| Ok(servers.remove(name).is_some()))?;
        if !in_file && running.is_none() {
            return Err(format!("there's no MCP server called {name} running or in your mcp.json; /mcp lists them"));
        }
        mcp.servers.remove(name);
        mcp.off.retain(|off| off.name != name);
        mcp.skipped.retain(|skipped| !(skipped.name == name && skipped.origin == Origin::Yours));
        say!("{pad}{} {name} is stopped, and out of {}", s.green("✓"), shown_file(&mcp.dir));
    }
    agent.servers_changed();
    Ok(())
}

/// `/mcp allow`: lets `tools` of your server `name` run without asking, or
/// all of them for `*`, along with those it let before.
fn allow(name: &str, tools: &[String], mcp: &Mcp, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    let Some((server, status)) = mcp.servers.statuses().into_iter().find(|(server, _)| server.name == name) else {
        return Err(format!("there's no MCP server called {name}; /mcp lists them"));
    };
    if server.origin == Origin::Project {
        return Err(format!("{name} is this project's, and a project's tools always ask, as it may be anyone's; /mcp add it as yours to let them run"));
    }
    // A name it doesn't list is a typo, once it has listed its tools.
    if let Status::Ready(listed) = &status {
        let unknown: Vec<&str> = tools.iter().map(String::as_str).filter(|tool| *tool != "*" && !listed.iter().any(|listed| listed == tool)).collect();
        if !unknown.is_empty() {
            return Err(format!("{name} has no tool called {}; its tools are {}", unknown.join(" or "), listed.join(", ")));
        }
    }
    let mut allow = server.allow;
    for tool in tools {
        if !allow.contains(tool) {
            allow.push(tool.clone());
        }
    }
    set_allow(name, &allow, mcp, agent, terminal)
}

/// Lets `allow`, tools of your server `name`, run without asking, and no
/// others, from the next message on: kept in your file.
fn set_allow(name: &str, allow: &[String], mcp: &Mcp, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    edit(&mcp.dir, |servers| {
        let entry = entry(servers, name)?;
        if allow.is_empty() {
            entry.remove("allow");
        } else {
            entry.insert("allow".into(), json!(allow));
        }
        Ok(())
    })?;
    mcp.servers.allow(name, allow);
    agent.servers_changed();
    let which = match allow {
        [] => format!("each of {name}'s tools asks before it runs"),
        _ if allow.iter().any(|tool| tool == "*") => format!("all of {name}'s tools run without asking"),
        _ => format!("these of {name}'s tools run: {} without asking", allow.join(", ")),
    };
    say!("{}{} {which} now, from your next message", " ".repeat(GUTTER), terminal.err.green("✓"));
    Ok(())
}

/// The tools `allow` lets run without asking, in a few words.
fn allowed(allow: &[String]) -> String {
    match allow {
        [] => "none; each asks".into(),
        _ if allow.iter().any(|tool| tool == "*") => "all of them".into(),
        _ => allow.join(", "),
    }
}

/// Changes the servers in your file with `change`, leaving all else in it
/// as it was, and writes it whole. One rein can't read is left as it is.
fn edit<T>(dir: &Path, change: impl FnOnce(&mut serde_json::Map<String, Value>) -> CliResult<T>) -> CliResult<T> {
    let (path, shown) = (dir.join(FILE), shown_file(dir));
    let mut file: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{shown} isn't JSON rein can read ({e}), so rein leaves it as it is"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("can't read {shown}: {e}")),
    };
    if let Some(file) = file.as_object_mut() {
        file.entry("mcpServers").or_insert_with(|| json!({}));
    }
    let before = file.clone();
    let Some(Value::Object(servers)) = file.get_mut("mcpServers") else { return Err(format!("{shown} has no mcpServers rein can read, so rein leaves it as it is")) };
    let done = change(servers)?;
    if file != before {
        rein_llm::store::write_json_private(&path, &file).map_err(describe)?;
    }
    Ok(done)
}

/// The servers a file names, or none, said why, if it can't be read.
fn read(path: &Path, origin: Origin) -> Named {
    let shown = match origin {
        Origin::Yours => path.display().to_string(),
        Origin::Project => PROJECT_FILE.to_owned(),
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Named::default(),
        Err(e) => {
            say!("✗ can't read {shown}: {e}; its MCP servers aren't started");
            return Named::default();
        }
    };
    rein_mcp::config::read(&text, origin).unwrap_or_else(|e| {
        say!("✗ {shown} {e}; its MCP servers aren't started");
        Named::default()
    })
}

/// Asks whether to start a project's `server`, saying what it runs, with
/// what, and in which walls, and keeps a yes in the project `key`.
async fn say_yes(server: &Server, dir: &Path, key: &str, terminal: &mut Terminal) -> bool {
    let mut detail = format!("It runs {}", rein_mcp::walls(server.access));
    let takes = server.takes();
    if !takes.is_empty() {
        detail.push_str(&format!(", and gets your {}", takes.join(", ")));
    }
    detail.push('.');
    let question = Question {
        title: format!("Start {}, an MCP server this project's .mcp.json names?", server.name),
        detail: Some(detail),
        command: Some(server.command_line()),
        answers: vec![("y", "Yes, start it".to_owned()), ("n", "No, not this time".to_owned())],
    };
    if !yes(question, terminal).await {
        return false;
    }
    if let Err(e) = approve(dir, key, server) {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ can't keep that yes: {e}")));
    }
    true
}

/// Asks `question`, whose answers are `y` and `n`: in the box, or on a
/// line of its own without one. Only yes is yes.
async fn yes(question: Question, terminal: &mut Terminal) -> bool {
    if !terminal.input.pinned() {
        let pad = " ".repeat(GUTTER);
        say!("{}", question.title);
        if let Some(command) = &question.command {
            say!("{pad}{command}");
        }
        if let Some(detail) = &question.detail {
            say!("{pad}{}", terminal.err.dim(detail));
        }
        return matches!(terminal.input.read(&format!("{pad}[y]es  [n]o › "), false).await, Read::Line(answer) if matches!(answer.trim(), "y" | "Y" | "yes"));
    }
    matches!(terminal.input.ask(question).await, Read::Line(key) if key == "y")
}

/// The project servers you said yes to, by project: each as it was then.
fn approved(dir: &Path) -> BTreeMap<String, Vec<String>> {
    rein_llm::store::read_json(&dir.join(APPROVED_FILE)).ok().flatten().unwrap_or_default()
}

/// Keeps your yes to `server` in the project `key`, as it is now: a
/// change to it asks again.
fn approve(dir: &Path, key: &str, server: &Server) -> rein_llm::Result<()> {
    let mut approved = approved(dir);
    let yes = approved.entry(key.to_owned()).or_default();
    yes.retain(|fingerprint| named(fingerprint).as_deref() != Some(&server.name));
    yes.push(server.fingerprint());
    rein_llm::store::write_json_private(&dir.join(APPROVED_FILE), &approved)
}

/// Takes back your yes to the project `key`'s server called `name`.
fn forget(dir: &Path, key: &str, name: &str) -> rein_llm::Result<()> {
    let mut approved = approved(dir);
    if let Some(yes) = approved.get_mut(key) {
        yes.retain(|fingerprint| named(fingerprint).as_deref() != Some(name));
    }
    rein_llm::store::write_json_private(&dir.join(APPROVED_FILE), &approved)
}

/// The name of the server a yes was to.
fn named(fingerprint: &str) -> Option<String> {
    serde_json::from_str::<Value>(fingerprint).ok()?.get("name")?.as_str().map(str::to_owned)
}

/// Where servers keep what they fetch as they start, a folder each: in
/// the cache folder, which commands may read but not write.
fn caches() -> PathBuf {
    let cache = rein_agent::sandbox::secrets::get("XDG_CACHE_HOME").map(PathBuf::from).filter(|path| path.is_absolute());
    let cache = cache.or_else(|| std::env::home_dir().map(|home| home.join(".cache"))).unwrap_or_else(std::env::temp_dir);
    cache.join("rein").join("mcp")
}

/// How a server is doing, in a few words.
fn how(status: &Status, s: Style) -> String {
    match status {
        Status::Starting => s.dim("starting…"),
        Status::Ready(tools) if tools.is_empty() => "no tools".to_owned(),
        Status::Ready(tools) => format!("{} tool{}: {}", tools.len(), if tools.len() == 1 { "" } else { "s" }, tools.join(", ")),
        Status::Stopped => s.yellow("stopped; it starts again on its next call"),
        Status::Failed(why) => s.red(&format!("didn't start: {why}")),
    }
}

/// Your own file, in rein's folder `dir`, as you'd find it.
fn shown_file(dir: &Path) -> String {
    let file = dir.join(FILE);
    match std::env::home_dir().and_then(|home| file.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => file.display().to_string(),
    }
}
