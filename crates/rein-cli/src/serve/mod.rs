//! `rein serve`: rein with no screen, for another program to drive over its
//! standard input and output, as an agent of the Agent Client Protocol.
//!
//! A process holds one conversation, as a rein in a tab does; a client that
//! wants another starts another. What comes in while a turn runs is read
//! alongside it, so a client can answer a question, steer or stop it. Only
//! messages go to standard output: what rein says goes to standard error,
//! and nothing it runs gets its standard input.
//!
//! A client that says it can open panes joins this rein to the others, on
//! the terms a tab does. Each sub-agent is a `rein serve` of its own, which
//! the client starts when it's asked to, and drives like any other.

mod ui;
mod wire;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use rein_agent::agent::{Agent, Heard, Llm, Outcome, Ui};
use rein_agent::background::Jobs;
use rein_agent::permissions::{Mode, Sandboxing};
use rein_agent::session;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, Stdin};
use tokio::sync::{mpsc, oneshot};

use crate::args::{AskArgs, Resume};
use crate::conversation::{self, Opened, Start, step_limit};
use crate::format::{shorten, utc_stamp};
use crate::peers::node::Incoming as Sent;
use crate::peers::talk::Taken;
use crate::peers::{self, State, talk};
use crate::settings::{self, Settings};
use crate::{CliResult, config_dir, describe};
use ui::Served;
use wire::{Incoming, Refused, Request};

type Input = Lines<BufReader<Stdin>>;

/// How many saved conversations `session/list` gives at once.
const PAGE: usize = 100;
/// The longest a conversation's title is.
const TITLE: usize = 70;

/// What the parts of `serve` share: the way out, and what came in for a
/// turn that's running.
#[derive(Clone)]
struct Link {
    out: mpsc::UnboundedSender<String>,
    asked: Rc<RefCell<HashMap<u64, oneshot::Sender<Option<Value>>>>>,
    asks: Rc<Cell<u64>>,
    heard: Rc<RefCell<Vec<Heard>>>,
    /// The client can open a pane for a sub-agent.
    panes: Rc<Cell<bool>>,
}

impl Link {
    /// Writing waits for the client, and rein has one thread: a task of its
    /// own does that, in the order sent.
    fn send(&self, line: String) {
        // Gone only once the client has stopped reading.
        let _ = self.out.send(line);
    }

    /// A number to ask under, and what the answer comes through.
    fn ask(&self) -> (u64, oneshot::Receiver<Option<Value>>) {
        let id = self.asks.get();
        self.asks.set(id + 1);
        let (given, answer) = oneshot::channel();
        self.asked.borrow_mut().insert(id, given);
        (id, answer)
    }

    fn answered(&self, id: u64, result: Option<Value>) {
        if let Some(given) = self.asked.borrow_mut().remove(&id) {
            let _ = given.send(result);
        }
    }
}

/// The one conversation this process holds.
struct Session {
    id: String,
    agent: Agent<Llm>,
    served: Served,
    max_steps: Option<usize>,
    sandboxing: Sandboxing,
    /// Turns other reins started since the client last said something.
    in_a_row: usize,
    /// Kept for as long as the conversation: it holds its MCP servers.
    _setup: conversation::Setup,
}

/// Runs until the client closes standard input. `flags` are what a
/// conversation takes, for a client to start this one as it wants.
pub(crate) async fn run(http: &rein_llm::reqwest::Client, flags: Vec<String>) -> CliResult {
    // Now, not when a client asks for a conversation: nothing's listening
    // to a rein that was started wrong.
    let settings = Settings::load(&config_dir()?.join(settings::FILE))?;
    let args = AskArgs::parse(flags.clone(), &settings)?;
    if args.prompt.is_some() {
        return Err("rein serve takes no message: its client sends them".into());
    }
    if args.resume != Resume::New {
        return Err("rein serve starts a new conversation".into());
    }
    let (out, mut lines_out) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines_out.recv().await {
            let written = stdout.write_all(line.as_bytes()).await.is_ok() && stdout.write_all(b"\n").await.is_ok() && stdout.flush().await.is_ok();
            if !written {
                break;
            }
        }
    });
    let link = Link { out, asked: Rc::default(), asks: Rc::new(Cell::new(0)), heard: Rc::default(), panes: Rc::default() };
    let mut input: Input = BufReader::new(tokio::io::stdin()).lines();
    let mut session: Option<Session> = None;
    loop {
        // A client that takes no turn nobody prompted hears with its next.
        let jobs = session.as_ref().filter(|_| link.panes.get()).map(|session| session.agent.tools.jobs().clone());
        tokio::select! {
            line = input.next_line() => {
                let Ok(Some(line)) = line else { break };
                match Incoming::read(&line) {
                    Incoming::Request { id, request } => {
                        let Some(goes_on) = handle(request, http, &flags, &mut session, &link, &mut input).await else { break };
                        goes_on.send(&link, &id);
                    }
                    Incoming::Unreadable(refused) => link.send(wire::refuse(&Value::Null, &refused)),
                    // Nothing's running to stop, and nothing was asked.
                    Incoming::Cancel | Incoming::Reply { .. } | Incoming::Ignored => {}
                }
            }
            sent = from_the_others(&mut session) => {
                let Some(session) = &mut session else { continue };
                if !take_in(session, sent, &link, &mut input).await {
                    break;
                }
            }
            () = job_ended(jobs.as_deref()) => {
                let Some(session) = &mut session else { continue };
                if !unprompted(session, Start::Ended, &link, &mut input).await {
                    break;
                }
            }
        }
    }
    // Its pane closed before its task was done: while this rein is still
    // among the others, whoever's owed the result hears what it did.
    peers::farewell::say_how_it_ended(peers::Ended::Closed).await;
    if let Some(mut session) = session {
        session.agent.session_ended(rein_agent::hooks::Ended::Exit, &mut session.served).await;
    }
    drop(link);
    let _ = writer.await;
    Ok(())
}

/// What a request comes to.
enum Answer {
    Result(Value),
    Refused(Refused),
}

impl Answer {
    fn send(self, link: &Link, id: &Value) {
        link.send(match self {
            Answer::Result(result) => wire::answer(id, &result),
            Answer::Refused(refused) => wire::refuse(id, &refused),
        });
    }
}

/// Answers a request. `None`: the client went away during a turn.
async fn handle(
    request: Result<Request, Refused>,
    http: &rein_llm::reqwest::Client,
    flags: &[String],
    session: &mut Option<Session>,
    link: &Link,
    input: &mut Input,
) -> Option<Answer> {
    let request = match request {
        Ok(request) => request,
        Err(refused) => return Some(Answer::Refused(refused)),
    };
    Some(match request {
        Request::Initialize { panes } => {
            link.panes.set(panes);
            Answer::Result(initialized())
        }
        Request::NewSession { .. } | Request::LoadSession { .. } if session.is_some() => {
            Answer::Refused(Refused::new("rein serve holds one conversation: start another rein serve for another"))
        }
        Request::NewSession { cwd } => match open(&cwd, Resume::New, flags, http, link).await {
            Ok(opened) => {
                let result = json!({ "sessionId": opened.id, "modes": modes(opened.agent.permissions.mode, opened.sandboxing) });
                *session = Some(opened);
                Answer::Result(result)
            }
            Err(said) => Answer::Refused(Refused::new(said)),
        },
        Request::LoadSession { session: id, cwd } => match load(&id, &cwd, flags, http, link).await {
            Ok(opened) => {
                let result = json!({ "modes": modes(opened.agent.permissions.mode, opened.sandboxing) });
                *session = Some(opened);
                Answer::Result(result)
            }
            Err(said) => Answer::Refused(Refused::new(said)),
        },
        Request::ListSessions { cwd, skip } => match listed(cwd.as_deref(), skip) {
            Ok(result) => Answer::Result(result),
            Err(said) => Answer::Refused(Refused::new(said)),
        },
        Request::Prompt { session: id, text } => match mine(session, &id) {
            Ok(session) => return turn(session, text, link, input).await,
            Err(refused) => Answer::Refused(refused),
        },
        Request::SetMode { session: id, mode } => match mine(session, &id) {
            Ok(session) => {
                session.agent.permissions.mode = mode;
                session.agent.step_limit = step_limit(mode, false, session.max_steps);
                session.served.terminal.mode = mode;
                if let Some(peers) = &session.served.terminal.peers {
                    peers.follow_mode(mode);
                }
                Answer::Result(json!({}))
            }
            Err(refused) => Answer::Refused(refused),
        },
        // With no turn running, it goes in as the next one starts.
        Request::Steer { session: id, text } => match mine(session, &id) {
            Ok(_) => {
                link.heard.borrow_mut().push(Heard::You(text));
                Answer::Result(json!({}))
            }
            Err(refused) => Answer::Refused(refused),
        },
    })
}

fn mine<'a>(session: &'a mut Option<Session>, id: &str) -> Result<&'a mut Session, Refused> {
    session.as_mut().filter(|session| session.id == id).ok_or_else(|| Refused::new(format!("there's no conversation {id} here")))
}

fn initialized() -> Value {
    json!({
        "protocolVersion": wire::VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "sessionCapabilities": { "list": {} },
            "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false },
            "mcpCapabilities": { "http": false, "sse": false },
            "_meta": { (wire::STEERS): true, (wire::PANES): true },
        },
        "agentInfo": { "name": "rein", "title": "rein", "version": env!("CARGO_PKG_VERSION") },
        "authMethods": [],
    })
}

/// The modes, as `/mode` lists them, marks and all.
fn modes(mode: Mode, sandboxing: Sandboxing) -> Value {
    let available: Vec<Value> = Mode::ALL
        .iter()
        .map(|&mode| {
            let description = crate::modes::description(mode, sandboxing);
            let about = description.split_once(": ").map_or(description, |(_, about)| about);
            json!({ "id": mode.as_str(), "name": crate::modes::shown_name(mode, sandboxing), "description": about })
        })
        .collect();
    json!({ "currentModeId": mode.as_str(), "availableModes": available })
}

/// Opens the conversation in `cwd`, as a task run on its own: nobody's there
/// for what a project brings to wait on, so that stays off.
async fn open(cwd: &Path, resume: Resume, flags: &[String], http: &rein_llm::reqwest::Client, link: &Link) -> CliResult<Session> {
    std::env::set_current_dir(cwd).map_err(|e| format!("can't go into {}: {e}", cwd.display()))?;
    let dir = config_dir()?;
    let settings = Settings::load(&dir.join(settings::FILE))?;
    let compact_at = settings.get("compact_at", "REIN_COMPACT_AT");
    let mut args = AskArgs::parse(flags.to_vec(), &settings)?;
    args.resume = resume;
    let (mode, max_steps) = (args.mode, args.max_steps);
    let (joining, name, workspace, child) = (args.joining, args.name.clone(), args.workspace.clone(), args.child.clone());
    // Its task waits on its parent, and nothing here would ever fetch it.
    if child.is_some() && !link.panes.get() {
        return Err("a sub-agent needs a client that opens panes".into());
    }
    let opened = conversation::open(http, args, compact_at, true).await?.ok_or("the conversation didn't open")?;
    let Opened { mut agent, mut terminal, setup } = opened;
    if link.panes.get() {
        // A sub-agent's worktree, if it has one.
        let root = std::env::current_dir().map_err(|e| format!("can't find the current directory: {e}"))?;
        let claiming = child.as_ref().map(|(pid, id)| (*pid, id.as_str()));
        let joined = talk::join(joining, name.as_deref(), workspace.as_deref(), claiming, false, &dir, &root, mode, &mut terminal).await;
        // Without its task it would sit there with nothing to do, and the
        // rein that started it would wait: its client should hear.
        if child.is_some() {
            joined?;
            if terminal.peers.is_none() {
                return Err("a sub-agent needs to be among the other reins, and they're turned off here".into());
            }
        }
        if terminal.peers.is_some() {
            // A sub-agent doesn't start sub-agents: one level, no further.
            agent = if child.is_some() { agent.with_child_peer_tools() } else { agent.with_peer_tools() };
        }
    }
    // Someone's there, to press stop.
    agent.step_limit = step_limit(mode, false, max_steps);
    // The saved conversation's own, for `session/load` to find it by.
    let id = agent.session().map(|session| session.header().id.clone()).ok_or("the conversation has no file")?;
    let sandboxing = agent.permissions.sandboxing();
    let served = Served::new(id.clone(), link.clone(), terminal);
    Ok(Session { id, agent, served, max_steps, sandboxing, in_a_row: 0, _setup: setup })
}

/// Where the conversations held in `cwd` are saved.
fn saved_in(cwd: &Path) -> CliResult<std::path::PathBuf> {
    // As `open` will find it: on macOS /tmp is /private/tmp.
    let root = std::fs::canonicalize(cwd).map_err(|e| format!("can't find {}: {e}", cwd.display()))?;
    Ok(session::folder(&config_dir()?.join("sessions"), &root))
}

/// Opens the conversation `id`, saved in `cwd`, and says it so far.
async fn load(id: &str, cwd: &Path, flags: &[String], http: &rein_llm::reqwest::Client, link: &Link) -> CliResult<Session> {
    let path = session::find(&saved_in(cwd)?, id).ok_or_else(|| format!("there's no saved conversation {id} in {}", cwd.display()))?;
    let opened = open(cwd, Resume::Saved(path), flags, http, link).await?;
    opened.served.replay(opened.agent.full());
    Ok(opened)
}

/// A page of saved conversations: `cwd`'s, or every project's, most
/// recently used first, with a cursor to the next page if there's one.
fn listed(cwd: Option<&Path>, skip: usize) -> CliResult<Value> {
    let folders = match cwd {
        Some(cwd) => vec![saved_in(cwd)?],
        None => session::folders(&config_dir()?.join("sessions")),
    };
    let (saved, more) = session::page(&folders, skip, PAGE);
    let sessions: Vec<Value> = saved
        .iter()
        .map(|saved| {
            // A sub-agent's first message is its task, from another rein.
            let first = saved.first.as_deref().map(|first| rein_agent::peers::unheard(first).map_or(first, |(_, text)| crate::peers::node::task_in(text).unwrap_or(text)));
            json!({ "sessionId": saved.id, "cwd": saved.cwd, "title": first.map(|first| shorten(first, TITLE)), "updatedAt": utc_stamp(saved.modified) })
        })
        .collect();
    let mut listed = json!({ "sessions": sessions });
    if more {
        listed["nextCursor"] = json!(skip.saturating_add(PAGE).to_string());
    }
    Ok(listed)
}

/// The next thing another rein sends; never, before there's a conversation
/// or for one not among them.
async fn from_the_others(session: &mut Option<Session>) -> Sent {
    match session {
        Some(session) => conversation::from_peers(&mut session.served.terminal.peers).await,
        None => std::future::pending().await,
    }
}

/// The next background command to end; never, without `jobs` to watch.
async fn job_ended(jobs: Option<&Jobs>) {
    match jobs {
        Some(jobs) => jobs.ended().await,
        None => std::future::pending().await,
    }
}

/// What another rein sent while nothing ran, which may start a turn nobody
/// prompted. `false`: the client went away during it.
async fn take_in(session: &mut Session, sent: Sent, link: &Link, input: &mut Input) -> bool {
    let sent = match talk::starts_a_turn(sent, &mut session.in_a_row, &mut session.served.terminal) {
        Taken::Turn(sent) => sent,
        Taken::Noted => return true,
        // There's no `/inbox` here. Not what your rules hold back: that
        // stays there, and only a terminal can let it in.
        Taken::TooMany(sent) => {
            let terminal = &mut session.served.terminal;
            let note = terminal.heard_of(sent);
            terminal.notes.extend(note);
            session.served.note(&format!("other reins have started {} turns in a row: what they send now goes in with your next message", talk::TURNS_IN_A_ROW));
            return true;
        }
    };
    // A sub-agent whose parent has gone stays: its pane is the client's to close.
    let (Some(Heard::Peer { from, text }) | Some(Heard::Reported { from, text })) = session.served.terminal.heard_of(sent) else { return true };
    link.send(wire::heard(&session.id, &from, &text));
    unprompted(session, Start::Peer { from, text }, link, input).await
}

/// A turn nobody prompted: the client hears it start and end. `false`: the
/// client went away during it.
async fn unprompted(session: &mut Session, start: Start, link: &Link, input: &mut Input) -> bool {
    link.send(wire::turn_started(&session.id));
    let ended = work(session, start, link, input).await;
    link.send(match ended {
        Ended::Finished(Ok(outcome)) => wire::turn_ended(&session.id, stop_reason(&outcome)),
        Ended::Finished(Err(e)) => wire::turn_failed(&session.id, &describe(e)),
        Ended::Cancelled => wire::turn_ended(&session.id, "cancelled"),
        Ended::ClientGone => return false,
    });
    true
}

/// How a turn ended.
enum Ended {
    Finished(Result<Outcome, rein_llm::Error>),
    Cancelled,
    /// Standard input closed.
    ClientGone,
}

/// What a client's prompt comes to. `None`: it went away.
async fn turn(session: &mut Session, prompt: String, link: &Link, input: &mut Input) -> Option<Answer> {
    session.in_a_row = 0;
    for (skill, given) in crate::skills::called(&prompt, session.agent.skills()) {
        session.agent.call_skill(&skill, given);
    }
    Some(match work(session, Start::You(prompt), link, input).await {
        Ended::Finished(Ok(outcome)) => Answer::Result(json!({ "stopReason": stop_reason(&outcome) })),
        Ended::Finished(Err(e)) => Answer::Refused(Refused::new(describe(e))),
        Ended::Cancelled => Answer::Result(json!({ "stopReason": "cancelled" })),
        Ended::ClientGone => return None,
    })
}

/// Runs a turn, reading what comes in alongside it, then sends each task
/// it took on its result.
async fn work(session: &mut Session, start: Start, link: &Link, input: &mut Input) -> Ended {
    session.served.starting();
    let ended = {
        let Session { agent, served, .. } = &mut *session;
        let work = async {
            match &start {
                Start::You(prompt) => agent.send(prompt.clone(), served).await,
                Start::Peer { from, text } => agent.hear(from, text, served).await,
                Start::Ended => agent.hear_ended(served).await,
            }
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                outcome = &mut work => break Ended::Finished(outcome),
                line = input.next_line() => {
                    let Ok(Some(line)) = line else { break Ended::ClientGone };
                    match Incoming::read(&line) {
                        Incoming::Cancel => break Ended::Cancelled,
                        Incoming::Reply { id, result } => link.answered(id, result),
                        Incoming::Request { id, request: Ok(Request::Steer { text, .. }) } => {
                            link.heard.borrow_mut().push(Heard::You(text));
                            link.send(wire::answer(&id, &json!({})));
                        }
                        Incoming::Request { id, .. } => link.send(wire::refuse(&id, &Refused::new("a turn is running: stop it, or wait for it to end"))),
                        Incoming::Unreadable(refused) => link.send(wire::refuse(&Value::Null, &refused)),
                        Incoming::Ignored => {}
                    }
                }
            }
        }
    };
    crate::skills::count(session.agent.take_loaded());
    // Whatever was waiting on an answer has gone with the turn.
    link.asked.borrow_mut().clear();
    if !matches!(ended, Ended::Finished(Ok(_))) {
        session.agent.repair();
    }
    let terminal = &mut session.served.terminal;
    let outcome = if let Ended::Finished(Ok(outcome)) = &ended { Some(outcome) } else { None };
    terminal.state(if outcome == Some(&Outcome::Done) { State::Done } else { State::Idle });
    // A client that went gets its results sent as `run` ends.
    if !matches!(ended, Ended::ClientGone) {
        if matches!(start, Start::You(_)) && terminal.tasks.is_empty() {
            talk::follow_up(terminal).await;
        }
        talk::report(terminal, outcome).await;
    }
    ended
}

fn stop_reason(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Done | Outcome::HookStopped(_) => "end_turn",
        Outcome::Stopped => "cancelled",
        Outcome::StepLimit(_) | Outcome::Stuck { .. } => "max_turn_requests",
        Outcome::Refused(_) => "refusal",
    }
}
