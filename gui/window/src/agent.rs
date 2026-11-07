//! An agent behind a pane: a process speaking the Agent Client Protocol.
//! Its output is read on a thread of its own, so a loud agent never holds
//! up drawing, and what it says waits in a queue for the window to take.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::channel;
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use rein_transcript::{Asking, Meter, Modes, Saved, Setting, Words};
use serde_json::{Value, json};

/// What an agent said.
#[derive(Debug, PartialEq)]
pub(crate) enum Said {
    Update(Value),
    /// Another rein's words.
    Heard {
        from: String,
        text: String,
    },
    /// It asks, under this number, whether step `call` may run, offering
    /// `options`. `rein serve` says which step and no more: what it does was
    /// said as it was shown.
    Asked {
        id: Value,
        call: String,
        options: Value,
        words: Words,
    },
    /// It asks, under this number, for a sub-agent to be started: `None` if
    /// what it asked can't be read.
    SubAgent {
        id: Value,
        asked: Option<SubAgent>,
    },
    /// A command asks you something, under this number.
    Asking {
        id: Value,
        asking: Asking,
    },
    /// The modes it offers, and the one it's in.
    Modes(Modes),
    /// All that can be set, as it stands now.
    Settings(Vec<Setting>),
    /// What a window shows that ACP has no word for.
    State {
        branch: Option<String>,
        meters: Vec<Meter>,
        /// How many commands are running in the background.
        background: u64,
    },
    /// The conversation has started, under this id.
    Started(String),
    /// It couldn't start: you're signed in to nothing that runs the model,
    /// and these are the ways there are.
    SignedOut(Vec<rein_transcript::signing::Offered>),
    /// A turn nobody here prompted has started.
    TurnStarted,
    /// A turn came to an end.
    TurnOver,
    /// What it was doing failed, and this is why.
    Failed(String),
    /// What came of the request sent under this number.
    Answer {
        id: u64,
        came_to: CameTo,
    },
    Ended,
}

/// What came of a request. Whoever sent it knows what that means: a mode
/// that's turned down hasn't failed a turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CameTo {
    Done,
    /// The turn it started has ended.
    TurnOver,
    /// It can't be done while a turn runs.
    Busy(String),
    Refused(String),
    /// It answered, and this is what it said.
    Told(Value),
}

/// An image to go with a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attached {
    /// What the message calls it: `[Image #1]`.
    pub(crate) label: String,
    pub(crate) mime: &'static str,
    /// In base64, as it goes.
    pub(crate) data: String,
}

/// A sub-agent to run: `rein serve` says what to start, and it's started
/// with no shell in between.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SubAgent {
    pub(crate) name: String,
    pub(crate) command: Vec<String>,
    pub(crate) cwd: String,
    /// Added to what the window has.
    pub(crate) env: Vec<(String, String)>,
}

impl SubAgent {
    fn of(params: &Value) -> Option<SubAgent> {
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let mut command = vec![text(&params["command"])?];
        for arg in params["args"].as_array()? {
            command.push(text(arg)?);
        }
        let env = params["env"].as_array()?.iter().map(|one| Some((text(&one["name"])?, text(&one["value"])?))).collect::<Option<_>>()?;
        Some(SubAgent { name: text(&params["name"])?, command, cwd: text(&params["cwd"])?, env })
    }
}

/// What an agent starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Opening {
    New,
    /// The saved conversation with this id.
    Saved(String),
}

/// What a stop came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stopping {
    /// The agent's been told, and what ends the turn will say so.
    Told,
    /// There's no conversation yet, so no turn: what was waiting for one
    /// went, and nothing will say it ended.
    NeverBegan,
}

/// How to start an agent.
pub(crate) struct Start<'a> {
    pub(crate) command: &'a [String],
    pub(crate) cwd: &'a str,
    pub(crate) env: &'a [(String, String)],
    pub(crate) opening: Opening,
}

pub(crate) struct Agent {
    /// Taken as it's closed.
    child: Option<Child>,
    /// Closing it is what ends the agent, so a drop takes it first.
    input: Option<ChildStdin>,
    /// Taken by whoever listens, which then wakes only when there's news.
    pub(crate) said: Option<UnboundedReceiver<Said>>,
    session: Option<String>,
    /// What was written before the conversation had started: the first is
    /// a prompt, and the rest steer it.
    waiting: Vec<(u64, String, Vec<Attached>)>,
    /// What opens the conversation, to send again once you've signed in.
    opens: Value,
}

const NEW: u64 = 2;
/// How long an agent gets to stop what it started, once its input closes.
const TO_END: Duration = Duration::from_secs(3);
/// How long a list of saved conversations may take.
const LIST_WITHIN: Duration = Duration::from_secs(10);
/// The most pages of saved conversations asked for, so that an agent that
/// always says there's more can't hold the list open.
const PAGES_MOST: usize = 1000;
/// What `rein serve` says of a request it can't take while a turn runs.
const BUSY: i64 = -32001;
/// Said in `initialize`: this window starts sub-agents, each in a pane.
const PANES: &str = "io.github.furkancodes/panes";
const OPEN_PANE: &str = "_io.github.furkancodes/open_pane";
const TURN_STARTED: &str = "_io.github.furkancodes/turn_started";
const TURN_ENDED: &str = "_io.github.furkancodes/turn_ended";
const STATE: &str = "_io.github.furkancodes/state";
/// Said in `initialize`: this window shows what a command asks.
const ASKS: &str = "io.github.furkancodes/asks";
/// Said in `initialize`: this window has a switch for keeping the mode and
/// sandbox for new conversations, so changing them needn't ask.
const KEEPS: &str = "io.github.furkancodes/keeps";
/// What you sent from this folder, and what `@` can point at.
const SENT: &str = "_io.github.furkancodes/sent";
const PATHS: &str = "_io.github.furkancodes/paths";
const BACKGROUND: &str = "_io.github.furkancodes/background";
const TASKS: &str = "_io.github.furkancodes/tasks";
const STOP_TASK: &str = "_io.github.furkancodes/stop_task";
const ROOM: &str = "_io.github.furkancodes/room";
const MODELS: &str = "_io.github.furkancodes/models";
const CHANGES: &str = "_io.github.furkancodes/changes";
const UNDO_TURNS: &str = "_io.github.furkancodes/undo_turns";
const UNDO_PLAN: &str = "_io.github.furkancodes/undo_plan";
const UNDO: &str = "_io.github.furkancodes/undo";
const LABEL: &str = "io.github.furkancodes/label";
const SIGN_IN: &str = "_io.github.furkancodes/sign_in";
const WAYS: &str = "_io.github.furkancodes/ways";
/// Your settings, for the settings page; a key of them set; a rule of
/// yours added or taken out.
const CONFIG: &str = "_io.github.furkancodes/config";
const SET_CONFIG: &str = "_io.github.furkancodes/set_config";
const RULE: &str = "_io.github.furkancodes/rule";

fn initialize() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": 1, "clientCapabilities": { "_meta": { PANES: true, ASKS: true, KEEPS: true } } } })
}

fn send(input: &mut impl Write, message: &Value) {
    // Gone only once the agent has: its output's end says so.
    let _ = writeln!(input, "{message}").and_then(|()| input.flush());
}

/// What an answer to a request comes to: opening the conversation is the
/// agent's own affair, and whoever sent any other is told by its number.
fn answered(message: &Value, opening: &Opening) -> Option<Said> {
    let id = message["id"].as_u64()?;
    let error = &message["error"];
    let why = || error["message"].as_str().unwrap_or("That failed.").to_owned();
    if id == NEW {
        if error["code"] == rein_transcript::signing::SIGNED_OUT {
            return Some(Said::SignedOut(rein_transcript::signing::ways(&error["data"])));
        }
        if error.is_object() {
            return Some(Said::Failed(why()));
        }
        // `session/load` says no id: the conversation already has one.
        return match opening {
            Opening::New => message["result"]["sessionId"].as_str().map(|session| Said::Started(session.to_owned())),
            Opening::Saved(session) => message["result"].is_object().then(|| Said::Started(session.clone())),
        };
    }
    let came_to = if error["code"] == BUSY {
        CameTo::Busy(why())
    } else if error.is_object() {
        CameTo::Refused(why())
    } else if message["result"]["stopReason"].is_string() {
        CameTo::TurnOver
    } else if message["result"].as_object().is_some_and(|said| !said.is_empty()) {
        CameTo::Told(message["result"].clone())
    } else {
        CameTo::Done
    };
    (id > NEW).then_some(Said::Answer { id, came_to })
}

/// Asks for the page of saved conversations after `cursor`.
fn list_after(id: u64, cursor: Option<&str>) -> Value {
    let params = cursor.map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
    json!({ "jsonrpc": "2.0", "id": id, "method": "session/list", "params": params })
}

/// A turn nobody prompted has ended, so no answer says so.
fn turn_ended(params: &Value) -> Said {
    params["error"].as_str().map_or(Said::TurnOver, |why| Said::Failed(why.to_owned()))
}

fn read(output: impl BufRead, said: &UnboundedSender<Said>, opening: &Opening) {
    for line in output.lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        // With no conversation there's nothing more it can do: left open,
        // what you write would wait for one for good. Signed out, it waits
        // for you to sign in.
        let no_conversation = message["id"] == NEW && message["error"].is_object() && message["error"]["code"] != rein_transcript::signing::SIGNED_OUT;
        if message["id"] == NEW {
            let settings = Some(Setting::listed(&message["result"]["configOptions"])).filter(|settings| !settings.is_empty()).map(Said::Settings);
            let modes = Modes::of(&message["result"]["modes"]).map(Said::Modes);
            if modes.into_iter().chain(settings).any(|one| said.unbounded_send(one).is_err()) {
                return;
            }
        }
        let params = &message["params"];
        let one = match message["method"].as_str() {
            Some("session/update") if params["update"]["sessionUpdate"] == "config_option_update" => Some(Said::Settings(Setting::listed(&params["update"]["configOptions"]))),
            Some("session/update") => Some(Said::Update(params["update"].clone())),
            Some(STATE) => {
                Some(Said::State { branch: params["branch"].as_str().map(str::to_owned), meters: Meter::listed(params), background: params["background"].as_u64().unwrap_or(0) })
            }
            Some("session/request_permission") => Some(Said::Asked {
                id: message["id"].clone(),
                call: params["toolCall"]["toolCallId"].as_str().unwrap_or_default().to_owned(),
                options: params["options"].clone(),
                words: Words::of(params),
            }),
            Some(OPEN_PANE) => Some(Said::SubAgent { id: message["id"].clone(), asked: SubAgent::of(params) }),
            Some(TURN_STARTED) => Some(Said::TurnStarted),
            Some(TURN_ENDED) => Some(turn_ended(params)),
            Some("_io.github.furkancodes/heard") => {
                Some(Said::Heard { from: params["from"].as_str().unwrap_or("?").to_owned(), text: params["text"].as_str().unwrap_or_default().to_owned() })
            }
            Some(method) => Asking::of(method, params).map(|asking| Said::Asking { id: message["id"].clone(), asking }),
            None => answered(&message, opening),
        };
        if let Some(one) = one
            && said.unbounded_send(one).is_err()
        {
            return;
        }
        if no_conversation {
            break;
        }
    }
    let _ = said.unbounded_send(Said::Ended);
}

impl Agent {
    /// Starts an agent, and opens a conversation in it. It doesn't wait for
    /// that: an agent's MCP servers can take their time, and this is the UI
    /// thread. [`Said::Started`] says when it has.
    pub(crate) fn start(start: &Start<'_>) -> Result<Agent, String> {
        let Start { command, cwd, env, opening } = start;
        let (program, args) = command.split_first().ok_or("no agent to run")?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("can't start {program} in {cwd}: {e}"))?;
        let mut input = child.stdin.take().ok_or("the agent has no input")?;
        let output = child.stdout.take().ok_or("the agent has no output")?;
        send(&mut input, &initialize());
        let open = match opening {
            Opening::New => json!({ "jsonrpc": "2.0", "id": NEW, "method": "session/new", "params": { "cwd": cwd, "mcpServers": [] } }),
            Opening::Saved(session) => json!({ "jsonrpc": "2.0", "id": NEW, "method": "session/load", "params": { "sessionId": session, "cwd": cwd, "mcpServers": [] } }),
        };
        send(&mut input, &open);
        let (said_tx, said) = unbounded();
        let opening = opening.clone();
        std::thread::spawn(move || read(BufReader::new(output), &said_tx, &opening));
        Ok(Agent { child: Some(child), input: Some(input), said: Some(said), session: None, waiting: Vec::new(), opens: open })
    }

    /// The conversations `command` has saved, every project's, as many pages
    /// as there are. It runs for this and ends, and it blocks, so never on the
    /// UI thread.
    pub(crate) fn saved(command: &[String]) -> Result<Vec<Saved>, String> {
        let (program, args) = command.split_first().ok_or("no agent to run")?;
        let mut child =
            Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("can't start {program}: {e}"))?;
        let mut input = child.stdin.take().ok_or("the agent has no input")?;
        let output = child.stdout.take().ok_or("the agent has no output")?;
        send(&mut input, &initialize());
        send(&mut input, &list_after(NEW, None));
        let (answer_tx, answer) = channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(output).lines().map_while(Result::ok);
            for id in (NEW..).take(PAGES_MOST) {
                let found = lines.by_ref().filter_map(|line| serde_json::from_str::<Value>(&line).ok()).find(|message| message["id"] == id);
                let next = found.as_ref().and_then(|message| message["result"]["nextCursor"].as_str().map(str::to_owned));
                let last = next.is_none();
                if answer_tx.send(found).is_err() || last {
                    return;
                }
                send(&mut input, &list_after(id + 1, next.as_deref()));
            }
        });
        let mut saved = Vec::new();
        let came_to = loop {
            match answer.recv_timeout(LIST_WITHIN) {
                Ok(Some(message)) if message["error"].is_object() => break Err(message["error"]["message"].as_str().unwrap_or("it can't list them").to_owned()),
                Ok(Some(message)) => {
                    saved.extend(Saved::listed(&message["result"]));
                    if message["result"]["nextCursor"].is_null() {
                        break Ok(saved);
                    }
                }
                Ok(None) => break Err(format!("{program} ended without listing them")),
                // It always says there's more: what it listed is still so.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) if !saved.is_empty() => break Ok(saved),
                Err(_) => break Err(format!("{program} didn't list them in time")),
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        came_to
    }

    fn say(&mut self, message: &Value) {
        if let Some(input) = &mut self.input {
            send(input, message);
        }
    }

    /// The conversation has started: what was waiting for it goes.
    pub(crate) fn started(&mut self, session: String) {
        self.session = Some(session);
        let mut waiting = std::mem::take(&mut self.waiting).into_iter();
        if let Some((id, text, images)) = waiting.next() {
            self.prompt(id, &text, &images);
        }
        for (id, text, _) in waiting {
            self.steer(id, &text);
        }
    }

    /// Sends `text` as a prompt, with `images`.
    pub(crate) fn prompt(&mut self, id: u64, text: &str, images: &[Attached]) {
        let Some(session) = &self.session else {
            self.waiting.push((id, text.to_owned(), images.to_vec()));
            return;
        };
        let mut prompt = vec![json!({ "type": "text", "text": text })];
        prompt.extend(images.iter().map(|image| json!({ "type": "image", "data": image.data, "mimeType": image.mime, "_meta": { LABEL: image.label } })));
        let params = json!({ "sessionId": session, "prompt": prompt });
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params }));
    }

    /// Asks `method`, of this conversation, under the number `id`.
    fn ask_of_it(&mut self, id: u64, method: &str) -> bool {
        self.ask_with(id, method, json!({}))
    }

    /// The same, saying more.
    fn ask_with(&mut self, id: u64, method: &str, mut params: Value) -> bool {
        let Some(session) = &self.session else { return false };
        params["sessionId"] = json!(session);
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        true
    }

    /// Signs in the way `way` says, under the number `id`: there's no
    /// conversation to ask it of yet.
    pub(crate) fn sign_in(&mut self, id: u64, way: rein_transcript::signing::Way) {
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": SIGN_IN, "params": { "way": way.id() } }));
    }

    /// Asks the ways there are to sign in to more, under the number `id`.
    pub(crate) fn ask_ways(&mut self, id: u64) {
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": WAYS, "params": {} }));
    }

    /// Opens the conversation again, once you've signed in.
    pub(crate) fn open_again(&mut self) {
        let open = self.opens.clone();
        self.say(&open);
    }

    /// Asks what's in the background.
    pub(crate) fn ask_tasks(&mut self, id: u64) -> bool {
        self.ask_of_it(id, TASKS)
    }

    /// Stops the background command `task`.
    pub(crate) fn stop_task(&mut self, id: u64, task: &str) -> bool {
        self.ask_with(id, STOP_TASK, json!({ "id": task }))
    }

    /// Asks what's taking room.
    pub(crate) fn ask_room(&mut self, id: u64) -> bool {
        self.ask_of_it(id, ROOM)
    }

    /// Asks for your settings, as a settings page shows them.
    pub(crate) fn ask_config(&mut self, id: u64) -> bool {
        self.ask_of_it(id, CONFIG)
    }

    /// Sets `key` in your settings to `value`, or takes it out with `None`.
    pub(crate) fn set_config(&mut self, id: u64, key: &str, value: Option<&str>) -> bool {
        self.ask_with(id, SET_CONFIG, json!({ "key": key, "value": value }))
    }

    /// Adds `rule` to yours, or takes it out.
    pub(crate) fn rule(&mut self, id: u64, rule: &str, add: bool) -> bool {
        self.ask_with(id, RULE, json!({ "rule": rule, "add": add }))
    }

    /// Asks what `span` changed, with its `lines`, or only how much.
    pub(crate) fn ask_changes(&mut self, id: u64, span: rein_transcript::changes::Span, lines: bool) -> bool {
        self.ask_with(id, CHANGES, json!({ "span": span.id(), "lines": lines }))
    }

    /// Has the other providers' models looked up: they come with the options.
    pub(crate) fn ask_models(&mut self, id: u64) -> bool {
        self.ask_of_it(id, MODELS)
    }

    /// Asks which turns there are to undo.
    pub(crate) fn ask_undo_turns(&mut self, id: u64) -> bool {
        self.ask_of_it(id, UNDO_TURNS)
    }

    /// Asks what undoing back to `turn` does.
    pub(crate) fn ask_undo_plan(&mut self, id: u64, turn: usize) -> bool {
        self.ask_with(id, UNDO_PLAN, json!({ "turn": turn }))
    }

    /// Undoes back to before `turn`.
    pub(crate) fn undo(&mut self, id: u64, turn: usize) -> bool {
        self.ask_with(id, UNDO, json!({ "turn": turn }))
    }

    /// Asks what you sent from this folder before.
    pub(crate) fn ask_sent(&mut self, id: u64) -> bool {
        self.ask_of_it(id, SENT)
    }

    /// Asks what `@` can point at.
    pub(crate) fn ask_paths(&mut self, id: u64) -> bool {
        self.ask_of_it(id, PATHS)
    }

    /// Sends the command that's running to the background.
    pub(crate) fn background(&mut self, id: u64) -> bool {
        self.ask_of_it(id, BACKGROUND)
    }

    /// Sends `text` into the turn that's running.
    pub(crate) fn steer(&mut self, id: u64, text: &str) {
        let Some(session) = &self.session else {
            self.waiting.push((id, text.to_owned(), Vec::new()));
            return;
        };
        let params = json!({ "sessionId": session, "message": text });
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": "_io.github.furkancodes/steer", "params": params }));
    }

    /// Stops the turn that's running, or what's waiting to become one: a
    /// stop you pressed while the conversation opened still counts.
    pub(crate) fn stop(&mut self) -> Stopping {
        let Some(session) = &self.session else {
            self.waiting.clear();
            return Stopping::NeverBegan;
        };
        let params = json!({ "sessionId": session });
        self.say(&json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": params }));
        Stopping::Told
    }

    /// Puts the conversation in the mode `mode`, under the number `id`.
    pub(crate) fn set_mode(&mut self, id: u64, mode: &str) {
        let Some(session) = &self.session else { return };
        let params = json!({ "sessionId": session, "modeId": mode });
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": "session/set_mode", "params": params }));
    }

    /// Answers what was asked under `id`, with the option `chosen`. Only
    /// what's pressed is a yes.
    pub(crate) fn answer(&mut self, id: &Value, chosen: &str) {
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "selected", "optionId": chosen } } }));
    }

    /// Sets `option` to `value`, under the number `id`.
    pub(crate) fn set_option(&mut self, id: u64, option: &str, value: &str) -> bool {
        let Some(session) = &self.session else { return false };
        let params = json!({ "sessionId": session, "configId": option, "value": value });
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": "session/set_config_option", "params": params }));
        true
    }

    /// Answers what a command asked under `id`.
    pub(crate) fn reply(&mut self, id: &Value, result: &Value) {
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    /// Says the sub-agent asked for under `id` has started.
    pub(crate) fn opened(&mut self, id: &Value) {
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "result": {} }));
    }

    /// Says what was asked under `id` can't be done, and why.
    pub(crate) fn refuse(&mut self, id: &Value, why: &str) {
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": why } }));
    }
}

#[cfg(test)]
impl Agent {
    /// Closes it, and waits for it to end.
    pub(crate) fn finish(mut self) {
        if let Some(ending) = self.end() {
            ending.join().expect("what ends it doesn't panic");
        }
    }
}

/// Agents on their way out, for the window's last moments.
static ENDING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

impl Agent {
    /// Closes its input, and gives it time to end on a thread of its own: it
    /// runs its hooks and stops what it started as it goes, which can take
    /// seconds, and this is usually the UI thread. `None` once it's been done.
    fn end(&mut self) -> Option<JoinHandle<()>> {
        // Its input closing ends it, and it stops what it started as it goes.
        // Killed, it couldn't: a command runs in a group of its own, and a
        // dev server or `cargo test` would run on with nobody to stop it.
        self.input = None;
        let mut child = self.child.take()?;
        Some(std::thread::spawn(move || {
            let closed = Instant::now();
            while closed.elapsed() < TO_END {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        }))
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(ending) = self.end() {
            let mut all = ENDING.lock().unwrap_or_else(PoisonError::into_inner);
            all.retain(|one| !one.is_finished());
            all.push(ending);
        }
    }
}

/// Waits for the agents already closed to end. Only as the app goes: past
/// this, nothing would be left to end one that won't.
pub(crate) fn wait_for_those_ending() {
    let ending = std::mem::take(&mut *ENDING.lock().unwrap_or_else(PoisonError::into_inner));
    for one in ending {
        let _ = one.join();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    /// What an agent saying `lines` comes to.
    fn heard(lines: &[Value]) -> Vec<Said> {
        heard_opening(lines, &Opening::New)
    }

    fn heard_opening(lines: &[Value], opening: &Opening) -> Vec<Said> {
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (said, mut taken) = unbounded();
        read(text.as_bytes(), &said, opening);
        drop(said);
        std::iter::from_fn(|| taken.try_recv().ok()).collect()
    }

    /// An agent running `script` in a shell.
    fn sh(script: &str) -> Agent {
        let command = ["sh".to_owned(), "-c".to_owned(), script.to_owned()];
        Agent::start(&Start { command: &command, cwd: "/", env: &[], opening: Opening::New }).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-window-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn what_a_request_came_to_is_said() {
        let started = heard(&[json!({ "id": NEW, "result": { "sessionId": "abc" } })]);
        assert_eq!(started, [Said::Started("abc".to_owned()), Said::Ended]);
        let refused = heard(&[json!({ "id": NEW, "error": { "code": -32000, "message": "sign in first" } }), json!({ "id": 101, "result": { "stopReason": "end_turn" } })]);
        assert_eq!(refused, [Said::Failed("sign in first".to_owned()), Said::Ended], "a conversation that can't start says why, and that's the end of its agent");
        let answer = |line: Value| heard(&[line]).remove(0);
        let refused = answer(json!({ "id": 101, "error": { "code": -32000, "message": "the key was refused" } }));
        assert_eq!(refused, Said::Answer { id: 101, came_to: CameTo::Refused("the key was refused".to_owned()) });
        let busy = answer(json!({ "id": 101, "error": { "code": BUSY, "message": "a turn is running" } }));
        assert_eq!(busy, Said::Answer { id: 101, came_to: CameTo::Busy("a turn is running".to_owned()) });
        let ended = answer(json!({ "id": 101, "result": { "stopReason": "end_turn" } }));
        assert_eq!(ended, Said::Answer { id: 101, came_to: CameTo::TurnOver });
        let steered = answer(json!({ "id": 102, "result": {} }));
        assert_eq!(steered, Said::Answer { id: 102, came_to: CameTo::Done }, "a steer is answered too, and the turn goes on");
        assert_eq!(heard(&[json!({ "id": 1, "result": {} })]), [Said::Ended], "what started it isn't anything to act on");
    }

    #[test]
    fn a_question_names_its_step() {
        let options = json!([{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }]);
        let asked = heard(&[
            json!({ "id": 7, "method": "session/request_permission", "params": { "toolCall": { "toolCallId": "call_1" }, "options": options, "_meta": { "io.github.furkancodes/question": "Allow this edit?", "io.github.furkancodes/why": "Why: it must" } } }),
        ]);
        assert_eq!(
            asked,
            [
                Said::Asked { id: json!(7), call: "call_1".to_owned(), options, words: Words { asks: Some("Allow this edit?".to_owned()), why: Some("Why: it must".to_owned()) } },
                Said::Ended
            ]
        );
    }

    #[test]
    fn signed_out_it_says_the_ways_and_waits_for_you_to_sign_in() {
        let refused = json!({ "id": NEW, "error": { "code": rein_transcript::signing::SIGNED_OUT, "message": "signed out", "data": { "ways": [{ "way": "local", "providers": ["Ollama"] }] } } });
        let heard = heard(&[refused, json!({ "id": NEW, "result": { "sessionId": "abc" } })]);
        let Said::SignedOut(ways) = &heard[0] else { panic!("{heard:?}") };
        assert_eq!(ways.iter().map(|way| way.way).collect::<Vec<_>>(), [rein_transcript::signing::Way::Local]);
        assert_eq!(heard[1..], [Said::Started("abc".to_owned()), Said::Ended], "it goes on reading, for the conversation once you've signed in");
    }

    #[test]
    fn a_saved_conversation_goes_by_the_id_it_has() {
        let opening = Opening::Saved("kept".to_owned());
        assert_eq!(heard_opening(&[json!({ "id": NEW, "result": { "modes": {} } })], &opening), [Said::Started("kept".to_owned()), Said::Ended]);
        let refused = heard_opening(&[json!({ "id": NEW, "error": { "message": "there's no saved conversation kept" } })], &opening);
        assert_eq!(refused, [Said::Failed("there's no saved conversation kept".to_owned()), Said::Ended]);
    }

    #[test]
    fn a_turn_nobody_prompted_is_said_to_start_and_end() {
        let lines = [
            json!({ "method": TURN_STARTED, "params": {} }),
            json!({ "method": TURN_ENDED, "params": { "stopReason": "end_turn" } }),
            json!({ "method": TURN_ENDED, "params": { "error": "the key was refused" } }),
        ];
        assert_eq!(heard(&lines), [Said::TurnStarted, Said::TurnOver, Said::Failed("the key was refused".to_owned()), Said::Ended]);
    }

    #[test]
    fn a_sub_agent_is_what_rein_said_to_run_and_what_cant_be_read_is_no_sub_agent() {
        let params = json!({ "name": "tests", "command": "rein", "args": ["serve", "--child", "1:abc"], "cwd": "/work", "env": [{ "name": "REIN_HOME", "value": "/home" }] });
        let asked = heard(&[json!({ "id": 9, "method": OPEN_PANE, "params": params })]);
        let wanted = SubAgent {
            name: "tests".to_owned(),
            command: ["rein", "serve", "--child", "1:abc"].map(str::to_owned).to_vec(),
            cwd: "/work".to_owned(),
            env: vec![("REIN_HOME".to_owned(), "/home".to_owned())],
        };
        assert_eq!(asked, [Said::SubAgent { id: json!(9), asked: Some(wanted) }, Said::Ended]);
        for broken in [json!({ "name": "tests" }), json!({ "name": "tests", "command": "rein", "args": [1], "cwd": "/", "env": [] })] {
            let refused = heard(&[json!({ "id": 9, "method": OPEN_PANE, "params": broken })]);
            assert_eq!(refused, [Said::SubAgent { id: json!(9), asked: None }, Said::Ended]);
        }
    }

    #[test]
    fn it_says_it_opens_panes_and_opens_what_its_told_where_its_told() {
        let dir = scratch("opens");
        let (file, whats_run) = (dir.join("got"), dir.join("ran"));
        let script = format!("pwd > {}; echo \"$CARRIED\" >> {}; cat > {}", whats_run.display(), whats_run.display(), file.display());
        let command = ["sh".to_owned(), "-c".to_owned(), script];
        let env = [("CARRIED".to_owned(), "over".to_owned())];
        let cwd = dir.canonicalize().unwrap().display().to_string();
        let agent = Agent::start(&Start { command: &command, cwd: &cwd, env: &env, opening: Opening::Saved("kept".to_owned()) }).unwrap();
        agent.finish();
        let got: Vec<Value> = std::fs::read_to_string(&file).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(got[0]["params"]["clientCapabilities"]["_meta"][PANES], true, "or a sub-agent has nowhere to run");
        assert_eq!((&got[1]["method"], &got[1]["params"]["sessionId"]), (&json!("session/load"), &json!("kept")));
        assert_eq!(std::fs::read_to_string(&whats_run).unwrap(), format!("{cwd}\nover\n"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stopping_and_answering_go_as_acp_says() {
        let dir = scratch("stops-a-turn");
        let file = dir.join("got");
        let mut agent = sh(&format!("cat > {}", file.display()));
        assert_eq!(agent.stop(), Stopping::NeverBegan);
        agent.started("abc".to_owned());
        assert_eq!(agent.stop(), Stopping::Told);
        agent.answer(&json!(7), "always");
        agent.opened(&json!(8));
        agent.refuse(&json!(9), "can't");
        agent.finish();
        let got: Vec<Value> = std::fs::read_to_string(&file).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(got.len(), 6, "a stop before there's a conversation has nothing to stop: {got:?}");
        assert_eq!((got[2]["method"].as_str(), got[2].get("id")), (Some("session/cancel"), None), "a notification, with no answer to wait for");
        assert_eq!(got[3]["result"]["outcome"], json!({ "outcome": "selected", "optionId": "always" }));
        assert_eq!((&got[4]["id"], &got[4]["result"]), (&json!(8), &json!({})));
        assert_eq!(got[5]["error"]["message"], "can't");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_saved_list_goes_on_for_as_many_pages_as_there_are() {
        let lists = r#"read a; read b; printf '%s\n' '{"id":1,"result":{}}' '{"id":2,"result":{"sessions":[{"sessionId":"new","cwd":"/a"}],"nextCursor":"100"}}'; read c; case "$c" in *'"cursor":"100"'*) printf '%s\n' '{"id":3,"result":{"sessions":[{"sessionId":"old","cwd":"/b"}]}}';; esac"#;
        let saved = Agent::saved(&["sh".to_owned(), "-c".to_owned(), lists.to_owned()]).unwrap();
        assert_eq!(saved.iter().map(|saved| saved.id.as_str()).collect::<Vec<_>>(), ["new", "old"], "past the first 100 is still yours");
    }

    #[test]
    fn the_saved_list_comes_from_an_agent_run_for_it() {
        let lists = r#"read a; read b; printf '%s\n' '{"id":1,"result":{}}' '{"id":2,"result":{"sessions":[{"sessionId":"abc","cwd":"/work"}]}}'"#;
        let command = ["sh".to_owned(), "-c".to_owned(), lists.to_owned()];
        let saved = Agent::saved(&command).unwrap();
        assert_eq!(saved.iter().map(|saved| saved.id.as_str()).collect::<Vec<_>>(), ["abc"]);
        let refuses = r#"read a; read b; printf '%s\n' '{"id":2,"error":{"message":"no"}}'"#;
        assert_eq!(Agent::saved(&["sh".to_owned(), "-c".to_owned(), refuses.to_owned()]), Err("no".to_owned()));
        assert!(Agent::saved(&["true".to_owned()]).is_err(), "one that says nothing lists nothing");
    }

    #[test]
    fn what_you_write_before_a_conversation_has_started_waits_for_it() {
        let dir = scratch("waits");
        let file = dir.join("got");
        let mut agent = sh(&format!("cat > {}", file.display()));
        agent.prompt(101, "first", &[]);
        agent.steer(102, "second");
        agent.started("abc".to_owned());
        agent.steer(103, "third");
        agent.finish();
        let got = std::fs::read_to_string(&file).unwrap();
        let methods: Vec<Value> = got.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["method"].clone()).collect();
        assert_eq!(methods, ["initialize", "session/new", "session/prompt", "_io.github.furkancodes/steer", "_io.github.furkancodes/steer"]);
        assert!(got.contains("\"first\"") && got.contains("\"second\"") && got.contains("\"third\""), "{got}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stop_before_a_conversation_has_started_drops_what_was_waiting_for_it() {
        let dir = scratch("stopped-early");
        let file = dir.join("got");
        let mut agent = sh(&format!("cat > {}", file.display()));
        agent.prompt(101, "delete the lot", &[]);
        agent.steer(102, "and quickly");
        assert_eq!(agent.stop(), Stopping::NeverBegan);
        agent.started("abc".to_owned());
        agent.finish();
        let got = std::fs::read_to_string(&file).unwrap();
        let methods: Vec<Value> = got.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["method"].clone()).collect();
        assert_eq!(methods, ["initialize", "session/new"], "you pressed Stop, so it never goes");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_agent_gets_to_stop_what_it_started_when_its_pane_goes() {
        let dir = scratch("stops");
        let done = dir.join("done");
        let agent = sh(&format!("cat > /dev/null; touch {}", done.display()));
        agent.finish();
        assert!(done.exists(), "killed outright, a command in a group of its own runs on");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_agent_that_wont_end_is_ended_with_the_window_left_alone() {
        let agent = Agent::start(&Start { command: &["sleep".to_owned(), "60".to_owned()], cwd: "/", env: &[], opening: Opening::New }).unwrap();
        let (pid, started) = (agent.child.as_ref().unwrap().id(), Instant::now());
        drop(agent);
        assert!(started.elapsed() < Duration::from_millis(500), "a pane closes on the UI thread: {:?}", started.elapsed());
        wait_for_those_ending();
        assert!(started.elapsed() < TO_END + Duration::from_secs(2), "{:?}", started.elapsed());
        let alive = Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().unwrap().success();
        assert!(!alive, "it's still running");
    }
}
