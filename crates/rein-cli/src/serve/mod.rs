//! `rein serve`: rein with no screen, for another program to drive over its
//! standard input and output, as an agent of the Agent Client Protocol.
//!
//! A process holds one conversation, as a rein in a tab does; a client that
//! wants another starts another. What comes in while a turn runs is read
//! alongside it, so a client can answer a question, steer or stop it. Only
//! messages go to standard output: what rein says goes to standard error,
//! and nothing it runs gets its standard input.

mod ui;
mod wire;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use rein_agent::agent::{Agent, Heard, Llm, Outcome};
use rein_agent::permissions::{Mode, Sandboxing};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, Stdin};
use tokio::sync::{mpsc, oneshot};

use crate::args::AskArgs;
use crate::conversation::{self, Opened, step_limit};
use crate::settings::{self, Settings};
use crate::{CliResult, config_dir, describe};
use ui::Served;
use wire::{Incoming, Refused, Request};

type Input = Lines<BufReader<Stdin>>;

/// What the parts of `serve` share: the way out, and what came in for a
/// turn that's running.
#[derive(Clone)]
struct Link {
    out: mpsc::UnboundedSender<String>,
    asked: Rc<RefCell<HashMap<u64, oneshot::Sender<Option<Value>>>>>,
    asks: Rc<Cell<u64>>,
    heard: Rc<RefCell<Vec<Heard>>>,
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
    /// Kept for as long as the conversation: it holds its MCP servers.
    _setup: conversation::Setup,
    /// What setting up reports to, which draws nothing here.
    _terminal: crate::terminal::Terminal,
}

/// Runs until the client closes standard input.
pub(crate) async fn run(http: &rein_llm::reqwest::Client) -> CliResult {
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
    let link = Link { out, asked: Rc::default(), asks: Rc::new(Cell::new(0)), heard: Rc::default() };
    let mut input: Input = BufReader::new(tokio::io::stdin()).lines();
    let mut session: Option<Session> = None;
    while let Ok(Some(line)) = input.next_line().await {
        match Incoming::read(&line) {
            Incoming::Request { id, request } => {
                let Some(goes_on) = handle(request, http, &mut session, &link, &mut input).await else { break };
                goes_on.send(&link, &id);
            }
            Incoming::Unreadable(refused) => link.send(wire::refuse(&Value::Null, &refused)),
            // Nothing's running to stop, and nothing was asked.
            Incoming::Cancel | Incoming::Reply { .. } | Incoming::Ignored => {}
        }
    }
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
async fn handle(request: Result<Request, Refused>, http: &rein_llm::reqwest::Client, session: &mut Option<Session>, link: &Link, input: &mut Input) -> Option<Answer> {
    let request = match request {
        Ok(request) => request,
        Err(refused) => return Some(Answer::Refused(refused)),
    };
    Some(match request {
        Request::Initialize => Answer::Result(initialized()),
        Request::NewSession { .. } if session.is_some() => Answer::Refused(Refused::new("rein serve holds one conversation: start another rein serve for another")),
        Request::NewSession { cwd } => match open(&cwd, http, link).await {
            Ok(opened) => {
                let result = json!({ "sessionId": opened.id, "modes": modes(opened.agent.permissions.mode, opened.sandboxing) });
                *session = Some(opened);
                Answer::Result(result)
            }
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
            "loadSession": false,
            "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false },
            "mcpCapabilities": { "http": false, "sse": false },
            "_meta": { (wire::STEERS): true },
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
async fn open(cwd: &Path, http: &rein_llm::reqwest::Client, link: &Link) -> CliResult<Session> {
    std::env::set_current_dir(cwd).map_err(|e| format!("can't go into {}: {e}", cwd.display()))?;
    let dir = config_dir()?;
    let settings = Settings::load(&dir.join(settings::FILE))?;
    let compact_at = settings.get("compact_at", "REIN_COMPACT_AT");
    let args = AskArgs::parse(Vec::new(), &settings)?;
    let (mode, max_steps) = (args.mode, args.max_steps);
    let opened = conversation::open(http, args, compact_at, true).await?.ok_or("the conversation didn't open")?;
    let Opened { mut agent, terminal, setup } = opened;
    // Someone's there, to press stop.
    agent.step_limit = step_limit(mode, false, max_steps);
    let id = format!("rein-{}", std::process::id());
    let sandboxing = agent.permissions.sandboxing();
    let served = Served::new(id.clone(), link.clone(), terminal.context_limit);
    Ok(Session { id, agent, served, max_steps, sandboxing, _setup: setup, _terminal: terminal })
}

/// How a turn ended.
enum Ended {
    Finished(Result<Outcome, rein_llm::Error>),
    Cancelled,
    /// Standard input closed.
    ClientGone,
}

/// Runs a turn, reading what comes in alongside it.
async fn turn(session: &mut Session, prompt: String, link: &Link, input: &mut Input) -> Option<Answer> {
    for (skill, given) in crate::skills::called(&prompt, session.agent.skills()) {
        session.agent.call_skill(&skill, given);
    }
    let ended = {
        let work = session.agent.send(prompt, &mut session.served);
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
    let stop = match ended {
        Ended::Finished(Ok(outcome)) => stop_reason(&outcome),
        Ended::Finished(Err(e)) => {
            session.agent.repair();
            return Some(Answer::Refused(Refused::new(describe(e))));
        }
        Ended::Cancelled => {
            session.agent.repair();
            "cancelled"
        }
        Ended::ClientGone => {
            session.agent.repair();
            return None;
        }
    };
    Some(Answer::Result(json!({ "stopReason": stop })))
}

fn stop_reason(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Done | Outcome::HookStopped(_) => "end_turn",
        Outcome::Stopped => "cancelled",
        Outcome::StepLimit(_) | Outcome::Stuck { .. } => "max_turn_requests",
        Outcome::Refused(_) => "refusal",
    }
}
