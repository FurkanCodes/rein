//! The MCP servers rein starts: yours, from `mcp.json` in rein's folder,
//! and a project's, from its `.mcp.json`, each only once you've said yes to
//! it as it is. The files have the shape Claude Code's and Cursor's do (see
//! [`rein_mcp::config`]); `/mcp` says how each server is doing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rein_agent::sandbox::Sandbox;
use rein_mcp::{Named, Origin, Server, Servers, Skipped, Status};

use crate::input::{Question, Read};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// Your own servers, in rein's folder.
pub(crate) const FILE: &str = "mcp.json";

/// A project's, as Claude Code and others name it.
pub(crate) const PROJECT_FILE: &str = ".mcp.json";

/// The project servers you said yes to, as each was then, by project.
const APPROVED_FILE: &str = "mcp-approved.json";

/// The servers rein started, and those it didn't, with why.
#[derive(Default)]
pub(crate) struct Mcp {
    pub(crate) servers: Option<Arc<Servers>>,
    pub(crate) skipped: Vec<Skipped>,
}

impl Mcp {
    /// What the servers' environments hold that rein hides wherever it
    /// shows up: the values of your variables they get, and keys.
    pub(crate) fn secrets(&self) -> Vec<String> {
        let servers = self.servers.iter().flat_map(|servers| servers.servers());
        servers.flat_map(|server| server.secrets(&rein_agent::sandbox::secrets::get)).collect()
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
    let servers = (!servers.is_empty()).then(|| Arc::new(Servers::start(servers, launch)));
    Mcp { servers, skipped }
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
    yes.retain(|fingerprint| serde_json::from_str::<serde_json::Value>(fingerprint).ok().and_then(|f| f["name"].as_str().map(str::to_owned)).as_deref() != Some(&server.name));
    yes.push(server.fingerprint());
    rein_llm::store::write_json_private(&dir.join(APPROVED_FILE), &approved)
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
    let statuses = mcp.servers.as_deref().map(Servers::statuses).unwrap_or_default();
    if statuses.is_empty() && mcp.skipped.is_empty() {
        say!("{pad}No MCP servers. Add them to {} or a project's {PROJECT_FILE}, as a server's README gives them (mcpServers).", shown_file());
        return;
    }
    let width = statuses.iter().map(|(server, _)| server.name.len()).chain(mcp.skipped.iter().map(|skipped| skipped.name.len())).max().unwrap_or(0);
    for (server, status) in statuses {
        let walls = match server.access.walls() {
            Some(_) => format!("in the sandbox ({})", server.access),
            None => "outside the sandbox".to_owned(),
        };
        let how = match status {
            Status::Starting => s.dim("starting…"),
            Status::Ready(tools) if tools.is_empty() => "no tools".to_owned(),
            Status::Ready(tools) => format!("{} tool{}: {}", tools.len(), if tools.len() == 1 { "" } else { "s" }, tools.join(", ")),
            Status::Stopped => s.yellow("stopped; it starts again on its next call"),
            Status::Failed(why) => s.red(&format!("didn't start: {why}")),
        };
        let line = format!("{pad}{:<width$}  {}  {how}", server.name, s.dim(&format!("{} · {walls}", server.origin.describe())));
        say!("{}", crate::ui::style::fit(&line, terminal.wrap_width().unwrap_or(100)));
    }
    for skipped in &mcp.skipped {
        say!("{pad}{:<width$}  {}  {}", skipped.name, s.dim(skipped.origin.describe()), s.dim(&format!("not started: {}", skipped.why)));
    }
    say!("{pad}{}", s.dim("The model sees a server's tools as mcp__<server>__<tool>, from a conversation's first message on."));
}

/// Your own file, as you'd find it.
fn shown_file() -> String {
    let file = crate::config_dir().map(|dir| dir.join(FILE)).unwrap_or_else(|_| PathBuf::from(FILE));
    match std::env::home_dir().and_then(|home| file.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => file.display().to_string(),
    }
}
