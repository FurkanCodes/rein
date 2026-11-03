//! The MCP servers rein starts: yours, from `mcp.json` in rein's folder,
//! and a project's, from its `.mcp.json`, each only once you've said yes to
//! it as it is. The files have the shape Claude Code's and Cursor's do (see
//! [`rein_mcp::config`]). `/mcp` says how each server is doing, and adds,
//! removes and allows them as rein runs, writing your file, so you needn't.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rein_agent::agent::{Agent, Llm};
use rein_agent::sandbox::Sandbox;
use rein_mcp::{Named, Origin, Server, Servers, Skipped, Status};
use serde_json::{Value, json};

use crate::commands::Mcp as Command;
use crate::input::{Question, Read};
use crate::terminal::Terminal;
use crate::ui::style::{GUTTER, Style};
use crate::{CliResult, describe};

/// Your own servers, in rein's folder.
pub(crate) const FILE: &str = "mcp.json";

/// A project's, as Claude Code and others name it.
pub(crate) const PROJECT_FILE: &str = ".mcp.json";

/// The project servers you said yes to, as each was then, by project.
const APPROVED_FILE: &str = "mcp-approved.json";

/// The servers rein started, and those it didn't, with why.
pub(crate) struct Mcp {
    /// Those started, which `/mcp add` adds to.
    pub(crate) servers: Arc<Servers>,
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
        self.servers.servers().iter().flat_map(|server| server.secrets(&rein_agent::sandbox::secrets::get)).collect()
    }
}

/// Reads your servers and the project at `root`'s, asks about each of the
/// project's you haven't said yes to as it is now (when `asks`: there's
/// someone to ask), and starts them all in the background.
pub(crate) async fn open(dir: &Path, root: &Path, sandbox: Arc<Sandbox>, terminal: &mut Terminal, asks: bool) -> Mcp {
    let named = read(&dir.join(FILE), Origin::Yours).and(read(&root.join(PROJECT_FILE), Origin::Project));
    let approved = approved(dir);
    let key = root.display().to_string();
    let mut skipped = named.skipped;
    let mut servers = Vec::new();
    for server in named.servers {
        let said_yes = server.origin == Origin::Yours || approved.get(&key).is_some_and(|yes| yes.contains(&server.fingerprint()));
        if said_yes {
            servers.push(server);
            continue;
        }
        if !asks {
            skipped.push(Skipped { name: server.name, origin: server.origin, why: "you haven't said yes to it: rein asks when it starts here in a conversation".into() });
            continue;
        }
        if ask(&server, terminal).await {
            if let Err(e) = approve(dir, &key, &server) {
                say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ can't keep that yes: {e}")));
            }
            servers.push(server);
        } else {
            skipped.push(Skipped { name: server.name, origin: server.origin, why: "you said no; rein asks again when it starts here".into() });
        }
    }
    if !asks && !skipped.is_empty() {
        let names: Vec<&str> = skipped.iter().map(|skipped| skipped.name.as_str()).collect();
        say!("{}", terminal.err.dim(&format!("(MCP servers not started: {}; rein says why in a conversation, with /mcp)", names.join(", "))));
    } else if !skipped.is_empty() {
        let count = skipped.len();
        say!("{}", terminal.err.dim(&format!("({count} MCP server{} not started: /mcp says why)", if count == 1 { "" } else { "s" })));
    }
    let launch = rein_mcp::Launch { sandbox, root: root.to_owned(), caches: caches() };
    Mcp { servers: Arc::new(Servers::start(servers, launch)), skipped, dir: dir.to_owned(), root: root.to_owned() }
}

/// `/mcp`, and what follows it.
pub(crate) async fn command(command: Command, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    match command {
        Command::Show => show(mcp, terminal),
        Command::Add(named) => add(named, mcp, agent, terminal).await?,
        Command::Remove(name) => remove(&name, mcp, agent, terminal)?,
        Command::Allow(name, tools) => allow(&name, &tools, mcp, agent, terminal)?,
    }
    Ok(())
}

/// Puts `named`'s servers in your file, each in place of one of its name,
/// and starts them, waiting to say how each did: whether it works is what
/// you'd want to know next.
async fn add(named: Named, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    for skipped in &named.skipped {
        say!("{pad}{}", s.red(&format!("✗ {} isn't added: {}", skipped.name, skipped.why)));
    }
    if named.servers.is_empty() {
        return if named.skipped.is_empty() { Err("there's no server in that to add".into()) } else { Ok(()) };
    }
    let replaced = edit(&mcp.dir, |servers| Ok(named.servers.iter().map(|server| servers.insert(server.name.clone(), server.entry()).is_some()).collect::<Vec<_>>()))?;
    let file = shown_file(&mcp.dir);
    for (server, replaced) in named.servers.iter().zip(replaced) {
        let name = &server.name;
        // Yours is the one you meant, as when both files name it.
        if let Some(theirs) = mcp.servers.servers().into_iter().find(|running| running.name == *name && running.origin == Origin::Project) {
            mcp.skipped.push(Skipped { name: theirs.name, origin: theirs.origin, why: "your own mcp.json has a server of that name".into() });
        }
        mcp.skipped.retain(|skipped| !(skipped.name == *name && skipped.origin == Origin::Yours));
        agent.tools.hide_too(server.secrets(&rein_agent::sandbox::secrets::get));
        mcp.servers.add(server.clone());
        say!("{pad}{} {name} {} {file}", s.green("✓"), if replaced { "replaced the one in" } else { "added to" });
    }
    agent.servers_changed();
    for server in &named.servers {
        let servers = mcp.servers.clone();
        let started = crate::conversation::waiting(terminal, &format!("Starting {}", server.name), async { Ok(servers.started(&server.name).await) }).await?;
        let how = match started {
            Some(Some(status)) => how(&status, s),
            Some(None) => continue,
            None => s.dim("still starting: /mcp says how it's doing"),
        };
        say!("{pad}{}  {how}", server.name);
    }
    say!("{pad}{}", s.dim("The model is offered its tools from your next message. Each asks before it runs, unless you /mcp allow it."));
    Ok(())
}

/// Stops the server called `name` and takes it out of your file; one of
/// the project's, rein can't take out of its file, so your yes to it goes.
fn remove(name: &str, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let origin = mcp.servers.servers().into_iter().find(|server| server.name == name).map(|server| server.origin);
    if origin == Some(Origin::Project) {
        forget(&mcp.dir, &mcp.root.display().to_string(), name).map_err(describe)?;
        mcp.servers.remove(name);
        mcp.skipped.push(Skipped { name: name.to_owned(), origin: Origin::Project, why: "you took back your yes; rein asks again when it starts here".into() });
        say!("{pad}{} {name} is stopped, and your yes to it taken back: it's this project's, so rein asks again when it starts here", s.green("✓"));
    } else {
        let in_file = edit(&mcp.dir, |servers| Ok(servers.remove(name).is_some()))?;
        if !in_file && origin.is_none() {
            return Err(format!("there's no MCP server called {name} running or in your mcp.json; /mcp lists them"));
        }
        mcp.servers.remove(name);
        mcp.skipped.retain(|skipped| !(skipped.name == name && skipped.origin == Origin::Yours));
        say!("{pad}{} {name} is stopped, and out of {}", s.green("✓"), shown_file(&mcp.dir));
    }
    agent.servers_changed();
    Ok(())
}

/// Lets `tools` of your server `name` run without asking, or all of them
/// for `*`, from the next message on: kept in your file.
fn allow(name: &str, tools: &[String], mcp: &Mcp, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
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
    let file = shown_file(&mcp.dir);
    edit(&mcp.dir, |servers| {
        let entry = servers.get_mut(name).and_then(Value::as_object_mut).ok_or_else(|| format!("{name} isn't in {file} any more"))?;
        entry.insert("allow".into(), json!(allow));
        Ok(())
    })?;
    mcp.servers.allow(name, &allow);
    agent.servers_changed();
    let which = if allow.iter().any(|tool| tool == "*") { format!("all of {name}'s tools run") } else { format!("these of {name}'s tools run: {}", allow.join(", ")) };
    say!("{pad}{} {which} without asking now, from your next message", s.green("✓"));
    Ok(())
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
/// what, and in which walls. Only yes is yes.
async fn ask(server: &Server, terminal: &mut Terminal) -> bool {
    let title = format!("Start {}, an MCP server this project's .mcp.json names?", server.name);
    let mut detail = format!("It runs {}", rein_mcp::walls(server.access));
    let takes = server.takes();
    if !takes.is_empty() {
        detail.push_str(&format!(", and gets your {}", takes.join(", ")));
    }
    detail.push('.');
    let command = server.command_line();
    if !terminal.input.pinned() {
        let pad = " ".repeat(GUTTER);
        say!("{title}");
        say!("{pad}{command}");
        say!("{pad}{}", terminal.err.dim(&detail));
        return matches!(terminal.input.read(&format!("{pad}Start it?  [y]es  [n]o › "), false).await, Read::Line(answer) if matches!(answer.trim(), "y" | "Y" | "yes"));
    }
    let answers = vec![("y", "Yes, start it".to_owned()), ("n", "No, not this time".to_owned())];
    matches!(terminal.input.ask(Question { title, detail: Some(detail), command: Some(command), answers }).await, Read::Line(key) if key == "y")
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

/// What `/mcp` says: each server, whose it is, its walls, and how it's
/// doing, then those not started, and why.
pub(crate) fn show(mcp: &Mcp, terminal: &Terminal) {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let statuses = mcp.servers.statuses();
    if statuses.is_empty() && mcp.skipped.is_empty() {
        say!("{pad}No MCP servers yet. To add one: {}", crate::commands::ADD);
        say!("{pad}{}", s.dim("Like: /mcp add github GITHUB_PERSONAL_ACCESS_TOKEN=$GITHUB_TOKEN npx -y @modelcontextprotocol/server-github"));
        return;
    }
    let width = statuses.iter().map(|(server, _)| server.name.len()).chain(mcp.skipped.iter().map(|skipped| skipped.name.len())).max().unwrap_or(0);
    for (server, status) in statuses {
        let walls = match server.access.walls() {
            Some(_) => format!("in the sandbox ({})", server.access),
            None => "outside the sandbox".to_owned(),
        };
        let line = format!("{pad}{:<width$}  {}  {}", server.name, s.dim(&format!("{} · {walls}", server.origin.describe())), how(&status, s));
        say!("{}", crate::ui::style::fit(&line, terminal.wrap_width().unwrap_or(100)));
    }
    for skipped in &mcp.skipped {
        say!("{pad}{:<width$}  {}  {}", skipped.name, s.dim(skipped.origin.describe()), s.dim(&format!("not started: {}", skipped.why)));
    }
    say!("{pad}{}", s.dim("The model sees a server's tools as mcp__<server>__<tool>."));
    say!("{pad}{}", s.dim("/mcp add <name> <command> [args …] adds one · /mcp remove <name> · /mcp allow <name> <tool …|*>"));
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
