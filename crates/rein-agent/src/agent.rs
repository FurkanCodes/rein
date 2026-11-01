//! The agent loop: ask the model, run the tools it asks for, send back what
//! they produced, and repeat until it answers without asking for anything.

use std::fs::File;
use std::future::Future;

use rein_llm::openai::{Client, Request};
use rein_llm::{Error, Event, Item, Result, Tool, ToolCall, Usage};

use crate::compact;
use crate::context;
use crate::permissions::{Decision, Permissions};
use crate::session::Session;
use crate::tools::Tools;

/// After this many model turns in a row, stop and give the user control back.
pub const MAX_TURNS: usize = 25;

/// What streams in while the model answers.
pub enum Streamed<'a> {
    /// A piece of the answer.
    Text(&'a str),
    /// A piece of a short summary of what the model is thinking.
    Thought(&'a str),
}

/// Something that answers requests. The real one is OpenAI; tests script one.
pub trait Model {
    /// Sends one request, passing what streams in to `on` as it arrives,
    /// and returns the finished items and the token counts.
    fn respond(&self, request: &Request, on: &mut dyn FnMut(Streamed)) -> impl Future<Output = Result<(Vec<Item>, Usage)>>;
}

/// The answer to "may this run?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Yes,
    /// Yes, and don't ask again this session: for this exact command, or
    /// for all file changes.
    Always,
    No,
    /// No, and stop the turn: the user wants to say what to do instead.
    Stop,
}

/// Whoever is at the keyboard: sees what happens, and decides what may run.
pub trait Ui {
    fn text(&mut self, delta: &str);
    /// A piece of a summary of the model's thinking, like `**Checking the
    /// tests**`.
    fn thought(&mut self, _delta: &str) {}
    /// Asked when the permissions say to ask. It's async so that an
    /// interrupt can cancel the turn even while it waits for an answer.
    fn approve(&mut self, call: &ToolCall) -> impl Future<Output = Approval>;
    fn tool_started(&mut self, call: &ToolCall);
    /// A call the permissions refused without asking, and why.
    fn tool_blocked(&mut self, call: &ToolCall, reason: &str);
    fn tool_finished(&mut self, call: &ToolCall, output: &str);
    fn usage(&mut self, usage: &Usage);
    /// A request is on its way to the model; nothing to show yet.
    fn thinking(&mut self) {}
    /// Compaction is starting. A summary, if one is written, streams
    /// through `text`.
    fn compacting(&mut self) {}
    fn compacted(&mut self, _report: &Compacted) {}
}

/// What a compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// About how many tokens the conversation took, before and after.
    pub before: u64,
    pub after: u64,
    /// How many stale tool outputs were retired.
    pub retired: usize,
    /// The model wrote a summary; otherwise retiring was enough.
    pub summarized: bool,
}

/// How full the conversation is, and what compaction needs to know.
pub struct Window {
    /// Compact once a request would carry more than this many tokens.
    pub limit: u64,
    /// The provider's token count for the last request, and how many
    /// history items that request carried.
    pub measured: Option<(u64, usize)>,
    /// Everything said so far, compacted away or not: rein's record and
    /// `recall` work from it. It's never sent as is.
    pub full: Vec<Item>,
}

impl Default for Window {
    fn default() -> Self {
        Window { limit: compact::DEFAULT_LIMIT, measured: None, full: Vec::new() }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The model answered without asking for a tool.
    Done,
    /// It kept asking for tools; we stopped after `MAX_TURNS`.
    TurnLimit,
    /// The user declined a step and stopped the turn, to say what to do
    /// instead.
    Stopped,
}

pub struct Agent<M> {
    pub model: M,
    pub tools: Tools,
    /// What may run without asking.
    pub permissions: Permissions,
    pub model_name: String,
    pub instructions: Option<String>,
    pub reasoning_effort: Option<String>,
    /// One ID for the whole conversation, so its requests share a prompt cache.
    pub session_id: Option<String>,
    /// The whole conversation. Every request sends all of it.
    pub history: Vec<Item>,
    /// Where the conversation is saved as it grows, if anywhere.
    pub session: Option<Session>,
    /// What the model is told before the first message (see
    /// [`crate::context`]); it goes in when the first message does.
    pub context: Option<String>,
    pub window: Window,
}

impl<M: Model> Agent<M> {
    /// Adds the user's message and works on it until the model is done.
    /// Dropping the future cancels the turn; [`Agent::repair`] then makes
    /// the history valid again, and `send` calls it first anyway.
    pub async fn send(&mut self, prompt: String, ui: &mut impl Ui) -> Result<Outcome> {
        self.repair();
        self.tools.checkpoint();
        if let Some(context) = self.context.take() {
            if let Some(session) = &mut self.session {
                session.append_context(&context)?;
            }
            self.window.full.push(Item::User(context.clone()));
            self.history.push(Item::User(context));
        }
        self.record(Item::User(prompt))?;
        // Compact at most once per message, so a summary that is itself too
        // big can't start a loop of summaries.
        let mut compacted = false;
        for _ in 0..MAX_TURNS {
            if !compacted && self.estimated() > self.window.limit {
                self.compact(None, false, ui).await?;
                compacted = true;
            }
            let request = self.request(self.history.clone());
            let sent = self.history.len();
            ui.thinking();
            let (items, usage) = match self.model.respond(&request, &mut |streamed| stream_to(ui, streamed)).await {
                // The provider says it's too long after all: make room and
                // try again, once.
                Err(e) if !compacted && e.code() == Some("context_length_exceeded") => {
                    self.compact(None, true, ui).await?;
                    compacted = true;
                    continue;
                }
                other => other?,
            };
            ui.usage(&usage);
            self.window.measured = Some((usage.input_tokens, sent));

            let calls: Vec<ToolCall> = items
                .iter()
                .filter_map(|item| match item {
                    Item::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect();
            // Everything the model produced goes into the history, in order:
            // text, encrypted reasoning, and the calls themselves.
            for item in items {
                self.record(item)?;
            }
            if calls.is_empty() {
                return Ok(Outcome::Done);
            }

            for call in calls {
                // Searching the conversation is rein's own business: no
                // permission needed, nothing outside is touched.
                if call.name == "recall" {
                    ui.tool_started(&call);
                    let output = compact::recall(&self.window.full, &call.arguments);
                    ui.tool_finished(&call, &output);
                    self.record(Item::ToolResult { call_id: call.call_id, output })?;
                    continue;
                }
                let approved = match self.permissions.decide(&call) {
                    Decision::Allow => Ok(()),
                    Decision::Deny(reason) => {
                        ui.tool_blocked(&call, &reason);
                        Err(reason)
                    }
                    Decision::Ask => match ui.approve(&call).await {
                        Approval::Yes => Ok(()),
                        Approval::Always => {
                            self.permissions.remember(&call);
                            Ok(())
                        }
                        Approval::No => Err(match call.name.as_str() {
                            "bash" => "The user declined to run this command.".to_owned(),
                            _ => "The user declined this change.".to_owned(),
                        }),
                        Approval::Stop => {
                            let output = "The user declined this and stopped, to tell you what to do instead.".to_owned();
                            ui.tool_finished(&call, &output);
                            self.record(Item::ToolResult { call_id: call.call_id, output })?;
                            return Ok(Outcome::Stopped);
                        }
                    },
                };
                let output = match approved {
                    Ok(()) => {
                        ui.tool_started(&call);
                        self.tools.run(&call).await
                    }
                    Err(reason) => reason,
                };
                ui.tool_finished(&call, &output);
                self.record(Item::ToolResult { call_id: call.call_id, output })?;
            }
        }
        Ok(Outcome::TurnLimit)
    }

    /// Makes room in the conversation. If retiring stale tool output is
    /// enough to get well under the limit, and `summarize` is false, that's
    /// all. Otherwise the model writes a summary, and the conversation
    /// becomes the first message, the summary with rein's own record, and
    /// its latest part word for word, with stale output there retired.
    /// `focus` is what the user asked the summary to keep in mind
    /// (`/compact <focus>`).
    pub async fn compact(&mut self, focus: Option<&str>, summarize: bool, ui: &mut impl Ui) -> Result<Compacted> {
        ui.compacting();
        let before = self.estimated();
        let retired = compact::stale(&self.history);
        let freed: u64 = retired
            .iter()
            .map(|(id, text)| {
                let old = self.history.iter().find_map(|item| match item {
                    Item::ToolResult { call_id, output } if call_id == id => Some(output.len()),
                    _ => None,
                });
                compact::estimate(old.unwrap_or(0).saturating_sub(text.len()))
            })
            .sum();
        if !summarize && before.saturating_sub(freed) <= self.window.limit * 6 / 10 {
            if let Some(session) = &mut self.session {
                session.append_retired(&retired)?;
            }
            compact::retire(&mut self.history, &retired);
            self.window.measured = None;
            let report = Compacted { before, after: self.estimated(), retired: retired.len(), summarized: false };
            ui.compacted(&report);
            return Ok(report);
        }

        // The conversation as it is, untouched, plus the request for a
        // summary: it starts exactly like the last request, so the cache
        // serves most of it. (Retiring first would change it early on and
        // lose the cache from there.)
        let mut input = self.history.clone();
        input.push(Item::User(compact::prompt(focus)));
        ui.thinking();
        let (items, usage) = self.model.respond(&self.request(input), &mut |streamed| stream_to(ui, streamed)).await?;
        ui.usage(&usage);
        let summary: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                Item::Assistant(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if summary.iter().all(|text| text.trim().is_empty()) {
            return Err(Error::Incomplete("the model wrote no summary".into()));
        }
        let message = compact::summary_message(&summary.join("\n"), &compact::record(&self.window.full));

        let pinned = usize::from(self.history.first().is_some_and(context::is_context));
        // Keep less word for word when the limit is small (as in tests).
        let budget = compact::KEEP_RECENT.min(self.window.limit / 5);
        let start = compact::recent_start(&self.history, pinned, budget);
        let mut kept = self.history[start..].to_vec();
        compact::retire(&mut kept, &retired);
        if let Some(session) = &mut self.session {
            session.append_compaction(&message, &kept)?;
        }
        self.history.truncate(pinned);
        self.history.push(Item::User(message));
        self.history.extend(kept);
        self.window.measured = None;
        let report = Compacted { before, after: self.estimated(), retired: retired.len(), summarized: true };
        ui.compacted(&report);
        Ok(report)
    }
}

impl<M> Agent<M> {
    /// The tools offered. `recall` is there from the start, though it only
    /// matters after a compaction: the tool list comes before everything
    /// else in a request, so changing it would lose the whole cache.
    fn specs(&self) -> Vec<Tool> {
        let mut specs = self.tools.specs();
        specs.push(compact::recall_spec());
        specs
    }

    fn request(&self, input: Vec<Item>) -> Request {
        Request {
            model: self.model_name.clone(),
            instructions: self.instructions.clone(),
            input,
            tools: self.specs(),
            reasoning_effort: self.reasoning_effort.clone(),
            reasoning_summary: true,
            session_id: self.session_id.clone(),
        }
    }

    /// About how many tokens the next request will carry: the provider's
    /// count for the last one, plus an estimate for what was added since.
    /// Without a count, all of it is estimated.
    pub fn estimated(&self) -> u64 {
        let added = |items: &[Item]| items.iter().map(|item| compact::estimate(compact::size(item))).sum::<u64>();
        match self.window.measured {
            Some((tokens, sent)) if sent <= self.history.len() => tokens + added(&self.history[sent..]),
            _ => {
                let tools: usize = self.specs().iter().map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len()).sum();
                let instructions = self.instructions.as_deref().map_or(0, str::len);
                compact::estimate(tools + instructions) + added(&self.history)
            }
        }
    }

    /// Adds an item to the history, saving it first. What [`Agent::repair`]
    /// fills in isn't saved: loading and repairing again gives the same.
    fn record(&mut self, item: Item) -> Result<()> {
        if let Some(session) = &mut self.session {
            session.append(&item)?;
        }
        self.window.full.push(item.clone());
        self.history.push(item);
        Ok(())
    }

    /// Undoes the file changes of the latest turn that made any, and tells
    /// the model, so it doesn't go on as if they were still there. Returns
    /// the files put back.
    pub fn undo(&mut self) -> Result<Vec<String>> {
        let undone = self.tools.undo().map_err(std::io::Error::other)?;
        if !undone.is_empty() {
            self.record(Item::User(format!(
                "[rein: the user undid your file changes from your last turn: {}. Those files are back as they were \
                 before it. Commands you ran then were not undone.]",
                undone.join(", ")
            )))?;
        }
        Ok(undone)
    }

    /// Gives every tool call that never got a result one now. A turn cut
    /// short (by Ctrl-C, say) can leave a call unanswered, and providers
    /// reject a conversation like that. Codex and pi do the same repair.
    pub fn repair(&mut self) {
        let answered: Vec<String> = self
            .history
            .iter()
            .filter_map(|item| match item {
                Item::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        let mut repaired = Vec::with_capacity(self.history.len());
        for item in std::mem::take(&mut self.history) {
            let orphan = match &item {
                Item::ToolCall(call) if !answered.contains(&call.call_id) => Some(call.call_id.clone()),
                _ => None,
            };
            repaired.push(item);
            if let Some(call_id) = orphan {
                let output = "The user interrupted this before it finished. It may have partly run.".to_owned();
                repaired.push(Item::ToolResult { call_id, output });
            }
        }
        self.history = repaired;
    }
}

/// OpenAI as a [`Model`], optionally saving every raw response stream to
/// `record` for replay tests.
pub struct OpenAi {
    pub client: Client,
    pub record: Option<File>,
}

fn stream_to(ui: &mut impl Ui, streamed: Streamed) {
    match streamed {
        Streamed::Text(text) => ui.text(text),
        Streamed::Thought(text) => ui.thought(text),
    }
}

/// How long to wait before each retry of a failed request.
const RETRY_WAITS: [u64; 3] = [1, 2, 4];

impl Model for OpenAi {
    async fn respond(&self, request: &Request, on: &mut dyn FnMut(Streamed)) -> Result<(Vec<Item>, Usage)> {
        // Only the request itself is retried: once the answer has started,
        // trying again would show it twice.
        let mut waits = RETRY_WAITS.iter();
        let mut stream = loop {
            match self.client.stream(request).await {
                Err(e) if e.retryable() && let Some(&seconds) = waits.next() => {
                    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                }
                result => break result?,
            }
        };
        if let Some(file) = &self.record {
            stream.record_to(file.try_clone()?);
        }
        let mut items = Vec::new();
        while let Some(event) = stream.next().await {
            match event? {
                Event::TextDelta(text) => on(Streamed::Text(&text)),
                Event::Thought(text) => on(Streamed::Thought(&text)),
                Event::Item(item) => items.push(item),
                Event::Completed { usage } => return Ok((items, usage)),
            }
        }
        // The stream only ends after `Completed` or an error, both handled above.
        Err(Error::Interrupted)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use serde_json::json;

    use super::*;
    use crate::permissions::Mode;

    /// Plays back scripted turns and remembers every request it was sent.
    struct Scripted {
        turns: RefCell<VecDeque<Vec<Item>>>,
        requests: RefCell<Vec<Request>>,
    }

    impl Scripted {
        fn new(turns: Vec<Vec<Item>>) -> Self {
            Scripted { turns: RefCell::new(turns.into()), requests: RefCell::new(Vec::new()) }
        }
    }

    impl Model for Scripted {
        async fn respond(&self, request: &Request, on: &mut dyn FnMut(Streamed)) -> Result<(Vec<Item>, Usage)> {
            self.requests.borrow_mut().push(request.clone());
            let items = self.turns.borrow_mut().pop_front().expect("the loop asked for more turns than scripted");
            for item in &items {
                if let Item::Assistant(text) = item {
                    on(Streamed::Text(text));
                }
            }
            Ok((items, Usage::default()))
        }
    }

    /// Gives the same answer every time and keeps a log of what it saw.
    struct Person {
        answer: Approval,
        log: Vec<String>,
    }

    impl Ui for Person {
        fn text(&mut self, delta: &str) {
            self.log.push(format!("text {delta}"));
        }
        async fn approve(&mut self, call: &ToolCall) -> Approval {
            self.log.push(format!("asked {}", call.arguments));
            self.answer
        }
        fn tool_started(&mut self, call: &ToolCall) {
            self.log.push(format!("ran {}", call.name));
        }
        fn tool_blocked(&mut self, call: &ToolCall, reason: &str) {
            self.log.push(format!("blocked {}: {reason}", call.name));
        }
        fn tool_finished(&mut self, _: &ToolCall, output: &str) {
            self.log.push(format!("result {output}"));
        }
        fn usage(&mut self, _: &Usage) {}
    }

    fn bash(id: &str, command: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "bash".into(), arguments: json!({ "command": command }).to_string() })
    }

    fn agent(dir: &str, turns: Vec<Vec<Item>>) -> Agent<Scripted> {
        let root = std::env::temp_dir().join(format!("rein-agent-{}-{dir}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Agent {
            model: Scripted::new(turns),
            tools: Tools::new(root),
            permissions: Permissions::default(),
            model_name: "m".into(),
            instructions: None,
            reasoning_effort: None,
            session_id: Some("session-1".into()),
            history: Vec::new(),
            session: None,
            context: None,
            window: Window::default(),
        }
    }

    #[tokio::test]
    async fn runs_approved_tools_and_sends_the_results_back() {
        let mut agent = agent(
            "loop",
            vec![
                vec![Item::Opaque(json!({"type": "reasoning"})), bash("c1", "echo hi")],
                vec![Item::Assistant("It printed hi.".into())],
            ],
        );
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("say hi".into(), &mut person).await.unwrap(), Outcome::Done);

        assert_eq!(person.log, [r#"asked {"command":"echo hi"}"#, "ran bash", "result hi\n[exit code 0]", "text It printed hi."]);
        // The second request carried everything: the prompt, the reasoning,
        // the call, and its result.
        let requests = agent.model.requests.borrow();
        assert_eq!(
            requests[1].input,
            [
                Item::User("say hi".into()),
                Item::Opaque(json!({"type": "reasoning"})),
                bash("c1", "echo hi"),
                Item::ToolResult { call_id: "c1".into(), output: "hi\n[exit code 0]".into() },
            ]
        );
        assert_eq!(agent.history.last(), Some(&Item::Assistant("It printed hi.".into())));
        // Both turns are one conversation, so they share one cache session.
        assert!(requests.iter().all(|r| r.session_id.as_deref() == Some("session-1")));
    }

    #[tokio::test]
    async fn saves_the_conversation_and_loads_it_back_the_same() {
        let mut agent = agent(
            "save",
            vec![vec![bash("c1", "echo hi")], vec![Item::Assistant("Done.".into())]],
        );
        let folder = std::env::temp_dir().join(format!("rein-agent-{}-save-sessions", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        agent.session = Some(Session::new(&folder, "t", crate::session::Header {
            id: "session-1".into(),
            cwd: "/p".into(),
            started: "now".into(),
            model: "m".into(),
            instructions: None,
        }));
        agent.context = Some("<environment>here</environment>".into());
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("first".into(), &mut person).await.unwrap();
        // The context went in once, in front of the first message.
        assert_eq!(agent.history[0], Item::User("<environment>here</environment>".into()));
        assert_eq!(agent.history[1], Item::User("first".into()));

        // A turn stopped while its command runs leaves a call unanswered,
        // which the next request repairs.
        agent.record(bash("c2", "sleep 100")).unwrap();
        agent.repair();

        let path = agent.session.as_ref().unwrap().path().to_owned();
        let loaded = Session::load(&path).unwrap();
        let mut resumed = Agent { history: loaded.history, ..self::agent("save-resumed", vec![]) };
        resumed.repair();
        assert_eq!(resumed.history, agent.history, "a continued conversation sends exactly the same history");
    }

    #[tokio::test]
    async fn compacts_when_the_conversation_outgrows_its_limit() {
        let read = |id: &str| Item::ToolCall(ToolCall { call_id: id.into(), name: "read".into(), arguments: json!({ "path": "big.txt" }).to_string() });
        let recall = Item::ToolCall(ToolCall { call_id: "q1".into(), name: "recall".into(), arguments: json!({ "query": "needle" }).to_string() });
        let mut agent = agent(
            "compact",
            vec![
                vec![read("r1")],
                vec![Item::Assistant("Goal: find the needle.".into())],
                vec![Item::Assistant("It's in big.txt.".into())],
                vec![recall],
                vec![Item::Assistant("needle 42".into())],
            ],
        );
        let root = std::env::temp_dir().join(format!("rein-agent-{}-compact", std::process::id()));
        std::fs::write(root.join("big.txt"), format!("{}needle 42\n", "hay\n".repeat(3000))).unwrap();
        let folder = root.join(".sessions");
        agent.session = Some(Session::new(&folder, "t", crate::session::Header {
            id: "session-1".into(),
            cwd: "/p".into(),
            started: "now".into(),
            model: "m".into(),
            instructions: None,
        }));
        agent.window.limit = 2_000;
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("where is the needle?".into(), &mut person).await.unwrap();

        // The file pushed the conversation past the limit, so before the
        // next request the model was asked for a summary...
        let requests = agent.model.requests.borrow().clone();
        let ask = &requests[1];
        assert!(ask.input.starts_with(&requests[0].input), "it starts like the last request");
        assert_eq!(ask.input.len(), 4, "the question, the read, its result, then the ask");
        assert!(matches!(ask.input.last(), Some(Item::User(text)) if text.starts_with("Pause the work")));
        assert_eq!(ask.tools, requests[0].tools, "same tools, so the cache still applies");
        // ...and the conversation went on from the summary and rein's record.
        let Item::User(summary) = &requests[2].input[0] else { panic!("no summary") };
        assert!(summary.starts_with("<summary>") && summary.contains("Goal: find the needle."));
        assert!(summary.contains("- where is the needle?\nFiles changed: none\nFiles read: big.txt"));
        assert!(!requests[2].input.contains(&read("r1")), "the big read is gone");
        assert!(requests[0].tools.iter().any(|tool| tool.name == "recall"));

        // What was compacted away can still be found.
        agent.send("what line was it?".into(), &mut person).await.unwrap();
        let found = agent.history.iter().find_map(|item| match item {
            Item::ToolResult { call_id, output } if call_id == "q1" => Some(output.clone()),
            _ => None,
        });
        assert!(found.unwrap().contains("needle 42"));

        // And the saved file gives back exactly the same conversation.
        let loaded = Session::load(agent.session.as_ref().unwrap().path()).unwrap();
        assert_eq!(loaded.history, agent.history);
        assert_eq!(loaded.full, agent.window.full);
    }

    /// A server that answers 503 twice, then a real saved response.
    #[tokio::test]
    async fn retries_a_busy_server() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hello = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../rein-llm/tests/fixtures/real-hello.sse")).unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = [0u8; 65536];
                let _ = socket.read(&mut request);
                let reply = if attempt < 2 {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()
                } else {
                    let mut ok = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", hello.len()).into_bytes();
                    ok.extend_from_slice(&hello);
                    ok
                };
                socket.write_all(&reply).unwrap();
            }
        });
        let client = Client::new(rein_llm::reqwest::Client::new(), "test").with_base_url(format!("http://127.0.0.1:{port}"));
        let model = OpenAi { client, record: None };
        let request = Request {
            model: "m".into(),
            instructions: None,
            input: vec![Item::User("hi".into())],
            tools: Vec::new(),
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
        };
        let started = std::time::Instant::now();
        let (items, _) = model.respond(&request, &mut |_| {}).await.unwrap();
        assert!(items.iter().any(|item| matches!(item, Item::Assistant(_))));
        assert!(started.elapsed() >= std::time::Duration::from_secs(3), "waited 1 s, then 2 s");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_refused_command_never_runs() {
        let mut agent =
            agent("refuse", vec![vec![bash("c1", "touch made-it")], vec![Item::Assistant("Okay.".into())]]);
        let mut person = Person { answer: Approval::No, log: Vec::new() };
        agent.send("make a file".into(), &mut person).await.unwrap();

        assert!(!person.log.contains(&"ran bash".to_owned()));
        let root = std::env::temp_dir().join(format!("rein-agent-{}-refuse", std::process::id()));
        assert!(!root.join("made-it").exists());
        assert!(agent.history.contains(&Item::ToolResult {
            call_id: "c1".into(),
            output: "The user declined to run this command.".into()
        }));
    }

    #[tokio::test]
    async fn stopping_at_a_question_ends_the_turn() {
        // The script has no second turn: asking the model again would panic.
        let mut agent = agent("stop", vec![vec![bash("c1", "touch made-it")]]);
        let mut person = Person { answer: Approval::Stop, log: Vec::new() };
        assert_eq!(agent.send("make a file".into(), &mut person).await.unwrap(), Outcome::Stopped);

        assert!(!person.log.contains(&"ran bash".to_owned()));
        let output = "The user declined this and stopped, to tell you what to do instead.";
        assert_eq!(person.log.last(), Some(&format!("result {output}")));
        assert_eq!(agent.history.last(), Some(&Item::ToolResult { call_id: "c1".into(), output: output.into() }));
    }

    #[tokio::test]
    async fn file_changes_ask_first_too() {
        let write = Item::ToolCall(ToolCall {
            call_id: "c1".into(),
            name: "write".into(),
            arguments: json!({"path": "notes.txt", "content": "hi"}).to_string(),
        });
        let mut agent = agent("refuse-write", vec![vec![write], vec![Item::Assistant("Okay.".into())]]);
        let mut person = Person { answer: Approval::No, log: Vec::new() };
        agent.send("write a note".into(), &mut person).await.unwrap();

        assert!(person.log[0].starts_with("asked"), "{:?}", person.log);
        let root = std::env::temp_dir().join(format!("rein-agent-{}-refuse-write", std::process::id()));
        assert!(!root.join("notes.txt").exists());
        assert!(agent.history.contains(&Item::ToolResult { call_id: "c1".into(), output: "The user declined this change.".into() }));
    }

    #[tokio::test]
    async fn rules_run_or_block_without_asking() {
        let turns = vec![vec![bash("c1", "echo allowed"), bash("c2", "git push")], vec![Item::Assistant("Done.".into())]];
        let mut agent = agent("rules", turns);
        agent.permissions = Permissions::new(Mode::Ask, "allow echo\ndeny git push").unwrap();
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("go".into(), &mut person).await.unwrap();

        // Nothing was asked: one rule allowed, the other blocked.
        assert!(!person.log.iter().any(|l| l.starts_with("asked")), "{:?}", person.log);
        assert!(person.log.contains(&"result allowed\n[exit code 0]".to_owned()));
        assert!(agent.history.contains(&Item::ToolResult { call_id: "c2".into(), output: "Blocked by a rule: `deny git push`.".into() }));
        assert!(person.log.contains(&"blocked bash: Blocked by a rule: `deny git push`.".to_owned()));
    }

    #[tokio::test]
    async fn always_means_not_asking_again() {
        let turns = vec![vec![bash("c1", "echo once")], vec![bash("c2", "echo once")], vec![Item::Assistant("Done.".into())]];
        let mut agent = agent("always", turns);
        let mut person = Person { answer: Approval::Always, log: Vec::new() };
        agent.send("go".into(), &mut person).await.unwrap();
        let asked = person.log.iter().filter(|l| l.starts_with("asked")).count();
        assert_eq!(asked, 1, "{:?}", person.log);
        assert_eq!(person.log.iter().filter(|l| *l == "ran bash").count(), 2);
    }

    #[test]
    fn repair_answers_calls_a_cancelled_turn_left_open() {
        let mut agent = agent("repair", vec![]);
        agent.history = vec![
            Item::User("go".into()),
            bash("c1", "echo done"),
            Item::ToolResult { call_id: "c1".into(), output: "done".into() },
            bash("c2", "sleep 100"),
        ];
        agent.repair();
        assert_eq!(agent.history.len(), 5);
        assert!(matches!(&agent.history[4], Item::ToolResult { call_id, output } if call_id == "c2" && output.contains("interrupted")));
        // Repairing twice changes nothing.
        agent.repair();
        assert_eq!(agent.history.len(), 5);
    }

    #[tokio::test]
    async fn stops_a_model_that_never_stops_asking() {
        let turns = (0..MAX_TURNS).map(|i| vec![bash(&format!("c{i}"), "true")]).collect();
        let mut agent = agent("limit", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("loop".into(), &mut person).await.unwrap(), Outcome::TurnLimit);
    }
}
