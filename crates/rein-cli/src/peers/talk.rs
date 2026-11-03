//! The conversation among the others: joining them, taking in what they
//! send, sending back how a task went, and the commands for all of it.

use std::path::Path;

use rein_agent::agent::{Agent, Heard, Llm, Outcome};
use rein_agent::permissions::Mode;
use rein_llm::Item;

use super::node::{Incoming, Joining, Policy, Show, preview};
use super::{Body, Node, Reply, State};
use crate::CliResult;
use crate::commands::Slash;
use crate::conversation::{Start, turn};
use crate::input::Menu;
use crate::terminal::{Terminal, peer_said};
use crate::ui::style::GUTTER;

/// How many turns in a row other reins may start before you say anything:
/// past that, what they send waits in `/inbox`.
const TURNS_IN_A_ROW: usize = 12;

/// The most of a turn's last answer that goes back as a task's result.
const RESULT_CHARS: usize = 4000;

/// Joins the other reins, as the settings say, and says who this one is.
pub(crate) async fn join(joining: Joining, name: Option<&str>, workspace: Option<&str>, strip: bool, rein: &Path, root: &Path, mode: Mode, terminal: &mut Terminal) -> bool {
    let Joining::On(policy) = joining else { return false };
    let s = terminal.err;
    let show = Show { title: terminal.fancy, strip: strip && terminal.input.pinned() };
    match Node::start(&super::folder(rein), name, workspace, root, &terminal.model, policy, show).await {
        Ok(node) => {
            node.follow_mode(mode);
            let others = node.peers().await.len();
            let about = match others {
                0 => "no other rein is running yet: start one in another tab".to_owned(),
                1 => "1 other rein is running · /peers".to_owned(),
                n => format!("{n} other reins are running · /peers"),
            };
            say!("{}", s.dim(&format!("You're {} in {} · {about}", node.info().name, node.info().workspace)));
            terminal.peers = Some(node);
            true
        }
        Err(e) => {
            say!("{}", s.dim(&format!("Peers: off ({e})")));
            false
        }
    }
}

/// What to do with what came in while nothing ran. A change waits for
/// the next turn; a message starts one, unless other reins have started
/// too many in a row, when it waits in `/inbox`.
pub(crate) async fn take_in(incoming: Incoming, in_a_row: &mut usize, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    if matches!(incoming, Incoming::Changed { .. }) {
        let note = terminal.heard_of(incoming);
        terminal.notes.push(note);
        return Ok(());
    }
    if *in_a_row >= TURNS_IN_A_ROW {
        if let Some(peers) = &terminal.peers {
            peers.hold(incoming);
        }
        terminal.input.notice(&format!("other reins have started {TURNS_IN_A_ROW} turns in a row: what they send now waits in /inbox"));
        return Ok(());
    }
    *in_a_row += 1;
    work_on(incoming, agent, terminal).await
}

/// A turn on what another rein sent, then its result back, if it was a task.
async fn work_on(incoming: Incoming, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let Heard::Peer { from, text } = terminal.heard_of(incoming) else { return Ok(()) };
    terminal.start_turn(None);
    if terminal.fancy {
        say!("\n{}", peer_said(&from, &text, terminal.err, terminal.wrap_width().unwrap_or(80)));
    }
    let outcome = turn(agent, Start::Peer { from, text }, terminal).await;
    report(agent, terminal, outcome.as_ref().ok()).await;
    outcome.map(|_| ())
}

/// Sends each task this turn took on its result: done or not, the turn's
/// last answer, and the files it changed.
pub(crate) async fn report(agent: &Agent<Llm>, terminal: &mut Terminal, outcome: Option<&Outcome>) {
    if terminal.tasks.is_empty() {
        return;
    }
    let summary: String = agent
        .full()
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::Assistant(text) if !text.trim().is_empty() => Some(text.trim().chars().take(RESULT_CHARS).collect()),
            _ => None,
        })
        .unwrap_or_default();
    let done = outcome == Some(&Outcome::Done);
    let files = terminal.changed.clone();
    let tasks = std::mem::take(&mut terminal.tasks);
    let Some(peers) = &terminal.peers else { return };
    let s = terminal.err;
    for (reply_to, id) in tasks {
        let body = Body::Result { id, done, summary: summary.clone(), files: files.clone() };
        let said = match peers.send(&reply_to, body).await {
            Ok((peer, Reply::Delivered | Reply::Held)) => s.dim(&format!("↳ the result went back to {}", peer.name)),
            Ok((peer, Reply::Refused { why })) => s.red(&format!("✗ {} turned the result away: {why}", peer.name)),
            Ok((peer, Reply::Status(_))) => s.red(&format!("✗ {} answered with something else", peer.name)),
            Err(e) => s.red(&format!("✗ the result didn't go back: {e}")),
        };
        say!("{}{said}", " ".repeat(GUTTER));
    }
    peers.set_state(State::Done, None);
}

/// The commands about other reins.
pub(crate) async fn command(command: Slash, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let pad = " ".repeat(GUTTER);
    let s = terminal.err;
    if terminal.peers.is_none() {
        return Err("this rein isn't among the others: peers are off in the settings, or couldn't start".into());
    }
    match command {
        Slash::Name(None) => say!("{pad}{}", info_line(terminal)),
        Slash::Name(Some(name)) => {
            let name = node(terminal)?.rename(&name).await?;
            say!("{pad}{} other reins call this one {name} now", s.green("✓"));
        }
        Slash::Workspace(None) => say!("{pad}{}", info_line(terminal)),
        Slash::Workspace(Some(workspace)) => {
            node(terminal)?.set_workspace(&workspace)?;
            say!("{pad}{} in workspace {workspace} now", s.green("✓"));
        }
        Slash::Peers => peers_view(terminal).await?,
        Slash::Send(to, text) => write_to(to, text, false, terminal).await?,
        Slash::Delegate(to, task) => write_to(to, task, true, terminal).await?,
        Slash::Inbox => inbox(agent, terminal).await?,
        _ => {}
    }
    Ok(())
}

fn node(terminal: &mut Terminal) -> CliResult<&mut Node> {
    terminal.peers.as_mut().ok_or_else(|| "this rein isn't among the others".to_owned())
}

/// `web in health, working in ~/projects/shop`.
fn info_line(terminal: &Terminal) -> String {
    terminal.peers.as_ref().map_or_else(String::new, |node| {
        let info = node.info();
        let held = match node.policy() {
            Policy::Accept => "messages go straight in",
            Policy::Hold => "messages wait in /inbox for you",
            Policy::Refuse => "messages are turned away",
        };
        format!("{} in workspace {}, in {} · {held}", info.name, info.workspace, info.root.display())
    })
}

/// `/peers`: the table, then what to do with the rein you pick.
async fn peers_view(terminal: &mut Terminal) -> CliResult {
    let node = node(terminal)?;
    let reins = super::dashboard::gather(node.peers().await).await;
    let me = match super::ask(node.info(), Body::Status).await {
        Ok(Reply::Status(status)) => Some(status),
        _ => None,
    };
    let s = terminal.err;
    let columns = crate::ui::screen::size().1.saturating_sub(GUTTER);
    for line in super::dashboard::lines(&reins, me.as_ref(), None, s, columns) {
        say!("{}{line}", " ".repeat(GUTTER - 1));
    }
    if reins.is_empty() {
        say!("{}{}", " ".repeat(GUTTER), s.dim("No other rein is running. Start one in another terminal tab, and it shows here."));
        return Ok(());
    }
    let items = reins
        .iter()
        .map(|(info, status)| {
            let about = status.as_ref().map_or("not answering".to_owned(), |st| format!("{} {} · {}", st.state.mark(), st.state.word(), info.workspace));
            (info.name.clone(), about)
        })
        .collect();
    let Some(picked) = terminal.input.choose(Menu { title: "which rein".into(), items, current: None }).await else { return Ok(()) };
    let (info, status) = &reins[picked];
    let mut actions = vec![("send it a message".to_owned(), String::new()), ("hand it a task".to_owned(), "its result comes back here".to_owned())];
    let question = status.as_ref().and_then(|st| st.question.clone());
    if let Some(question) = &question {
        actions.push(("answer yes".to_owned(), preview(&question.text)));
        actions.push(("answer no".to_owned(), String::new()));
    }
    let Some(action) = terminal.input.choose(Menu { title: format!("{}: do what", info.name), items: actions, current: None }).await else { return Ok(()) };
    match action {
        0 => write_to(Some(info.address()), None, false, terminal).await,
        1 => write_to(Some(info.address()), None, true, terminal).await,
        yes @ (2 | 3) => {
            let said = super::dashboard::answer(Some(&reins[picked]), yes == 2).await;
            say!("{}{said}", " ".repeat(GUTTER));
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `/send` and `/delegate`: who to, and what, asked for if not given.
async fn write_to(to: Option<String>, text: Option<String>, task: bool, terminal: &mut Terminal) -> CliResult {
    let to = match to {
        Some(to) => to,
        None => {
            let peers = node(terminal)?.peers().await;
            if peers.is_empty() {
                return Err("no other rein is running: start one in another terminal tab".into());
            }
            let items = peers.iter().map(|p| (p.name.clone(), p.workspace.clone())).collect();
            let Some(i) = terminal.input.choose(Menu { title: if task { "hand a task to".into() } else { "send to".into() }, items, current: None }).await else { return Ok(()) };
            peers[i].address()
        }
    };
    let text = match text {
        Some(text) => text,
        None => match terminal.input.read(if task { "the task  " } else { "message  " }, false).await {
            crate::input::Read::Line(line) if !line.trim().is_empty() => line,
            _ => return Ok(()),
        },
    };
    let body = if task {
        let id: String = rein_llm::new_session_id().map_err(|e| e.to_string())?.chars().take(8).collect();
        Body::Task { id, text }
    } else {
        Body::Message { text }
    };
    let s = terminal.err;
    let pad = " ".repeat(GUTTER);
    match node(terminal)?.send(&to, body).await? {
        (peer, Reply::Delivered) if task => say!("{pad}{} handed to {}; its result comes back here", s.green("✓"), peer.name),
        (peer, Reply::Delivered) => say!("{pad}{} sent to {}", s.green("✓"), peer.name),
        (peer, Reply::Held) => say!("{pad}{} {} keeps it for its user to let in", s.green("✓"), peer.name),
        (peer, Reply::Refused { why }) => return Err(format!("{} turned it away: {why}", peer.name)),
        (peer, Reply::Status(_)) => return Err(format!("{} answered with something else", peer.name)),
    }
    Ok(())
}

/// `/inbox`: what's waiting to be let in; the one picked goes in now.
async fn inbox(agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let held = node(terminal)?.held();
    if held.is_empty() {
        say!("{}nothing is waiting", " ".repeat(GUTTER));
        return Ok(());
    }
    let items = held.iter().map(|incoming| (incoming.from().to_owned(), preview(&incoming.text()))).collect();
    let Some(i) = terminal.input.choose(Menu { title: "let which in".into(), items, current: None }).await else { return Ok(()) };
    let Some(incoming) = node(terminal)?.take_held(i) else { return Ok(()) };
    work_on(incoming, agent, terminal).await
}
