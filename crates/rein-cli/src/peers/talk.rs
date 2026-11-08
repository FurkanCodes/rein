//! The conversation among the others: joining them, taking in what they
//! send, sending back how a task went, and the commands for all of it.

use std::path::Path;

use rein_agent::agent::{Agent, Heard, Llm, Outcome};
use rein_agent::permissions::{Mode, Sandboxing};

use super::node::{Incoming, Joining, Policy, Show, preview};
use super::{Body, Ended, Node, Reply, Report, State};
use crate::CliResult;
use crate::commands::Slash;
use crate::conversation::{Start, turn};
use crate::input::Menu;
use crate::terminal::{Terminal, peer_said};
use crate::ui::markdown::Markdown;
use crate::ui::style::{GUTTER, Style};
use crate::ui::wrap::Wrap;

/// How many turns in a row other reins may start before you say anything:
/// past that, what they send waits in `/inbox`.
pub(crate) const TURNS_IN_A_ROW: usize = 12;

/// The most of a turn's last answer that goes back as a task's result,
/// about 4,000 tokens: Claude Code's teammates send 4,000 characters, and
/// asking for the rest runs the teammate again; oh-my-pi sends 11,500.
/// Past this, its start, and how much more there was.
const RESULT_CHARS: usize = 16_000;

/// `text` as a result carries it: whole up to [`RESULT_CHARS`], or its
/// start and how much more there was, which `ask`, still there, has.
pub(crate) fn capped(text: &str, ask: Option<&str>) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= RESULT_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(RESULT_CHARS).collect();
    let more = count - RESULT_CHARS;
    match ask {
        Some(name) => format!("{head}\n\n[…{more} more characters: ask {name} for the rest]"),
        None => format!("{head}\n\n[…{more} more characters]"),
    }
}

/// Joins the other reins, as the settings say, and says who this one is.
/// `child` is the rein that started this one and the task it's for, for a
/// sub-agent: it claims the task as soon as it's in. Why it couldn't do
/// what it set out to, which has been said too: `terminal.peers` says
/// whether it's among the others.
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
) -> Result<(), String> {
    let Joining::On(policy) = joining else { return Ok(()) };
    let s = terminal.err;
    let show = Show { title: terminal.fancy, strip: strip && terminal.input.pinned() };
    match Node::start(&super::folder(rein), name, workspace, root, &terminal.model, policy, show).await {
        Ok(node) => {
            node.follow_mode(mode);
            super::farewell::joined(super::folder(rein), &node.info().workspace);
            let others = node.peers().await.len();
            let about = match others {
                0 => "no other rein is running yet: start one in another tab".to_owned(),
                1 => "1 other rein is running · /peers".to_owned(),
                n => format!("{n} other reins are running · /peers"),
            };
            // Under the logo, the tab's title and the strip say this.
            if !crate::ui::logo::is_shown() {
                say!("{}", s.dim(&format!("You're {} in {} · {about}", node.info().name, node.info().workspace)));
            }
            let mut claimed = Ok(());
            if let Some((parent, id)) = child {
                match node.claim(parent, id).await {
                    Ok(from) => say!("{}", s.dim(&format!("A sub-agent of {from}: its task follows, and the result goes back to it"))),
                    Err(e) => {
                        say!("{}", s.red(&format!("✗ no task: {e}")));
                        claimed = Err(format!("no task: {e}"));
                    }
                }
            }
            terminal.peers = Some(node);
            claimed
        }
        Err(e) => {
            say!("{}", s.dim(&format!("Peers: off ({e})")));
            Err(e)
        }
    }
}

/// What to do with what came in while nothing ran. A change waits for
/// the next turn; a message starts one, unless other reins have started
/// too many in a row, when it waits in `/inbox`.
pub(crate) async fn take_in(incoming: Incoming, in_a_row: &mut usize, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    match starts_a_turn(incoming, in_a_row, terminal) {
        Taken::Turn(incoming) => work_on(incoming, agent, terminal).await,
        Taken::Noted => Ok(()),
        Taken::TooMany(incoming) => {
            if let Some(peers) = &terminal.peers {
                peers.hold(incoming);
            }
            terminal.input.notice(&format!("other reins have started {TURNS_IN_A_ROW} turns in a row: what they send now waits in /inbox"));
            Ok(())
        }
    }
}

/// What's done with what came in while nothing ran.
pub(crate) enum Taken {
    /// It starts a turn.
    Turn(Incoming),
    /// It's noted for the next turn.
    Noted,
    /// It would start a turn, but other reins have started too many in a
    /// row. Where it waits depends on what's there to let it in.
    TooMany(Incoming),
}

/// What's done with what came in while nothing ran.
pub(crate) fn starts_a_turn(incoming: Incoming, in_a_row: &mut usize, terminal: &mut Terminal) -> Taken {
    if matches!(incoming, Incoming::Changed { .. }) {
        let note = terminal.heard_of(incoming);
        terminal.notes.extend(note);
        return Taken::Noted;
    }
    // You went on in a sub-agent's tab: you see it here now, and this
    // conversation hears of it with its next turn. It starts none: you're
    // at work in the other tab.
    if matches!(incoming, Incoming::Followed { .. }) {
        if terminal.fancy
            && let Some(shown) = reported(&incoming, terminal.err, terminal.wrap_width().unwrap_or(80))
        {
            say!("\n{shown}");
        }
        let note = terminal.heard_of(incoming);
        terminal.notes.extend(note);
        return Taken::Noted;
    }
    if *in_a_row >= TURNS_IN_A_ROW {
        return Taken::TooMany(incoming);
    }
    *in_a_row += 1;
    Taken::Turn(incoming)
}

/// A turn on what another rein sent, then its result back, if it was a task.
async fn work_on(incoming: Incoming, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let columns = terminal.wrap_width().unwrap_or(80);
    let shown = reported(&incoming, terminal.err, columns);
    let (Some(Heard::Peer { from, text }) | Some(Heard::Reported { from, text })) = terminal.heard_of(incoming) else { return Ok(()) };
    terminal.start_turn(None);
    if terminal.fancy {
        say!("\n{}", shown.unwrap_or_else(|| peer_said(&from, &text, terminal.err, columns)));
    }
    let outcome = turn(agent, Start::Peer { from, text }, terminal).await;
    report(terminal, outcome.as_ref().ok()).await;
    outcome.map(|_| ())
}

/// What the model asked for with `spawn`: you're asked, in a mode that
/// asks ([`Mode::asks_to_start_sub_agents`]), then a new tab opens with a
/// rein in it, named for its role, which claims the task.
/// What the model is told of how that went.
pub(crate) async fn spawn(call: &rein_llm::ToolCall, terminal: &mut Terminal) -> String {
    let error = rein_agent::tools::ERROR;
    match rein_llm::serde_json::from_str::<rein_agent::peers::SpawnArgs>(&call.arguments) {
        Ok(args) => start_child(args, true, terminal).await,
        Err(e) => format!("{error}bad arguments: {e}"),
    }
}

/// What the model is told of a sub-agent's result as it starts it: it
/// comes by itself. Told only that it comes back as a message, the model
/// messaged the sub-agent to ask for it, and the sub-agent answered with
/// a message too.
pub(crate) const AWAITED: &str =
    "Its result comes back to you by itself when it's done, and starts a turn: don't message it to ask, and end your turn if there's nothing else to do meanwhile.";

/// What a sub-agent's tab or pane is given of this rein's environment:
/// where rein keeps its files, and where programs and their settings are
/// found. A tab a terminal app opens has the app's environment, not your
/// shell's: on macOS its `PATH` is the system's alone, and a sub-agent
/// couldn't find `claude`, or `cargo` for a command. With another
/// `REIN_HOME` it couldn't find this rein, or its task. None is a key:
/// the script is a file, and keys never go in one.
const CARRIED: [&str; 4] = ["REIN_HOME", "PATH", "XDG_CONFIG_HOME", "CLAUDE_CONFIG_DIR"];

/// Each of [`CARRIED`] this rein has, and its value.
pub(crate) fn carried() -> Vec<(String, String)> {
    CARRIED.iter().filter_map(|key| std::env::var(key).ok().map(|value| ((*key).to_owned(), value))).collect()
}

/// Starts the sub-agent `args` asks for, asking you first if `ask` and the
/// mode asks: what the model is told of how it went.
async fn start_child(args: rein_agent::peers::SpawnArgs, ask: bool, terminal: &mut Terminal) -> String {
    let error = rein_agent::tools::ERROR;
    let Some(node) = &terminal.peers else { return format!("{error}this rein isn't among the others, so it can't start sub-agents") };
    let child = match Child::wanted(&args, &terminal.spec, node).await {
        Ok(child) => child,
        Err(e) => return format!("{error}{e}"),
    };
    let (name, model) = (child.name.clone(), child.model.clone());
    if ask && terminal.mode.asks_to_start_sub_agents() && !terminal.spawn_always {
        match ask_to_spawn(&name, &model, &args.task, args.edits, terminal).await {
            Some(true) => {}
            Some(false) => terminal.spawn_always = true,
            None => return "The user declined to start this sub-agent.".to_owned(),
        }
    }
    let Some(node) = &terminal.peers else { return format!("{error}this rein left the others") };
    let id = task_id();
    if let Err(e) = node.expect_child(&id, &name, &args.task) {
        return format!("{error}{e}");
    }
    let argv: Vec<String> = std::iter::once(rein_program()).chain(child.flags(&id, args.edits, terminal.mode, terminal.sandboxing)).collect();
    let env = carried();
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
            format!("Started sub-agent {name} ({model}) in {} (task {id}). {AWAITED}", opener.place())
        }
        Ok(super::spawn::Opened::Paste(command)) => {
            let copied = matches!(crate::input::copy(&command), crate::input::Copied::Clipboard);
            say!("{pad}{} open a new tab and paste{}:", s.yellow("◆"), if copied { " (it's copied)" } else { "" });
            say!("{pad}  {command}");
            format!("This terminal can't open tabs, so the user was given a command to start sub-agent {name} in a new tab (task {id}). {AWAITED}")
        }
        Err(e) => {
            if let Some(node) = &terminal.peers {
                node.forget_child(&id);
            }
            format!("{error}couldn't start the sub-agent: {e}")
        }
    }
}

/// A sub-agent about to start.
pub(crate) struct Child {
    pub(crate) name: String,
    pub(crate) model: String,
}

impl Child {
    /// The sub-agent `args` asks for of this rein, running `spec` itself,
    /// or why it can't have one.
    pub(crate) async fn wanted(args: &rein_agent::peers::SpawnArgs, spec: &str, node: &Node) -> Result<Child, String> {
        if node.parent().is_some() {
            return Err("a sub-agent can't start sub-agents of its own".to_owned());
        }
        let role = super::clean_name(&args.role)?;
        // Two asked for at once: the first hasn't joined when the second's named.
        let mut taken: Vec<String> = node.peers().await.into_iter().map(|peer| peer.name).collect();
        taken.extend(node.names_kept());
        let name = super::free_name(&role, &taken);
        let model = args.model.clone().unwrap_or_else(|| spec.to_owned());
        Ok(Child { name, model })
    }

    /// What it's started with for task `id`, in a tab or a client's pane:
    /// the task isn't on its command line, only where to fetch it.
    pub(crate) fn flags(&self, id: &str, edits: bool, mode: Mode, sandboxing: Sandboxing) -> Vec<String> {
        let name = &self.name;
        let sandbox = if sandboxing == Sandboxing::Off { "off" } else { "on" };
        let mut flags = vec!["--name".to_owned(), name.clone(), "--child".to_owned(), format!("{}:{id}", std::process::id()), "--mode".to_owned(), mode.as_str().to_owned()];
        flags.extend(["--sandbox".to_owned(), sandbox.to_owned()]);
        if !self.model.is_empty() {
            flags.extend(["-m".to_owned(), self.model.clone()]);
        }
        if edits {
            flags.extend(["--workspace".to_owned(), format!("sub/{name}"), "--worktree".to_owned()]);
        }
        flags
    }
}

/// A task's id, which the rein doing it claims it by.
pub(crate) fn task_id() -> String {
    rein_llm::new_session_id().unwrap_or_default().replace('-', "").chars().take(10).collect()
}

/// This program, to start another of.
pub(crate) fn rein_program() -> String {
    std::env::current_exe().map_or_else(|_| "rein".to_owned(), |path| path.to_string_lossy().into_owned())
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

/// Sends each task this turn took on its result: how it ended, its last
/// answer, the steps it ran, and the files it changed. A sub-agent stays
/// after, with what it knows: you can go on with it in its tab, and its
/// parent can hand it more.
pub(crate) async fn report(terminal: &mut Terminal, outcome: Option<&Outcome>) {
    if terminal.tasks.is_empty() {
        return;
    }
    let Some(peers) = &terminal.peers else { return };
    let ended = if outcome == Some(&Outcome::Done) { Ended::Done } else { Ended::Stopped };
    let report = super::farewell::finished(ended, &peers.info().name);
    super::farewell::settled();
    let tasks = std::mem::take(&mut terminal.tasks);
    let sub_agent = peers.parent().is_some();
    let s = terminal.err;
    let mut followed = None;
    for (reply_to, id) in tasks {
        let body = Body::Result { id: id.clone(), report: report.clone() };
        let said = match peers.send(&reply_to, body).await {
            Ok((peer, Reply::Delivered | Reply::Held)) if sub_agent => {
                followed = Some((reply_to, id));
                s.dim(&format!("↳ the report went back to {} · type to go on, or Ctrl-D to close this tab", peer.name))
            }
            Ok((peer, Reply::Delivered | Reply::Held)) => s.dim(&format!("↳ the result went back to {}", peer.name)),
            Ok((peer, Reply::Refused { why })) => s.red(&format!("✗ {} turned the result away: {why}", peer.name)),
            Ok((peer, Reply::Status(_) | Reply::Task { .. })) => s.red(&format!("✗ {} answered with something else", peer.name)),
            Err(e) => s.red(&format!("✗ the result didn't go back: {e}")),
        };
        say!("{}{said}", " ".repeat(GUTTER));
    }
    peers.set_state(State::Done, None);
    if followed.is_some() {
        terminal.followed = followed;
    }
}

/// After a turn you typed into a sub-agent whose task went back: what it
/// did goes to the rein that started it too, which hears of it with its
/// next turn. Codex and OpenCode make a finished one read-only; Kilo and
/// Roo let you go on, and the parent never hears.
pub(crate) async fn follow_up(terminal: &mut Terminal) {
    let Some((to, id)) = terminal.followed.clone() else { return };
    // Its parent has gone: nobody to tell.
    let Some(peers) = terminal.peers.as_ref().filter(|peers| peers.parent().is_some()) else { return };
    let report = super::farewell::finished(Ended::Done, &peers.info().name);
    let s = terminal.err;
    let said = match peers.send(&to, Body::Followed { id, summary: report.summary, files: report.files }).await {
        Ok((peer, Reply::Delivered | Reply::Held)) => s.dim(&format!("↳ {} hears of this with its next turn", peer.name)),
        Ok((peer, Reply::Refused { why })) => s.red(&format!("✗ {} turned it away: {why}", peer.name)),
        Ok((peer, Reply::Status(_) | Reply::Task { .. })) => s.red(&format!("✗ {} answered with something else", peer.name)),
        Err(e) => s.red(&format!("✗ it didn't go to the rein that started this one: {e}")),
    };
    say!("{}{said}", " ".repeat(GUTTER));
}

/// A sub-agent's report as this rein shows it: a line saying how it went,
/// then all it said, drawn as answers are, and for one that stopped early,
/// the steps it ran. You see what the model reads: Claude Code shows
/// `Done (12 tool uses · 30k tokens)`, and the report only to the model.
/// `None` for what isn't a report.
pub(crate) fn reported(incoming: &Incoming, s: Style, columns: usize) -> Option<String> {
    let pad = " ".repeat(GUTTER);
    let (head, summary, steps) = match incoming {
        Incoming::Result { from, report, .. } => {
            let steps = if report.ended == Ended::Done { Vec::new() } else { step_lines(report) };
            (headline(from, report, s), &report.summary, steps)
        }
        Incoming::Followed { from, summary, files, .. } => {
            (format!("{} {}{}", s.yellow("✉"), s.bold(&format!("you went on with {from} in its tab")), s.dim(&format!(" · {}", changed_files(files.len())))), summary, Vec::new())
        }
        _ => return None,
    };
    let mut out = format!("{pad}{head}");
    let width = columns.saturating_sub(GUTTER + 2).max(20);
    for step in steps {
        out += &format!("\n{pad}  {}", s.dim(&crate::ui::style::fit(&step, width)));
    }
    if !summary.trim().is_empty() {
        let mut markdown = Markdown::new(s.on());
        let text = markdown.push(summary.trim()) + &markdown.finish();
        let mut wrap = Wrap::new(columns, GUTTER, GUTTER);
        let text = wrap.push(&text) + &wrap.finish();
        out += &format!("\n{pad}{}", text.trim_end());
    }
    Some(out)
}

/// `✓ tests is done · 14 steps · 2m · changed 3 files`.
fn headline(from: &str, report: &Report, s: Style) -> String {
    let steps = report.step_count();
    let mut about = Vec::new();
    if steps > 0 {
        about.push(format!("{steps} step{}", if steps == 1 { "" } else { "s" }));
    }
    if report.seconds > 0 {
        about.push(super::strip::short(report.seconds));
    }
    if report.ended != Ended::Gone {
        about.push(changed_files(report.files.len()));
    }
    let about = if about.is_empty() { String::new() } else { s.dim(&format!(" · {}", about.join(" · "))) };
    let (mark, what) = match report.ended {
        Ended::Done => (s.green("✓"), format!("{from} is done")),
        Ended::Stopped => (s.red("✗"), format!("{from} stopped before it was done")),
        Ended::Closed => (s.red("✗"), format!("{from} stopped before it was done: its tab was closed")),
        Ended::Killed => (s.red("✗"), format!("{from} stopped before it was done: it was ended")),
        Ended::Gone => (s.red("✗"), format!("{from} went without a word before it was done")),
    };
    format!("{mark} {}{about}", s.bold(&what))
}

fn changed_files(count: usize) -> String {
    match count {
        0 => "changed no files".to_owned(),
        1 => "changed 1 file".to_owned(),
        n => format!("changed {n} files"),
    }
}

/// The steps a report says it ran, a line each, after how many earlier,
/// then the one it was in the middle of.
fn step_lines(report: &Report) -> Vec<String> {
    let earlier = (report.earlier > 0).then(|| format!("…{} earlier", report.earlier));
    let on = report.on.as_ref().map(|on| format!("▸ {on} · cut off, may have partly run"));
    earlier.into_iter().chain(report.steps.iter().map(|step| format!("· {step}"))).chain(on).collect()
}

/// What `/spawn` asks for: what isn't given, its name and task, is asked
/// for. `None` if you leave it.
pub(crate) async fn spawn_args(spawn: crate::commands::Spawn, terminal: &mut Terminal) -> Option<rein_agent::peers::SpawnArgs> {
    let role = match spawn.role {
        Some(role) => role,
        None => match terminal.input.read("its name (tests, docs, review)  ", false).await {
            crate::input::Read::Line(line) if !line.trim().is_empty() => line.trim().to_owned(),
            _ => return None,
        },
    };
    let task = match spawn.task {
        Some(task) => task,
        None => match terminal.input.read("its task  ", false).await {
            crate::input::Read::Line(line) if !line.trim().is_empty() => line,
            _ => return None,
        },
    };
    Some(rein_agent::peers::SpawnArgs { role, task, model: spawn.model, edits: spawn.edits })
}

/// `/spawn`: a sub-agent you start yourself, so nothing asks.
async fn spawn_here(spawn: crate::commands::Spawn, terminal: &mut Terminal) -> CliResult {
    let Some(args) = spawn_args(spawn, terminal).await else { return Ok(()) };
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
    let Some(picked) = terminal.input.choose(Menu { title: "which rein".into(), items, current: None, at: None }).await else { return Ok(()) };
    let (info, status) = &reins[picked];
    let mut actions = vec![("send it a message".to_owned(), String::new()), ("hand it a task".to_owned(), "its result comes back here".to_owned())];
    let question = status.as_ref().and_then(|st| st.question.clone());
    if let Some(question) = &question {
        actions.push(("answer yes".to_owned(), preview(&question.text)));
        actions.push(("answer no".to_owned(), String::new()));
    }
    let Some(action) = terminal.input.choose(Menu { title: format!("{}: do what", info.name), items: actions, current: None, at: None }).await else { return Ok(()) };
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
            let Some(i) = terminal.input.choose(Menu { title: if task { "hand a task to".into() } else { "send to".into() }, items, current: None, at: None }).await else {
                return Ok(());
            };
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
    match pick_held(terminal).await? {
        Some(incoming) => work_on(incoming, agent, terminal).await,
        None => Ok(()),
    }
}

/// What you let in of what's held back, if there's anything to.
pub(crate) async fn pick_held(terminal: &mut Terminal) -> CliResult<Option<Incoming>> {
    let held = node(terminal)?.held();
    if held.is_empty() {
        say!("{}nothing is waiting", " ".repeat(GUTTER));
        return Ok(None);
    }
    let items = held.iter().map(|incoming| (incoming.from().to_owned(), preview(&incoming.text()))).collect();
    let Some(i) = terminal.input.choose(Menu { title: "let which in".into(), items, current: None, at: None }).await else { return Ok(None) };
    Ok(node(terminal)?.take_held(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_goes_whole_up_to_its_cap_then_says_how_much_more() {
        assert_eq!(capped("  All pass.\n", Some("tests")), "All pass.");
        let long = "é".repeat(RESULT_CHARS + 5);
        let sent = capped(&long, Some("tests"));
        assert!(sent.starts_with(&"é".repeat(RESULT_CHARS)) && sent.ends_with("[…5 more characters: ask tests for the rest]"), "{}", &sent[sent.len() - 60..]);
        assert!(capped(&long, None).ends_with("[…5 more characters]"), "one that's gone can't be asked");
    }

    #[test]
    fn a_report_shows_how_it_went_then_all_it_said() {
        let report = Report {
            ended: Ended::Closed,
            summary: "Half done.\n\n**Next**: the docs.".into(),
            steps: vec!["Run cargo test · exit 0".into()],
            earlier: 2,
            on: Some("Run cargo build".into()),
            seconds: 130,
            files: vec!["a.rs".into()],
        };
        let shown = reported(&Incoming::Result { from: "tests".into(), id: "t1".into(), report, folder: None }, Style::plain(), 80).unwrap();
        let lines: Vec<&str> = shown.lines().map(str::trim).collect();
        assert_eq!(
            lines,
            [
                "✗ tests stopped before it was done: its tab was closed · 3 steps · 2m · changed 1 file",
                "…2 earlier",
                "· Run cargo test · exit 0",
                "▸ Run cargo build · cut off, may have partly run",
                "Half done.",
                "",
                "Next: the docs."
            ],
            "{shown}"
        );
        assert_eq!(reported(&Incoming::Message { from: "api".into(), text: "hi".into() }, Style::plain(), 80), None, "a message isn't a report");
    }
}
