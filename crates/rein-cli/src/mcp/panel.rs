//! `/mcp`: how each server is doing, then the one to change, and how.
//! What would fix one that didn't start comes first.

use rein_agent::agent::{Agent, Llm};
use rein_agent::sandbox::Access;
use rein_mcp::{Failure, Origin, Reach, Server, Skipped, Status};

use super::{Fix, Mcp, allowed, edit, entry, give_keys, how, remove, restart, say_yes, set_access, set_allow, sign_in, turn_off};
use crate::CliResult;
use crate::input::Menu;
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// A server as `/mcp` shows it.
enum Row {
    /// Started, how it's doing, and what else is known of it.
    Running(Server, Status, Also),
    /// Yours, turned off.
    Off(Server),
    /// The project's, waiting for your yes.
    Waiting(Server),
    /// One rein can't start, and why.
    Skipped(Skipped),
}

/// What else is known of a server started: of one on this computer, that
/// a call failed in the sandbox, which may be why; of one at a URL, that
/// you're signed in to it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Also {
    Nothing,
    FailedInWalls,
    SignedIn,
}

impl Row {
    fn name(&self) -> &str {
        match self {
            Row::Running(server, ..) | Row::Off(server) | Row::Waiting(server) => &server.name,
            Row::Skipped(skipped) => &skipped.name,
        }
    }

    /// How it's doing in a few words, for the list to pick from.
    fn about(&self) -> String {
        match self {
            Row::Running(_, _, Also::FailedInWalls) => "● a call failed in the sandbox".into(),
            Row::Running(_, Status::Ready(tools), _) => format!("● {} tool{}", tools.len(), if tools.len() == 1 { "" } else { "s" }),
            Row::Running(_, Status::Failed(Failure::SignIn(_)), _) => "✗ needs you to sign in".into(),
            Row::Running(_, Status::Starting, _) => "◌ starting".into(),
            Row::Running(_, Status::Stopped, _) => "◌ stopped; it starts again on its next call".into(),
            Row::Running(_, Status::Failed(Failure::Needs(name)), _) => format!("✗ needs {name}"),
            Row::Running(_, Status::Failed(Failure::InWalls(_)), _) => "✗ didn't start in the sandbox".into(),
            Row::Running(_, Status::Failed(Failure::NoSandbox), _) => "✗ no sandbox here to start it in".into(),
            Row::Running(_, Status::Failed(Failure::Other(_)), _) => "✗ didn't start".into(),
            Row::Off(_) => "– off".into(),
            Row::Waiting(_) => "⏸ waiting for your yes".into(),
            Row::Skipped(skipped) => format!("– can't start: {}", skipped.why),
        }
    }
}

/// Every server, as `/mcp` shows them: started, off, waiting, skipped.
fn rows(mcp: &Mcp) -> Vec<Row> {
    let running = mcp.servers.statuses().into_iter().map(|(server, status)| {
        let also = if mcp.servers.failed_in_walls(&server.name) {
            Also::FailedInWalls
        } else if url(&server, mcp).is_some_and(|url| mcp.tokens.signed_in(&url)) {
            Also::SignedIn
        } else {
            Also::Nothing
        };
        Row::Running(server, status, also)
    });
    let off = mcp.off.iter().cloned().map(Row::Off);
    let waiting = mcp.waiting.iter().cloned().map(Row::Waiting);
    running.chain(off).chain(waiting).chain(mcp.skipped.iter().cloned().map(Row::Skipped)).collect()
}

/// The URL `server` is reached at, filled in: what its sign-in is kept by.
fn url(server: &Server, mcp: &Mcp) -> Option<String> {
    match server.filled(&|variable| mcp.servers.keys().get(variable)) {
        Ok(Reach::Remote(remote)) => Some(remote.url),
        _ => None,
    }
}

/// What can be done with a server, from `/mcp`.
enum Action {
    /// Give it the variable it needs.
    Give(Server, String),
    /// Run it outside the sandbox, where it didn't start.
    Outside(Server),
    /// Sign in to it, where it said.
    SignIn(Server, String),
    /// Forget rein's sign-in to it.
    SignOut(Server),
    /// Choose which of these, its tools, run without asking.
    Tools(Server, Vec<String>),
    /// Choose its walls.
    Walls(Server),
    Restart(Server),
    Off(Server),
    On(Server),
    /// Stop it and take it out of your file, or take back your yes.
    Remove(String, Origin),
    /// Say yes to a project's server, and start it.
    Start(Server),
}

impl Action {
    /// What it's called in the list, and a few words about it.
    fn shown(&self) -> (String, String) {
        let (name, about) = match self {
            Action::Give(_, name) => (format!("give it {name}"), "kept in rein's folder, never shown".to_owned()),
            Action::Outside(_) => ("run it outside the sandbox".into(), "with your full access: your files, keys, apps and screen".into()),
            Action::SignIn(..) => ("sign in".into(), "in your browser".into()),
            Action::SignOut(_) => ("sign out".into(), "rein forgets its token, and asks you to sign in again".into()),
            Action::Tools(server, _) => ("choose the tools that run without asking".into(), format!("now: {}", allowed(&server.allow))),
            Action::Walls(server) => ("change its walls".into(), server.access().map_or_else(String::new, |access| format!("now: {access}"))),
            Action::Restart(_) => ("restart it".into(), String::new()),
            Action::Off(_) => ("turn it off".into(), "it stays in your mcp.json, to turn on later".into()),
            Action::On(_) => ("turn it on".into(), String::new()),
            Action::Remove(_, Origin::Yours) => ("remove it".into(), "it stops, and goes from your mcp.json".into()),
            Action::Remove(_, Origin::Project) => ("take back your yes".into(), "it stops, and rein asks again when it starts here".into()),
            Action::Start(_) => ("start it".into(), "you're asked first, seeing what it runs".into()),
        };
        (name, about)
    }
}

/// What can be done with `row`: first what would fix it, if it didn't
/// start.
fn actions(row: Row) -> Vec<Action> {
    match row {
        Row::Running(server, status, also) => {
            let mut actions = Vec::new();
            let fix = match &status {
                Status::Failed(failure) => Fix::of(&server, failure),
                // It started, but a call failed where the sandbox may be why.
                _ if also == Also::FailedInWalls => Fix::of(&server, &Failure::InWalls(String::new())),
                _ => None,
            };
            match fix {
                Some(Fix::Give(name)) => actions.push(Action::Give(server.clone(), name)),
                Some(Fix::Outside) => actions.push(Action::Outside(server.clone())),
                Some(Fix::SignIn(challenge)) => actions.push(Action::SignIn(server.clone(), challenge)),
                None => {}
            }
            if server.origin == Origin::Yours {
                if let Status::Ready(tools) = status
                    && !tools.is_empty()
                {
                    actions.push(Action::Tools(server.clone(), tools));
                }
                // One at a URL runs none of its code here: it has no walls.
                if server.access().is_some() {
                    actions.push(Action::Walls(server.clone()));
                }
            }
            if also == Also::SignedIn {
                actions.push(Action::SignOut(server.clone()));
            }
            actions.push(Action::Restart(server.clone()));
            if server.origin == Origin::Yours {
                actions.push(Action::Off(server.clone()));
            }
            actions.push(Action::Remove(server.name, server.origin));
            actions
        }
        Row::Off(server) => vec![Action::On(server.clone()), Action::Remove(server.name, Origin::Yours)],
        Row::Waiting(server) => vec![Action::Start(server)],
        Row::Skipped(skipped) if skipped.origin == Origin::Yours => vec![Action::Remove(skipped.name, Origin::Yours)],
        Row::Skipped(_) => Vec::new(),
    }
}

/// `/mcp`: how each server is doing, then, in a conversation's box, the one
/// to change, and how.
pub(super) async fn panel(mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    show(mcp, terminal);
    if !terminal.input.pinned() {
        return Ok(());
    }
    let rows = rows(mcp);
    // After the servers, a new one: looked up, or brought from other apps.
    let more =
        [("+ add a server", "look one up in the MCP Registry"), ("+ bring servers from other apps", "Claude Code, Claude Desktop, Cursor, Codex, VS Code, Windsurf, Gemini CLI")];
    let items = rows.iter().map(|row| (row.name().to_owned(), row.about())).chain(more.iter().map(|(name, about)| ((*name).to_owned(), (*about).to_owned()))).collect();
    let title = if rows.is_empty() { "add a server" } else { "change which server" };
    let Some(picked) = terminal.input.choose(Menu { title: title.into(), items, current: None, at: None }).await else { return Ok(()) };
    match picked.checked_sub(rows.len()) {
        Some(0) => return super::registry::find(None, mcp, agent, terminal).await,
        Some(_) => return super::import::import(mcp, agent, terminal).await,
        None => {}
    }
    let Some(row) = rows.into_iter().nth(picked) else { return Ok(()) };
    let name = row.name().to_owned();
    let mut actions = actions(row);
    if actions.is_empty() {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&format!("{name} is the project's, and rein can't start it: only its .mcp.json can change that")));
        return Ok(());
    }
    let items = actions.iter().map(Action::shown).collect();
    let Some(picked) = terminal.input.choose(Menu { title: format!("{name}: do what"), items, current: None, at: None }).await else { return Ok(()) };
    act(actions.swap_remove(picked), mcp, agent, terminal).await
}

async fn act(action: Action, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    match action {
        Action::Give(server, _) => {
            let given = give_keys(&server, mcp.servers.keys(), &mcp.dir, terminal).await;
            if given.is_empty() {
                return Ok(());
            }
            agent.tools.hide_too(given);
            restart(server, mcp, agent, terminal).await
        }
        Action::Outside(server) => {
            let server = set_access(&server, Access::Outside, mcp)?;
            restart(server, mcp, agent, terminal).await
        }
        Action::SignIn(server, challenge) => {
            if !sign_in(&server, &challenge, mcp, terminal).await? {
                return Ok(());
            }
            restart(server, mcp, agent, terminal).await
        }
        Action::SignOut(server) => {
            if let Some(url) = url(&server, mcp) {
                mcp.tokens.sign_out(&url).await.map_err(crate::describe)?;
                say!("{}{} rein forgot its sign-in to {}", " ".repeat(GUTTER), terminal.err.green("✓"), server.name);
            }
            restart(server, mcp, agent, terminal).await
        }
        Action::Tools(server, tools) => choose_tools(&server, &tools, mcp, agent, terminal).await,
        Action::Walls(server) => {
            let items = Access::ALL.iter().map(|access| (access.to_string(), rein_mcp::walls(*access).to_owned())).collect();
            let current = Access::ALL.iter().position(|access| Some(*access) == server.access());
            let menu = Menu { title: format!("{}: run it with which walls", server.name), items, current, at: None };
            let Some(access) = terminal.input.choose(menu).await.and_then(|i| Access::ALL.get(i).copied()).filter(|access| Some(*access) != server.access()) else { return Ok(()) };
            let server = set_access(&server, access, mcp)?;
            restart(server, mcp, agent, terminal).await
        }
        Action::Restart(server) => restart(server, mcp, agent, terminal).await,
        Action::Off(server) => turn_off(server, mcp, agent, terminal),
        Action::On(server) => {
            edit(&mcp.dir, |servers| {
                entry(servers, &server.name)?.remove("disabled");
                Ok(())
            })?;
            mcp.off.retain(|off| off.name != server.name);
            restart(server, mcp, agent, terminal).await
        }
        Action::Remove(name, _) => remove(&name, mcp, agent, terminal),
        Action::Start(server) => {
            if !say_yes(&server, &mcp.dir, &mcp.project(), terminal).await {
                return Ok(());
            }
            mcp.waiting.retain(|waiting| waiting.name != server.name);
            give_keys(&server, mcp.servers.keys(), &mcp.dir, terminal).await;
            restart(server, mcp, agent, terminal).await
        }
    }
}

/// Lets you turn each of `tools`, `server`'s, on and off to run without
/// asking, until you're done: then kept in your file. Esc keeps it as it was.
async fn choose_tools(server: &Server, tools: &[String], mcp: &Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let mut allow = server.allow.clone();
    let mut at = None;
    loop {
        let all = allow.iter().any(|tool| tool == "*");
        let mark = |on: bool| if on { "✓" } else { " " };
        let mut items = vec![("done".to_owned(), "keep these".to_owned()), (format!("{} all of them", mark(all)), "and those it lists later".to_owned())];
        items.extend(tools.iter().map(|tool| {
            let on = all || allow.contains(tool);
            (format!("{} {tool}", mark(on)), if on { "runs without asking" } else { "asks first" }.to_owned())
        }));
        let menu = Menu { title: format!("{}: which tools run without asking", server.name), items, current: None, at };
        let Some(picked) = terminal.input.choose(menu).await else { return Ok(()) };
        match picked {
            0 => break,
            1 => allow = if all { Vec::new() } else { vec!["*".to_owned()] },
            n => {
                let Some(tool) = tools.get(n - 2) else { break };
                if all {
                    allow = tools.iter().filter(|other| *other != tool).cloned().collect();
                } else if let Some(i) = allow.iter().position(|allowed| allowed == tool) {
                    allow.remove(i);
                } else {
                    allow.push(tool.clone());
                }
            }
        }
        at = Some(picked);
    }
    if allow == server.allow {
        return Ok(());
    }
    set_allow(&server.name, &allow, mcp, agent, terminal)
}

/// What `/mcp` says: each server, whose it is, its walls, and how it's
/// doing, then those off, waiting for your yes, or not started, and why.
fn show(mcp: &Mcp, terminal: &Terminal) {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let rows = rows(mcp);
    if rows.is_empty() {
        say!("{pad}No MCP servers yet. /mcp add <name> looks one up in the MCP Registry, and /mcp import brings yours from other apps.");
        say!("{pad}{}", s.dim("Like: /mcp add playwright. Or as you'd start it: /mcp add context7 npx -y @upstash/context7-mcp"));
        return;
    }
    let shown: Vec<(&Row, String, String)> = rows
        .iter()
        .map(|row| {
            let (whose, how) = match row {
                Row::Running(server, status, also) => {
                    let walls = match &server.reach {
                        Reach::Local(local) if local.access.walls().is_some() => format!("in the sandbox ({})", local.access),
                        Reach::Local(_) => "outside the sandbox".to_owned(),
                        Reach::Remote(remote) => format!("at {}", host(&remote.url)),
                    };
                    let more = match also {
                        Also::FailedInWalls => s.yellow(" · a call failed in the sandbox, which may be why"),
                        Also::SignedIn => s.dim(" · signed in"),
                        Also::Nothing => String::new(),
                    };
                    (format!("{} · {walls}", server.origin.describe()), format!("{}{more}", how(status, s)))
                }
                Row::Off(_) => ("yours".to_owned(), s.dim("off: pick it to turn it on")),
                Row::Waiting(server) => (server.origin.describe().to_owned(), s.yellow("waiting for your yes: pick it to start it")),
                Row::Skipped(skipped) => (skipped.origin.describe().to_owned(), s.dim(&format!("not started: {}", skipped.why))),
            };
            (row, whose, how)
        })
        .collect();
    let names = rows.iter().map(|row| row.name().chars().count()).max().unwrap_or(0);
    let whose = shown.iter().map(|(_, whose, _)| whose.chars().count()).max().unwrap_or(0);
    for (row, said, how) in shown {
        let line = format!("{pad}{:<names$}  {}  {how}", row.name(), s.dim(&format!("{said:<whose$}")));
        // A long list of tools is cut to the screen; why one failed isn't.
        match row {
            Row::Running(_, Status::Ready(_), Also::Nothing | Also::SignedIn) => say!("{}", crate::ui::style::fit(&line, terminal.wrap_width().unwrap_or(100))),
            _ => say!("{line}"),
        }
    }
    say!("{pad}{}", s.dim("The model sees a server's tools as mcp__<server>__<tool>. /mcp add <name> <command> [args …] adds one."));
}

/// The host of `url`, as it's written.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.rsplit_once('@').map_or(authority, |(_, host)| host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn server(origin: Origin, access: Access) -> Server {
        let local = rein_mcp::Local { command: "sh".into(), args: Vec::new(), env: BTreeMap::new(), access };
        Server { name: "fake".into(), origin, reach: Reach::Local(local), allow: Vec::new() }
    }

    fn remote(origin: Origin) -> Server {
        let remote = rein_mcp::Remote { url: "https://mcp.example.com/mcp".into(), headers: BTreeMap::new() };
        Server { name: "far".into(), origin, reach: Reach::Remote(remote), allow: Vec::new() }
    }

    /// What `/mcp` offers for `row`, by name.
    fn offered(row: Row) -> Vec<String> {
        actions(row).iter().map(|action| action.shown().0).collect()
    }

    #[test]
    fn what_would_fix_a_server_is_offered_first_and_a_projects_isnt_let_out() {
        let ready = || Status::Ready(vec!["echo".into()]);
        let yours = server(Origin::Yours, Access::Network);
        assert_eq!(
            offered(Row::Running(yours.clone(), ready(), Also::Nothing)),
            ["choose the tools that run without asking", "change its walls", "restart it", "turn it off", "remove it"]
        );
        let failed = offered(Row::Running(yours.clone(), Status::Failed(Failure::InWalls("it stopped".into())), Also::Nothing));
        assert_eq!(failed[0], "run it outside the sandbox", "{failed:?}");
        assert_eq!(offered(Row::Running(yours.clone(), ready(), Also::FailedInWalls))[0], "run it outside the sandbox", "a call that failed in the sandbox counts too");
        assert_eq!(offered(Row::Running(yours.clone(), Status::Failed(Failure::Needs("TOKEN".into())), Also::Nothing))[0], "give it TOKEN");
        let outside = server(Origin::Yours, Access::Outside);
        assert!(
            !offered(Row::Running(outside, Status::Failed(Failure::InWalls("x".into())), Also::FailedInWalls)).contains(&"run it outside the sandbox".to_owned()),
            "it's out already"
        );
        let theirs = server(Origin::Project, Access::Network);
        assert_eq!(
            offered(Row::Running(theirs.clone(), Status::Failed(Failure::InWalls("x".into())), Also::FailedInWalls)),
            ["restart it", "take back your yes"],
            "the project doesn't decide its walls, nor what runs without asking"
        );
        assert_eq!(
            offered(Row::Running(theirs.clone(), Status::Failed(Failure::Needs("TOKEN".into())), Also::Nothing))[0],
            "give it TOKEN",
            "a key of yours it gets, as you said yes to"
        );
        assert_eq!(offered(Row::Off(yours)), ["turn it on", "remove it"]);
        assert_eq!(offered(Row::Waiting(theirs)), ["start it"]);
        assert!(offered(Row::Skipped(Skipped { name: "x".into(), origin: Origin::Project, why: String::new() })).is_empty());
        let far = remote(Origin::Yours);
        let signing = offered(Row::Running(far.clone(), Status::Failed(Failure::SignIn("Bearer".into())), Also::Nothing));
        assert_eq!(signing, ["sign in", "restart it", "turn it off", "remove it"], "no walls for one whose code doesn't run here");
        assert_eq!(offered(Row::Running(far, ready(), Also::SignedIn)), ["choose the tools that run without asking", "sign out", "restart it", "turn it off", "remove it"]);
        assert_eq!(offered(Row::Running(remote(Origin::Project), Status::Failed(Failure::SignIn("Bearer".into())), Also::Nothing))[0], "sign in", "you said yes to it");
        assert_eq!(host("https://me:pw@mcp.example.com:8443/mcp?x=1"), "mcp.example.com:8443");
    }
}
