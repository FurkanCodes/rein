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

mod beside;
mod config;
mod options;
mod signing;
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
use crate::commands::Slash;
use crate::conversation::{self, Opened, Routed, Start, step_limit};
use crate::format::{shorten, utc_stamp};
use crate::peers::node::Incoming as Sent;
use crate::peers::talk::Taken;
use crate::peers::{self, State, talk};
use crate::settings::{self, Settings};
use crate::ui::history::{self, History};
use crate::{CliResult, config_dir, describe};
use beside::{Beside, room, undo_plan, undo_turn};
use ui::{Asker, Served};
use wire::{Incoming, Pasted, Refused, Request};

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
    /// The client can show what a command asks.
    can_ask: Rc<Cell<bool>>,
    /// The client has a switch of its own for keeping the mode and sandbox
    /// for new conversations, so changing them asks nothing.
    keeps: Rc<Cell<bool>>,
    /// What's asked of the conversation beside a turn.
    beside: Rc<RefCell<Option<Beside>>>,
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
    /// What commands need, and it holds the conversation's MCP servers.
    setup: conversation::Setup,
    /// It's another rein's sub-agent: a project's hooks it runs are only
    /// those you've said yes to already, as in a tab.
    sub_agent: bool,
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
    let link = Link {
        out,
        asked: Rc::default(),
        asks: Rc::new(Cell::new(0)),
        heard: Rc::default(),
        panes: Rc::default(),
        can_ask: Rc::default(),
        keeps: Rc::default(),
        beside: Rc::default(),
    };
    let mut input: Input = BufReader::new(tokio::io::stdin()).lines();
    let mut session: Option<Session> = None;
    loop {
        // A client that takes no turn nobody prompted hears with its next.
        let jobs = session.as_ref().filter(|_| link.panes.get()).map(|session| session.agent.tools.jobs().clone());
        tokio::select! {
            line = input.next_line() => {
                let Ok(Some(line)) = line else { break };
                match Incoming::read(&line) {
                    Incoming::Request { id, request: Ok(request) } if request.beside_a_turn() => link.answer_beside(&id, &request),
                    Incoming::Request { id, request } => {
                        let opens = matches!(request, Ok(Request::NewSession { .. } | Request::LoadSession { .. }));
                        let Some(goes_on) = handle(request, http, &flags, &mut session, &link, &mut input).await else { break };
                        goes_on.send(&link, &id);
                        // After the answer: only that says what it's about.
                        if let Some(session) = session.as_mut().filter(|_| opens) {
                            session.served.commands(&crate::skills::offers(session.agent.skills()));
                            session.served.state();
                            session.served.terminal.meters.ask(&session.agent.model.client, &session.agent.model_name);
                            // A project's hooks wait for a yes from someone who can give one.
                            if link.can_ask.get() && !session.sub_agent && !ask_about_hooks(session, &link, &mut input).await {
                                break;
                            }
                        }
                    }
                    Incoming::Unreadable(refused) => link.send(wire::refuse(&Value::Null, &refused)),
                    // Nothing's running to stop, and nothing was asked.
                    Incoming::Cancel | Incoming::Reply { .. } | Incoming::Ignored => {}
                }
            }
            outside = from_outside(&mut session) => {
                let Some(session) = &mut session else { continue };
                match outside {
                    Outside::Sent(sent) => {
                        if !take_in(session, sent, &link, &mut input).await {
                            break;
                        }
                    }
                    Outside::Meters(meters) => session.served.meters(&meters),
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
    /// Turned down, with more said in the error's `data`.
    RefusedWith(Refused, Value),
}

impl Answer {
    fn send(self, link: &Link, id: &Value) {
        link.send(match self {
            Answer::Result(result) => wire::answer(id, &result),
            Answer::Refused(refused) => wire::refuse(id, &refused),
            Answer::RefusedWith(refused, data) => wire::refuse_with(id, &refused, &data),
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
        Request::Initialize { panes, asks, keeps } => {
            link.panes.set(panes);
            link.can_ask.set(asks);
            link.keeps.set(keeps);
            Answer::Result(initialized())
        }
        Request::NewSession { .. } | Request::LoadSession { .. } if session.is_some() => {
            Answer::Refused(Refused::new("rein serve holds one conversation: start another rein serve for another"))
        }
        // A client that can show a sign-in shows it, rather than a failure.
        Request::NewSession { .. } if link.can_ask.get() && signing::signed_out(flags).unwrap_or(false) => match signing::ways() {
            Ok(ways) => Answer::RefusedWith(Refused { code: wire::SIGNED_OUT, message: "you're signed in to nothing that runs the model".to_owned() }, ways),
            Err(said) => Answer::Refused(Refused::new(said)),
        },
        Request::SignIn { way } => match alongside(signing::sign_in(way, http, link), false, link, input).await {
            Alongside::ClientGone => return None,
            Alongside::Done(Ok(true)) => Answer::Result(json!({ "signedIn": true })),
            Alongside::Done(Ok(false)) | Alongside::Cancelled => Answer::Result(json!({ "signedIn": false })),
            Alongside::Done(Err(why)) => Answer::Refused(Refused::new(why)),
        },
        Request::NewSession { cwd } => match open(&cwd, Resume::New, flags, http, link).await {
            Ok(opened) => {
                let result = json!({ "sessionId": opened.id, "modes": modes(opened.agent.permissions.mode, opened.sandboxing), "configOptions": options::all(&opened.agent, &opened.setup) });
                *session = Some(opened);
                Answer::Result(result)
            }
            Err(said) => Answer::Refused(Refused::new(said)),
        },
        Request::LoadSession { session: id, cwd } => match load(&id, &cwd, flags, http, link).await {
            Ok(opened) => {
                let result = json!({ "modes": modes(opened.agent.permissions.mode, opened.sandboxing), "configOptions": options::all(&opened.agent, &opened.setup) });
                *session = Some(opened);
                Answer::Result(result)
            }
            Err(said) => Answer::Refused(Refused::new(said)),
        },
        Request::ListSessions { cwd, skip } => match listed(cwd.as_deref(), skip) {
            Ok(result) => Answer::Result(result),
            Err(said) => Answer::Refused(Refused::new(said)),
        },
        Request::Prompt { session: id, text, images } => match mine(session, &id) {
            Ok(session) => return turn(session, text, images, link, input).await,
            Err(refused) => Answer::Refused(refused),
        },
        // `run` answers these, as `alongside` does.
        Request::Ways
        | Request::Sent { .. }
        | Request::Paths { .. }
        | Request::Background { .. }
        | Request::Tasks { .. }
        | Request::StopTask { .. }
        | Request::Room { .. }
        | Request::Config { .. }
        | Request::SetConfig { .. } => Answer::Result(Value::Null),
        Request::Rule { session: id, rule, add } => match mine(session, &id) {
            Ok(session) => {
                let done = if add {
                    crate::rules::added(&rule, &mut session.agent).map(|_| ())
                } else {
                    rein_agent::permissions::Rule::parse(&rule, rein_agent::permissions::Whose::Yours).and_then(|rule| crate::rules::removed(&rule, &mut session.agent))
                };
                match done {
                    Ok(()) => {
                        link.room_changed(&session.agent, &session.served.terminal);
                        Answer::Result(json!({}))
                    }
                    Err(why) => Answer::Refused(Refused::params(why)),
                }
            }
            Err(refused) => Answer::Refused(refused),
        },
        // The list goes out with the options, as the others' are looked up.
        Request::Models { session: id } => match mine(session, &id) {
            Ok(session) => {
                conversation::other_models(&mut session.setup, &mut session.served.terminal).await;
                session.served.options(options::all(&session.agent, &session.setup));
                Answer::Result(json!({}))
            }
            Err(refused) => Answer::Refused(refused),
        },
        Request::Changes { session: id, span, lines } => match mine(session, &id) {
            Ok(session) => Answer::Result(wire::changes(&session.agent.changes(span, lines).await, lines)),
            Err(refused) => Answer::Refused(refused),
        },
        Request::UndoTurns { session: id } => match mine(session, &id) {
            Ok(session) => Answer::Result(json!({ "turns": session.agent.undo_list().await.iter().map(undo_turn).collect::<Vec<Value>>() })),
            Err(refused) => Answer::Refused(refused),
        },
        Request::UndoPlan { session: id, turn } => match mine(session, &id) {
            Ok(session) => Answer::Result(json!({ "plan": undo_plan(&session.agent.undo_plan(turn).await) })),
            Err(refused) => Answer::Refused(refused),
        },
        Request::Undo { session: id, turn } => match mine(session, &id) {
            Ok(session) => match session.agent.undo(turn).await {
                Ok(done) => {
                    let _said = session.served.take_said();
                    crate::undo::say_done(&done, &session.served.terminal);
                    Answer::Result(json!({ "plan": undo_plan(&done) }))
                }
                Err(e) => Answer::Refused(Refused::new(describe(e))),
            },
            Err(refused) => Answer::Refused(refused),
        },
        Request::SetMode { session: id, mode } => match mine(session, &id) {
            Ok(session) => {
                conversation::set_mode(mode, &mut session.agent, &mut session.served.terminal, session.max_steps);
                session.served.options(options::all(&session.agent, &session.setup));
                Answer::Result(json!({}))
            }
            Err(refused) => Answer::Refused(refused),
        },
        Request::SetOption { session: id, option, value } if option == "keep" => match mine(session, &id) {
            Ok(session) => match value.as_str() {
                "on" | "off" => match options::set_keep(&session.agent, value == "on") {
                    Ok(()) => Answer::Result(json!({ "configOptions": options::all(&session.agent, &session.setup) })),
                    Err(why) => Answer::Refused(Refused::new(why)),
                },
                other => Answer::Refused(Refused::params(format!("keep is on or off, not {other}"))),
            },
            Err(refused) => Answer::Refused(refused),
        },
        Request::SetOption { session: id, option, value } => match mine(session, &id) {
            Ok(session) => match options::command(&option, &value) {
                Ok(chosen) => match setting(session, chosen, link, input).await {
                    Alongside::ClientGone => return None,
                    Alongside::Done(Err(why)) => Answer::Refused(Refused::new(why)),
                    Alongside::Done(Ok(())) | Alongside::Cancelled => Answer::Result(json!({ "configOptions": options::all(&session.agent, &session.setup) })),
                },
                Err(refused) => Answer::Refused(refused),
            },
            Err(refused) => Answer::Refused(refused),
        },
        // With no turn running, it goes in as the next one starts.
        Request::Steer { session: id, text } => match mine(session, &id) {
            Ok(_) => {
                link.steer(text);
                Answer::Result(json!({}))
            }
            Err(refused) => Answer::Refused(refused),
        },
    })
}

/// Asks about the project's hooks that wait for a yes. `false`: the client
/// went away.
async fn ask_about_hooks(session: &mut Session, link: &Link, input: &mut Input) -> bool {
    let Ok(dir) = config_dir() else { return true };
    let Session { agent, served, .. } = &mut *session;
    let _said = served.take_said();
    let asked = alongside(crate::hooks::ask_waiting(agent, &mut served.terminal, &dir), false, link, input).await;
    link.asked.borrow_mut().clear();
    !matches!(asked, Alongside::ClientGone)
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
            "promptCapabilities": { "image": true, "audio": false, "embeddedContext": false },
            "mcpCapabilities": { "http": false, "sse": false },
            "_meta": { (wire::STEERS): true, (wire::PANES): true, (wire::ASKS): true },
        },
        "agentInfo": { "name": "rein", "title": "rein", "version": env!("CARGO_PKG_VERSION") },
        "authMethods": [],
    })
}

/// The modes, as `/mode` lists them, marks and all.
fn modes(mode: Mode, sandboxing: Sandboxing) -> Value {
    let available: Vec<Value> = Mode::ALL
        .iter()
        .map(|&mode| json!({ "id": mode.as_str(), "name": crate::modes::shown_name(mode, sandboxing), "description": crate::modes::brief(mode, sandboxing) }))
        .collect();
    json!({ "currentModeId": mode.as_str(), "availableModes": available })
}

/// Opens the conversation in `cwd`, as a task run on its own: nobody's there
/// for what a project brings to wait on, so that stays off.
async fn open(cwd: &Path, resume: Resume, flags: &[String], http: &rein_llm::reqwest::Client, link: &Link) -> CliResult<Session> {
    std::env::set_current_dir(cwd).map_err(|e| format!("can't go into {}: {e}", cwd.display()))?;
    let dir = config_dir()?;
    let settings = Settings::load(&dir.join(settings::FILE))?;
    let compact_at = settings.get("compact_at");
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
    // Its standard input is this protocol: nothing else may read it.
    terminal.input.stand_in(Rc::new(Asker { session: id.clone(), link: link.clone() }));
    let (root, sandbox, jobs) = (agent.tools.root().to_owned(), agent.tools.sandbox().clone(), agent.tools.jobs().clone());
    let sent = History::load(history::file(&dir, &root));
    let room = room(&agent, &terminal);
    let looking = agent.tools.looking();
    let found = config::found(&agent);
    *link.beside.borrow_mut() = Some(Beside { session: id.clone(), root, sandbox, jobs, sent, room, looking, found, flags: flags.to_vec() });
    let served = Served::new(id.clone(), link.clone(), terminal);
    Ok(Session { id, agent, served, max_steps, sandboxing, in_a_row: 0, setup, sub_agent: child.is_some() })
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

/// What came from outside the client.
enum Outside {
    /// Another rein sent this.
    Sent(Sent),
    /// The provider says this of your limits.
    Meters(Vec<rein_llm::Meter>),
}

/// The next thing from outside; never, before there's a conversation.
async fn from_outside(session: &mut Option<Session>) -> Outside {
    let Some(session) = session else { return std::future::pending().await };
    let terminal = &mut session.served.terminal;
    tokio::select! {
        sent = conversation::from_peers(&mut terminal.peers) => Outside::Sent(sent),
        meters = terminal.meters.answered() => Outside::Meters(meters),
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
        // Not what your rules hold back: that stays in `/inbox`, for you.
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

/// How something run alongside what comes in came out.
enum Alongside<T> {
    Done(T),
    Cancelled,
    /// Standard input closed.
    ClientGone,
}

/// Runs `work`, reading what comes in as it goes: the answers it's waiting
/// for, a stop, and only if it `steers`, what you say to a turn. Anything else
/// has to wait for it.
async fn alongside<T>(work: impl Future<Output = T>, steers: bool, link: &Link, input: &mut Input) -> Alongside<T> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            done = &mut work => return Alongside::Done(done),
            line = input.next_line() => {
                let Ok(Some(line)) = line else { return Alongside::ClientGone };
                match Incoming::read(&line) {
                    Incoming::Cancel => return Alongside::Cancelled,
                    Incoming::Reply { id, result } => link.answered(id, result),
                    Incoming::Request { id, request: Ok(Request::Steer { text, .. }) } if steers => {
                        link.steer(text);
                        link.send(wire::answer(&id, &json!({})));
                    }
                    Incoming::Request { id, request: Ok(Request::Changes { session, span, lines }) } => link.changes_beside(&id, &session, span, lines),
                    Incoming::Request { id, request: Ok(request) } if request.beside_a_turn() => link.answer_beside(&id, &request),
                    Incoming::Request { id, .. } => link.send(wire::refuse(&id, &Refused::busy("a turn is running: stop it, or wait for it to end"))),
                    Incoming::Unreadable(refused) => link.send(wire::refuse(&Value::Null, &refused)),
                    Incoming::Ignored => {}
                }
            }
        }
    }
}

/// What a client's prompt comes to: a command, as ACP has them, or a message.
/// `None`: it went away.
async fn turn(session: &mut Session, prompt: String, images: Vec<Pasted>, link: &Link, input: &mut Input) -> Option<Answer> {
    session.in_a_row = 0;
    link.remember(&prompt);
    // What starts with a `/` may have a key in it, and only rein knows.
    if prompt.trim_start().starts_with('/') {
        session.served.ran(prompt.trim());
    }
    let routed = {
        let Session { agent, served, setup, .. } = &mut *session;
        let _said = served.take_said();
        alongside(conversation::routed(prompt.trim(), agent, &mut served.terminal, setup), false, link, input).await
    };
    // Looking at the skills for this message may have found others.
    session.served.commands(&crate::skills::offers(session.agent.skills()));
    let done = || Answer::Result(json!({ "stopReason": "end_turn" }));
    let cancelled = || Answer::Result(json!({ "stopReason": "cancelled" }));
    Some(match routed {
        Alongside::Done(Routed::Message) => {
            if let Err(why) = take_images(session, images).await {
                return Some(Answer::Refused(Refused::new(why)));
            }
            match work(session, Start::You(prompt), link, input).await {
                Ended::Finished(Ok(outcome)) => Answer::Result(json!({ "stopReason": stop_reason(&outcome) })),
                Ended::Finished(Err(e)) => Answer::Refused(Refused::new(describe(e))),
                Ended::Cancelled => cancelled(),
                Ended::ClientGone => return None,
            }
        }
        Alongside::Done(Routed::Command(parsed)) => match command(session, parsed, link, input).await {
            Alongside::Done(()) => done(),
            Alongside::Cancelled => cancelled(),
            Alongside::ClientGone => return None,
        },
        Alongside::Done(Routed::Neither) => done(),
        Alongside::Cancelled => cancelled(),
        Alongside::ClientGone => return None,
    })
}

/// Checks and keeps what came with a message, as Ctrl-V does, for it to go
/// with it: all of them, or none, and why. Your words aren't sent without
/// what they're about.
async fn take_images(session: &mut Session, images: Vec<Pasted>) -> Result<(), String> {
    let mut taken = Vec::with_capacity(images.len());
    for pasted in images {
        let folder = session.setup.images();
        let what = pasted.label.map_or_else(|| "the image you pasted".to_owned(), |label| format!("the image you pasted as {label}"));
        let prepared = tokio::task::spawn_blocking(move || rein_agent::image::prepare(&pasted.bytes, &what, &folder));
        taken.push(prepared.await.unwrap_or_else(|e| Err(e.to_string()))?);
    }
    for image in taken {
        session.agent.paste(image);
    }
    Ok(())
}

/// `/inbox`: what you let in is a turn here like any another rein starts,
/// so its steps reach the client, which is asked what it asks. The
/// terminal's would run behind it, where Stop leaves a call unanswered.
async fn inbox(session: &mut Session, link: &Link, input: &mut Input) -> Alongside<()> {
    let picked = {
        let _said = session.served.take_said();
        match alongside(talk::pick_held(&mut session.served.terminal), false, link, input).await {
            Alongside::Done(Ok(picked)) => picked,
            Alongside::Done(Err(e)) => {
                say!("✗ {e}");
                None
            }
            Alongside::Cancelled => return Alongside::Cancelled,
            Alongside::ClientGone => return Alongside::ClientGone,
        }
    };
    let Some(incoming) = picked else { return Alongside::Done(()) };
    let (Some(Heard::Peer { from, text }) | Some(Heard::Reported { from, text })) = session.served.terminal.heard_of(incoming) else { return Alongside::Done(()) };
    link.send(wire::heard(&session.id, &from, &text));
    match work(session, Start::Peer { from, text }, link, input).await {
        Ended::Finished(outcome) => {
            if let Err(e) = outcome {
                let _said = session.served.take_said();
                say!("✗ {}", describe(e));
            }
            Alongside::Done(())
        }
        Ended::Cancelled => Alongside::Cancelled,
        Ended::ClientGone => Alongside::ClientGone,
    }
}

/// Runs a command. What it says goes to the client, and a command that
/// fails says so there too, as it does in a terminal: it isn't a turn, and
/// the conversation goes on.
async fn command(session: &mut Session, parsed: Result<Slash, crate::commands::Refused>, link: &Link, input: &mut Input) -> Alongside<()> {
    if matches!(parsed, Ok(Slash::Inbox)) {
        return inbox(session, link, input).await;
    }
    let Session { agent, served, setup, .. } = &mut *session;
    let _said = served.take_said();
    let done = match parsed {
        // A process holds one conversation, and what starts or ends one is
        // the client.
        Ok(Slash::New | Slash::Resume | Slash::Exit) => Err("that's for your client: close this conversation, or start or open another".to_owned()),
        // A sub-agent here is a pane, never a terminal's tab.
        Ok(Slash::Spawn(spawn)) => {
            let work = async {
                let Some(args) = talk::spawn_args(spawn, &mut served.terminal).await else { return Ok(()) };
                let said = served.spawn_yours(args).await;
                match said.strip_prefix(rein_agent::tools::ERROR) {
                    Some(why) => Err(why.to_owned()),
                    None => {
                        say!("{said}");
                        Ok(())
                    }
                }
            };
            match alongside(work, false, link, input).await {
                Alongside::Done(done) => done,
                Alongside::Cancelled => return Alongside::Cancelled,
                Alongside::ClientGone => return Alongside::ClientGone,
            }
        }
        Ok(command) => match alongside(conversation::run_command(command, agent, &mut served.terminal, setup), false, link, input).await {
            Alongside::Done(done) => done,
            Alongside::Cancelled => {
                link.asked.borrow_mut().clear();
                return Alongside::Cancelled;
            }
            Alongside::ClientGone => return Alongside::ClientGone,
        },
        Err(refused) => Err(refused.to_string()),
    };
    link.asked.borrow_mut().clear();
    if let Err(e) = done {
        say!("✗ {e}");
    }
    say_changes(agent, served, setup, link);
    Alongside::Done(())
}

/// Sets what was chosen in a client's own control, which shows it: nothing's
/// said of it, as nobody wrote it, and what stopped it is the answer. A mode
/// or a sandbox shows as set while rein asks whether to keep it.
async fn setting(session: &mut Session, chosen: Slash, link: &Link, input: &mut Input) -> Alongside<CliResult> {
    let Session { id, agent, served, setup, max_steps, .. } = &mut *session;
    let unsaid = served.leave_unsaid();
    let done = match chosen {
        // A client with a switch for keeping them asks nothing.
        Slash::Mode(Some(mode)) if link.keeps.get() => {
            conversation::set_mode(mode, agent, &mut served.terminal, *max_steps);
            Alongside::Done(Ok(()))
        }
        Slash::Mode(Some(mode)) => {
            conversation::set_mode(mode, agent, &mut served.terminal, *max_steps);
            served.options(options::all(agent, setup));
            alongside(crate::modes::keep_mode(mode, &mut served.terminal), false, link, input).await
        }
        Slash::Sandbox(Some(on)) if link.keeps.get() => {
            conversation::set_sandbox(on, agent, &mut served.terminal);
            if let Some(missing) = crate::modes::walls_missing(served.terminal.sandboxing, agent.permissions.sandbox()) {
                link.send(wire::noted(id, &missing));
            }
            Alongside::Done(Ok(()))
        }
        Slash::Sandbox(Some(on)) => {
            conversation::set_sandbox(on, agent, &mut served.terminal);
            served.options(options::all(agent, setup));
            if let Some(missing) = crate::modes::walls_missing(served.terminal.sandboxing, agent.permissions.sandbox()) {
                link.send(wire::noted(id, &missing));
            }
            alongside(crate::modes::keep_sandbox(on, &mut served.terminal), false, link, input).await
        }
        chosen => alongside(conversation::run_command(chosen, agent, &mut served.terminal, setup), false, link, input).await,
    };
    drop(unsaid);
    link.asked.borrow_mut().clear();
    say_changes(agent, served, setup, link);
    done
}

/// Tells the client what a command left changed.
fn say_changes(agent: &Agent<Llm>, served: &mut Served, setup: &conversation::Setup, link: &Link) {
    served.options(options::all(agent, setup));
    served.commands(&crate::skills::offers(agent.skills()));
    served.state();
    link.room_changed(agent, &served.terminal);
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
        match alongside(work, true, link, input).await {
            Alongside::Done(outcome) => Ended::Finished(outcome),
            Alongside::Cancelled => Ended::Cancelled,
            Alongside::ClientGone => Ended::ClientGone,
        }
    };
    crate::skills::count(session.agent.take_loaded());
    // Whatever was waiting on an answer has gone with the turn.
    link.asked.borrow_mut().clear();
    if !matches!(ended, Ended::Finished(Ok(_))) {
        session.agent.repair();
    }
    // What it comes to, and what it can be undone back to if it changed files.
    let undo = if session.served.terminal.turn.changed_files() { session.agent.undo_list().await.first().map(|turn| turn.turn) } else { None };
    if !matches!(ended, Ended::ClientGone) {
        session.served.turn_ended(undo);
    }
    link.room_changed(&session.agent, &session.served.terminal);
    // A model can turn an effort down, and a turn tells of your limits.
    session.served.options(options::all(&session.agent, &session.setup));
    session.served.state();
    session.served.terminal.meters.ask(&session.agent.model.client, &session.agent.model_name);
    let terminal = &mut session.served.terminal;
    let outcome = if let Ended::Finished(Ok(outcome)) = &ended { Some(outcome) } else { None };
    terminal.state(if outcome == Some(&Outcome::Done) { State::Done } else { State::Idle });
    // A client that went gets its results sent as `run` ends.
    if !matches!(ended, Ended::ClientGone) {
        if matches!(start, Start::You(_)) && terminal.tasks.is_empty() {
            talk::follow_up(terminal).await;
        }
        talk::report(terminal, outcome.as_ref().copied()).await;
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
