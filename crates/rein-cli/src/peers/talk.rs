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
/// `child` is the rein that started this one and the task it's for, for a
/// sub-agent: it claims the task as soon as it's in.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn join(
    joining: Joining,
    name: Option<&str>,
    workspace: Option<&str>,
    child: Option<(u32, &str)>,
    strip: bool,
    rein: &Path,
    root: &Path,
    mode: Mode,
    terminal: &mut Terminal,
) -> bool {
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
            if let Some((parent, id)) = child {
                match node.claim(parent, id).await {
                    Ok(from) => say!("{}", s.dim(&format!("A sub-agent of {from}: its task follows, and the result goes back to it"))),
                    Err(e) => say!("{}", s.red(&format!("✗ no task: {e}"))),
                }
            }
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
        terminal.notes.extend(note);
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
    let Some(Heard::Peer { from, text }) = terminal.heard_of(incoming) else { return Ok(()) };
    terminal.start_turn(None);
    if terminal.fancy {
        say!("\n{}", peer_said(&from, &text, terminal.err, terminal.wrap_width().unwrap_or(80)));
    }
    let outcome = turn(agent, Start::Peer { from, text }, terminal).await;
    report(agent, terminal, outcome.as_ref().ok()).await;
    close_finished(terminal).await;
    outcome.map(|_| ())
}

/// What the model asked for with `spawn`: you're asked, then a new tab
/// opens with a rein in it, named for its role, which claims the task.
/// What the model is told of how that went.
pub(crate) async fn spawn(call: &rein_llm::ToolCall, terminal: &mut Terminal) -> String {
    let error = rein_agent::tools::ERROR;
    match rein_llm::serde_json::from_str::<rein_agent::peers::SpawnArgs>(&call.arguments) {
        Ok(args) => start_child(args, true, terminal).await,
        Err(e) => format!("{error}bad arguments: {e}"),
    }
}

/// Starts the sub-agent `args` asks for, asking you first if `ask`: what
/// the model is told of how it went.
async fn start_child(args: rein_agent::peers::SpawnArgs, ask: bool, terminal: &mut Terminal) -> String {
    let error = rein_agent::tools::ERROR;
    let Some(node) = &terminal.peers else { return format!("{error}this rein isn't among the others, so it can't start sub-agents") };
    if node.parent().is_some() {
        return format!("{error}a sub-agent can't start sub-agents of its own");
    }
    let role = match super::clean_name(&args.role) {
        Ok(role) => role,
        Err(e) => return format!("{error}{e}"),
    };
    let name = super::free_name(&role, &node.peers().await);
    let model = args.model.clone().unwrap_or_else(|| terminal.spec.clone());
    if ask && !terminal.spawn_always {
        match ask_to_spawn(&name, &model, &args.task, args.edits, terminal).await {
            Some(true) => {}
            Some(false) => terminal.spawn_always = true,
            None => return "The user declined to start this sub-agent.".to_owned(),
        }
    }
    let Some(node) = &terminal.peers else { return format!("{error}this rein left the others") };
    let id: String = rein_llm::new_session_id().unwrap_or_default().replace('-', "").chars().take(10).collect();
    if let Err(e) = node.expect_child(&id, &name, &args.task) {
        return format!("{error}{e}");
    }
    // Its own command line: the task isn't on it, only where to fetch it.
    let exe = std::env::current_exe().map_or_else(|_| "rein".to_owned(), |path| path.to_string_lossy().into_owned());
    let mut argv =
        vec![exe, "--name".to_owned(), name.clone(), "--child".to_owned(), format!("{}:{id}", std::process::id()), "--mode".to_owned(), terminal.mode.as_str().to_owned()];
    if !model.is_empty() {
        argv.extend(["-m".to_owned(), model.clone()]);
    }
    if args.edits {
        argv.extend(["--workspace".to_owned(), format!("sub/{name}"), "--worktree".to_owned()]);
    }
    let env: Vec<(String, String)> = ["REIN_HOME"].iter().filter_map(|key| std::env::var(key).ok().map(|value| ((*key).to_owned(), value))).collect();
    let folder = super::folder(&crate::config_dir().unwrap_or_default());
    let opener = super::spawn::Opener::detect(&|key| std::env::var(key).ok(), terminal.tab_command.as_deref());
    let script = super::spawn::script(&opener, &node.info().root, &env, &argv);
    let s = terminal.err;
    let pad = " ".repeat(GUTTER);
    let opened = match super::spawn::open(&opener, &folder, &id, &name, &script).await {
        Ok(opened) => Ok(opened),
        // A terminal that said no: you can still open it yourself.
        Err(why) => {
            say!("{pad}{}", s.dim(&format!("couldn't open a tab here ({why}); start it yourself instead")));
            super::spawn::open(&super::spawn::Opener::Paste, &folder, &id, &name, &script).await
        }
    };
    match opened {
        Ok(super::spawn::Opened::Tab) => {
            say!("{pad}{} sub-agent {name} started in {}", s.green("✓"), opener.place());
            // Terminal can only be asked for a window; macOS can make it a tab.
            if opener == super::spawn::Opener::Terminal && !std::mem::replace(&mut terminal.said_tabs, true) && !super::spawn::windows_open_as_tabs().await {
                say!("{pad}{}", s.dim("Terminal opens a window for it. For a tab: System Settings → Desktop & Dock → Prefer tabs when opening documents → Always"));
            }
            format!("Started sub-agent {name} ({model}) in {} (task {id}). Its result comes back to you as a message when it's done.", opener.place())
        }
        Ok(super::spawn::Opened::Paste(command)) => {
            let copied = matches!(crate::input::copy(&command), crate::input::Copied::Clipboard);
            say!("{pad}{} open a new tab and paste{}:", s.yellow("◆"), if copied { " (it's copied)" } else { "" });
            say!("{pad}  {command}");
            format!(
                "This terminal can't open tabs, so the user was given a command to start sub-agent {name} in a new tab (task {id}). Its result comes back to you as a message when it's done."
            )
        }
        Err(e) => {
            if let Some(node) = &terminal.peers {
                node.forget_child(&id);
            }
            format!("{error}couldn't start the sub-agent: {e}")
        }
    }
}

/// Asks whether to open a tab for sub-agent `name`: `Some(true)` for yes,
/// `Some(false)` for yes and don't ask again, `None` for no.
async fn ask_to_spawn(name: &str, model: &str, task: &str, edits: bool, terminal: &mut Terminal) -> Option<bool> {
    let place = if edits { ", in a worktree of its own" } else { "" };
    let detail = format!("{name} · {model}{place} · {}", preview(task));
    if !terminal.input.pinned() {
        let prompt = format!("{}Start sub-agent {detail} in a new tab?  [y]es  [n]o  [a]lways › ", " ".repeat(GUTTER));
        return match terminal.input.read(&prompt, false).await {
            crate::input::Read::Line(answer) => match crate::terminal::approval(&answer) {
                rein_agent::agent::Approval::Yes => Some(true),
                rein_agent::agent::Approval::Always => Some(false),
                _ => None,
            },
            _ => None,
        };
    }
    crate::ui::screen::progress(crate::ui::screen::Progress::Waiting);
    let question = crate::input::Question {
        title: "Start a sub-agent in a new tab?".to_owned(),
        detail: Some(detail),
        command: None,
        answers: vec![("y", "Yes".to_owned()), ("a", "Yes, and don't ask again in this conversation".to_owned()), ("n", "No".to_owned())],
    };
    let answer = terminal.input.ask(question).await;
    crate::ui::screen::progress(crate::ui::screen::Progress::Working);
    match answer {
        crate::input::Read::Line(key) if key == "y" => Some(true),
        crate::input::Read::Line(key) if key == "a" => Some(false),
        _ => None,
    }
}

/// Tells each sub-agent whose result this rein has now read that it's
/// done with it: its tab goes, unless you've taken it over.
pub(crate) async fn close_finished(terminal: &Terminal) {
    let Some(node) = &terminal.peers else { return };
    for child in node.finished_children() {
        if let Some(address) = &child.address {
            let _ = node.send(address, Body::Close).await;
        }
    }
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
            Ok((peer, Reply::Status(_) | Reply::Task { .. })) => s.red(&format!("✗ {} answered with something else", peer.name)),
            Err(e) => s.red(&format!("✗ the result didn't go back: {e}")),
        };
        say!("{}{said}", " ".repeat(GUTTER));
    }
    peers.set_state(State::Done, None);
}

/// `/spawn`: a sub-agent you start yourself, so nothing asks. What isn't
/// given, its name and task, is asked for.
async fn spawn_here(spawn: crate::commands::Spawn, terminal: &mut Terminal) -> CliResult {
    let role = match spawn.role {
        Some(role) => role,
        None => match terminal.input.read("its name (tests, docs, review)  ", false).await {
            crate::input::Read::Line(line) if !line.trim().is_empty() => line.trim().to_owned(),
            _ => return Ok(()),
        },
    };
    let task = match spawn.task {
        Some(task) => task,
        None => match terminal.input.read("its task  ", false).await {
            crate::input::Read::Line(line) if !line.trim().is_empty() => line,
            _ => return Ok(()),
        },
    };
    let args = rein_agent::peers::SpawnArgs { role, task, model: spawn.model, edits: spawn.edits };
    let said = start_child(args, false, terminal).await;
    match said.strip_prefix(rein_agent::tools::ERROR) {
        Some(why) => Err(why.to_owned()),
        None => Ok(()),
    }
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
            let node = node(terminal)?;
            let name = node.rename(&name).await?;
            // A tab rein opened in tmux or screen has the name it was given.
            if node.parent().is_some() {
                super::spawn::rename_tab(&|key| std::env::var(key).ok(), &name).await;
            }
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
        Slash::Spawn(spawn) => spawn_here(spawn, terminal).await?,
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
        Ok(Reply::Status(status)) => Some(*status),
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
        (peer, Reply::Status(_) | Reply::Task { .. }) => return Err(format!("{} answered with something else", peer.name)),
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
