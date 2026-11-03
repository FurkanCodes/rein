//! `/mcp import`: your servers from other apps' settings (see
//! [`rein_mcp::import`]), each to pick or leave, then added as yours and
//! started, as `/mcp add` does. What rein has already, as it is, isn't
//! offered again.

use std::path::{Path, PathBuf};

use rein_agent::agent::{Agent, Llm};
use rein_mcp::import::App;
use rein_mcp::{Named, Origin, Server, Skipped};

use super::{Mcp, add};
use crate::CliResult;
use crate::input::Menu;
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// A server another app has, to bring in.
struct Theirs {
    app: App,
    server: Server,
    /// That app has it turned off.
    off: bool,
}

/// Finds your servers in other apps' settings, lets you pick which to
/// bring in, and adds those.
pub(super) async fn import(mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let home = std::env::home_dir().ok_or("can't find your home folder, where other apps keep their settings")?;
    let (mut theirs, left, read) = gather(&home, &mcp.root, terminal);
    let yours: Vec<Server> = mcp.servers.servers().into_iter().filter(|server| server.origin == Origin::Yours).chain(mcp.off.iter().cloned()).collect();
    // What rein has as it is, or what two apps have alike, is offered once.
    let mut seen: Vec<(String, String)> = yours.iter().map(|server| (server.name.clone(), server.entry().to_string())).collect();
    theirs.retain(|theirs| {
        let key = (theirs.server.name.clone(), theirs.server.entry().to_string());
        let new = !seen.contains(&key);
        seen.push(key);
        new
    });
    for (app, skipped) in &left {
        say!("{pad}{}", s.dim(&format!("{} from {} isn't brought in: {}", skipped.name, app.title(), skipped.why)));
    }
    if theirs.is_empty() {
        let why = if read.is_empty() { "no other app's settings here have MCP servers in them".to_owned() } else { format!("rein has every server {} have", list(&read)) };
        say!("{pad}Nothing to bring in: {why}.");
        return Ok(());
    }
    let Some(picked) = pick(&theirs, &yours, terminal).await else { return Ok(()) };
    if picked.is_empty() {
        return Ok(());
    }
    let servers = theirs.into_iter().enumerate().filter(|(i, _)| picked.contains(i)).map(|(_, theirs)| theirs.server).collect();
    add(Named { servers, ..Named::default() }, mcp, agent, terminal).await
}

/// The servers other apps keep for you, those they have that rein can't
/// start, and which apps' settings were read.
fn gather(home: &Path, root: &Path, terminal: &Terminal) -> (Vec<Theirs>, Vec<(App, Skipped)>, Vec<App>) {
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    let (mut theirs, mut left, mut read) = (Vec::new(), Vec::new(), Vec::new());
    for app in App::ALL {
        let path = app.file(home, codex_home.as_deref());
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ can't read {}'s {}: {e}", app.title(), path.display())));
                continue;
            }
        };
        match app.read(&text, home, root) {
            Ok(named) => {
                read.push(app);
                theirs.extend(named.servers.into_iter().map(|server| Theirs { app, server, off: false }));
                theirs.extend(named.off.into_iter().map(|server| Theirs { app, server, off: true }));
                left.extend(named.skipped.into_iter().map(|skipped| (app, skipped)));
            }
            Err(e) => say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ {}'s {} {e}", app.title(), path.display()))),
        }
    }
    (theirs, left, read)
}

/// Lets you turn each of `theirs` on and off to bring in, until you're
/// done: which, or `None` if you'd rather not. Each starts on, but one
/// that would take the place of one of `yours`, one that app has off, and
/// a second of a name; picking one of a name puts the others of it off.
async fn pick(theirs: &[Theirs], yours: &[Server], terminal: &mut Terminal) -> Option<Vec<usize>> {
    let replaces = |server: &Server| yours.iter().any(|yours| yours.name == server.name);
    let mut on: Vec<bool> =
        theirs.iter().enumerate().map(|(i, one)| !one.off && !replaces(&one.server) && !theirs[..i].iter().any(|other| other.server.name == one.server.name)).collect();
    let mut at = None;
    loop {
        let count = on.iter().filter(|on| **on).count();
        let mut items = vec![("done".to_owned(), format!("bring in the {count} marked"))];
        items.extend(theirs.iter().zip(&on).map(|(one, on)| {
            let mark = if *on { "✓" } else { " " };
            let off = if one.off { format!(" · off in {}", one.app.title()) } else { String::new() };
            let instead = if replaces(&one.server) { " · in place of yours" } else { "" };
            (format!("{mark} {}", one.server.name), format!("from {}{off}{instead} · {}", one.app.title(), one.server.described()))
        }));
        let picked = terminal.input.choose(Menu { title: "bring in which servers".into(), items, current: None, at }).await?;
        let Some(i) = picked.checked_sub(1) else { break };
        on[i] = !on[i];
        if on[i] {
            for (j, other) in theirs.iter().enumerate() {
                if j != i && other.server.name == theirs[i].server.name {
                    on[j] = false;
                }
            }
        }
        at = Some(picked);
    }
    Some(on.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i).collect())
}

/// `apps` by name, as a sentence lists them.
fn list(apps: &[App]) -> String {
    let names: Vec<&str> = apps.iter().map(|app| app.title()).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}
