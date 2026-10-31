//! The agent loop: ask the model, run the tools it asks for, send back what
//! they produced, and repeat until it answers without asking for anything.

use std::fs::File;
use std::future::Future;

use rein_llm::openai::{Client, Request};
use rein_llm::{Error, Event, Item, Result, ToolCall, Usage};

use crate::permissions::{Decision, Permissions};
use crate::tools::Tools;

/// After this many model turns in a row, stop and give the user control back.
pub const MAX_TURNS: usize = 25;

/// Something that answers requests. The real one is OpenAI; tests script one.
pub trait Model {
    /// Sends one request, passing answer text to `on_text` as it streams, and
    /// returns the finished items and the token counts.
    fn respond(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> impl Future<Output = Result<(Vec<Item>, Usage)>>;
}

/// The answer to "may this run?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Yes,
    /// Yes, and don't ask again this session: for this exact command, or
    /// for all file changes.
    Always,
    No,
}

/// Whoever is at the keyboard: sees what happens, and decides what may run.
pub trait Ui {
    fn text(&mut self, delta: &str);
    /// Asked when the permissions say to ask. It's async so that an
    /// interrupt can cancel the turn even while it waits for an answer.
    fn approve(&mut self, call: &ToolCall) -> impl Future<Output = Approval>;
    fn tool_started(&mut self, call: &ToolCall);
    /// A call the permissions refused without asking, and why.
    fn tool_blocked(&mut self, call: &ToolCall, reason: &str);
    fn tool_finished(&mut self, call: &ToolCall, output: &str);
    fn usage(&mut self, usage: &Usage);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The model answered without asking for a tool.
    Done,
    /// It kept asking for tools; we stopped after `MAX_TURNS`.
    TurnLimit,
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
}

impl<M: Model> Agent<M> {
    /// Adds the user's message and works on it until the model is done.
    /// Dropping the future cancels the turn; [`Agent::repair`] then makes
    /// the history valid again, and `send` calls it first anyway.
    pub async fn send(&mut self, prompt: String, ui: &mut impl Ui) -> Result<Outcome> {
        self.repair();
        self.history.push(Item::User(prompt));
        for _ in 0..MAX_TURNS {
            let request = Request {
                model: self.model_name.clone(),
                instructions: self.instructions.clone(),
                input: self.history.clone(),
                tools: self.tools.specs(),
                reasoning_effort: self.reasoning_effort.clone(),
                session_id: self.session_id.clone(),
            };
            let (items, usage) = self.model.respond(&request, &mut |text| ui.text(text)).await?;
            ui.usage(&usage);

            let calls: Vec<ToolCall> = items
                .iter()
                .filter_map(|item| match item {
                    Item::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect();
            // Everything the model produced goes into the history, in order:
            // text, encrypted reasoning, and the calls themselves.
            self.history.extend(items);
            if calls.is_empty() {
                return Ok(Outcome::Done);
            }

            for call in calls {
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
                self.history.push(Item::ToolResult { call_id: call.call_id, output });
            }
        }
        Ok(Outcome::TurnLimit)
    }
}

impl<M> Agent<M> {
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

impl Model for OpenAi {
    async fn respond(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> Result<(Vec<Item>, Usage)> {
        let mut stream = self.client.stream(request).await?;
        if let Some(file) = &self.record {
            stream.record_to(file.try_clone()?);
        }
        let mut items = Vec::new();
        while let Some(event) = stream.next().await {
            match event? {
                Event::TextDelta(text) => on_text(&text),
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
        async fn respond(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> Result<(Vec<Item>, Usage)> {
            self.requests.borrow_mut().push(request.clone());
            let items = self.turns.borrow_mut().pop_front().expect("the loop asked for more turns than scripted");
            for item in &items {
                if let Item::Assistant(text) = item {
                    on_text(text);
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
