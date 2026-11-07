//! An agent behind a pane: a process speaking the Agent Client Protocol.
//! Its output is read on a thread of its own, so a loud agent never holds
//! up drawing, and what it says waits in a queue for the window to take.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
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
    /// It asks, under this number, whether step `call` may run. `rein serve`
    /// says which step and no more: what it does was said as it was shown.
    Asked {
        id: Value,
        call: String,
    },
    /// The conversation has started, under this id.
    Started(String),
    /// A turn came to an end.
    TurnOver,
    /// What was asked of it failed, and this is why.
    Failed(String),
    Ended,
}

pub(crate) struct Agent {
    child: Child,
    /// Closing it is what ends the agent, so a drop takes it first.
    input: Option<ChildStdin>,
    /// Taken by whoever listens, which then wakes only when there's news.
    pub(crate) said: Option<UnboundedReceiver<Said>>,
    session: Option<String>,
    /// What was written before the conversation had started: the first is
    /// a prompt, and the rest steer it.
    waiting: Vec<(u64, String)>,
}

const NEW: u64 = 2;
/// How long an agent gets to stop what it started, once its input closes.
const TO_END: Duration = Duration::from_secs(3);

fn send(input: &mut impl Write, message: &Value) {
    // Gone only once the agent has: its output's end says so.
    let _ = writeln!(input, "{message}").and_then(|()| input.flush());
}

/// What an answer to a request comes to, if it's anything to act on. What
/// ends a prompt says why it ended, or that it failed: a steer is answered
/// too, and the turn goes on.
fn answered(message: &Value) -> Option<Said> {
    let id = message["id"].as_u64()?;
    if message["error"].is_object() {
        let why = message["error"]["message"].as_str().unwrap_or("That failed.");
        return (id >= NEW).then(|| Said::Failed(why.to_owned()));
    }
    if id == NEW {
        return message["result"]["sessionId"].as_str().map(|session| Said::Started(session.to_owned()));
    }
    (id > NEW && message["result"]["stopReason"].is_string()).then_some(Said::TurnOver)
}

fn read(output: impl BufRead, said: &UnboundedSender<Said>) {
    for line in output.lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let params = &message["params"];
        let one = match message["method"].as_str() {
            Some("session/update") => Some(Said::Update(params["update"].clone())),
            Some("session/request_permission") => Some(Said::Asked { id: message["id"].clone(), call: params["toolCall"]["toolCallId"].as_str().unwrap_or_default().to_owned() }),
            Some("_io.github.furkancodes/heard") => {
                Some(Said::Heard { from: params["from"].as_str().unwrap_or("?").to_owned(), text: params["text"].as_str().unwrap_or_default().to_owned() })
            }
            Some(_) => None,
            None => answered(&message),
        };
        if let Some(one) = one
            && said.unbounded_send(one).is_err()
        {
            return;
        }
    }
    let _ = said.unbounded_send(Said::Ended);
}

impl Agent {
    /// Starts `command`, with a conversation in `cwd`. It doesn't wait for
    /// the conversation: an agent's MCP servers can take their time, and
    /// this is the UI thread. [`Said::Started`] says when it has.
    pub(crate) fn start(command: &[String], cwd: &str) -> Result<Agent, String> {
        let (program, args) = command.split_first().ok_or("no agent to run")?;
        let mut child =
            Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("can't start {program}: {e}"))?;
        let mut input = child.stdin.take().ok_or("the agent has no input")?;
        let output = child.stdout.take().ok_or("the agent has no output")?;
        send(&mut input, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": 1, "clientCapabilities": {} } }));
        send(&mut input, &json!({ "jsonrpc": "2.0", "id": NEW, "method": "session/new", "params": { "cwd": cwd, "mcpServers": [] } }));
        let (said_tx, said) = unbounded();
        std::thread::spawn(move || read(BufReader::new(output), &said_tx));
        Ok(Agent { child, input: Some(input), said: Some(said), session: None, waiting: Vec::new() })
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
        if let Some((id, text)) = waiting.next() {
            self.prompt(id, &text);
        }
        for (id, text) in waiting {
            self.steer(id, &text);
        }
    }

    /// Sends `text` as a prompt.
    pub(crate) fn prompt(&mut self, id: u64, text: &str) {
        let Some(session) = &self.session else {
            self.waiting.push((id, text.to_owned()));
            return;
        };
        let params = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": params }));
    }

    /// Sends `text` into the turn that's running.
    pub(crate) fn steer(&mut self, id: u64, text: &str) {
        let Some(session) = &self.session else {
            self.waiting.push((id, text.to_owned()));
            return;
        };
        let params = json!({ "sessionId": session, "message": text });
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "method": "_io.github.furkancodes/steer", "params": params }));
    }

    /// Answers what was asked under `id`. Only what's pressed is a yes.
    pub(crate) fn answer(&mut self, id: &Value, allow: bool) {
        let option = if allow { "allow" } else { "reject" };
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": "selected", "optionId": option } } }));
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // Its input closing ends it, and it stops what it started as it goes.
        // Killed, it couldn't: a command runs in a group of its own, and a
        // dev server or `cargo test` would run on with nobody to stop it.
        self.input = None;
        let closed = Instant::now();
        while closed.elapsed() < TO_END {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        let (said, mut taken) = unbounded();
        read(text.as_bytes(), &said);
        drop(said);
        std::iter::from_fn(|| taken.try_recv().ok()).collect()
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
        let refused = heard(&[json!({ "id": NEW, "error": { "code": -32000, "message": "sign in first" } })]);
        assert_eq!(refused, [Said::Failed("sign in first".to_owned()), Said::Ended], "a conversation that can't start says why");
        let failed = heard(&[json!({ "id": 101, "error": { "code": -32000, "message": "the key was refused" } })]);
        assert_eq!(failed, [Said::Failed("the key was refused".to_owned()), Said::Ended], "and so does a turn");
        let ended = heard(&[json!({ "id": 101, "result": { "stopReason": "end_turn" } })]);
        assert_eq!(ended, [Said::TurnOver, Said::Ended]);
        let steered = heard(&[json!({ "id": 102, "result": {} })]);
        assert_eq!(steered, [Said::Ended], "a steer is answered too, and the turn goes on");
    }

    #[test]
    fn a_question_names_its_step() {
        let asked = heard(&[json!({ "id": 7, "method": "session/request_permission", "params": { "toolCall": { "toolCallId": "call_1" } } })]);
        assert_eq!(asked, [Said::Asked { id: json!(7), call: "call_1".to_owned() }, Said::Ended]);
    }

    #[test]
    fn what_you_write_before_a_conversation_has_started_waits_for_it() {
        let dir = scratch("waits");
        let file = dir.join("got");
        let mut agent = Agent::start(&["sh".to_owned(), "-c".to_owned(), format!("cat > {}", file.display())], "/").unwrap();
        agent.prompt(101, "first");
        agent.steer(102, "second");
        agent.started("abc".to_owned());
        agent.steer(103, "third");
        drop(agent);
        let got = std::fs::read_to_string(&file).unwrap();
        let methods: Vec<Value> = got.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["method"].clone()).collect();
        assert_eq!(methods, ["initialize", "session/new", "session/prompt", "_io.github.furkancodes/steer", "_io.github.furkancodes/steer"]);
        assert!(got.contains("\"first\"") && got.contains("\"second\"") && got.contains("\"third\""), "{got}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_agent_gets_to_stop_what_it_started_when_its_pane_goes() {
        let dir = scratch("stops");
        let done = dir.join("done");
        let agent = Agent::start(&["sh".to_owned(), "-c".to_owned(), format!("cat > /dev/null; touch {}", done.display())], "/").unwrap();
        drop(agent);
        assert!(done.exists(), "killed outright, a command in a group of its own runs on");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_agent_that_wont_end_is_ended() {
        let agent = Agent::start(&["sleep".to_owned(), "60".to_owned()], "/").unwrap();
        let (pid, started) = (agent.child.id(), Instant::now());
        drop(agent);
        assert!(started.elapsed() < TO_END + Duration::from_secs(2), "{:?}", started.elapsed());
        let alive = Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().unwrap().success();
        assert!(!alive, "it's still running");
    }
}
