//! The agent loop: ask the model, run the tools it asks for, send back what
//! they produced, and repeat until it answers without asking for anything.

use std::fs::File;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::Poll;

use serde_json::{Value, json};

use rein_llm::{Client, Error, Event, Item, Request, Result, STALE_THINKING, Tool, ToolCall, Usage};

use crate::compact;
use crate::context;
use crate::mentions;
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
    /// Something to tell the user about the answer, like another model
    /// having given it.
    Note(&'a str),
}

/// Something that answers requests. The real one is a provider's API
/// ([`Llm`]); tests script one.
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
    /// Something to know about the answer: another model gave it, say.
    fn note(&mut self, _note: &str) {}
    /// Asked when the permissions say to ask. It's async so that an
    /// interrupt can cancel the turn even while it waits for an answer.
    fn approve(&mut self, call: &ToolCall) -> impl Future<Output = Approval>;
    fn tool_started(&mut self, call: &ToolCall);
    /// Steps that only look, asked for together, starting at once.
    fn tools_started(&mut self, calls: &[ToolCall]) {
        for call in calls {
            self.tool_started(call);
        }
    }
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
    /// Messages the user sent while the turn went on, to steer it: taken
    /// out, oldest first. Asked between steps.
    fn steering(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// A steering message has gone into the conversation, here.
    fn steered(&mut self, _message: &str) {}
    /// What the user's message pointed at with `@`, read for them.
    fn attached(&mut self, call: &ToolCall, output: &str) {
        self.tool_started(call);
        self.tool_finished(call, output);
    }
}

/// The tools that only look, and so may run at the same time.
const LOOKS: [&str; 3] = ["read", "grep", "glob"];

/// A read's path and lines, to tell whether two reads read the same.
fn read_args(arguments: &str) -> Option<(String, Option<u64>, Option<u64>)> {
    let args: Value = serde_json::from_str(arguments).ok()?;
    let path = args["path"].as_str()?.trim_start_matches("./").to_owned();
    Some((path, args["start_line"].as_u64().filter(|&start| start > 1), args["line_count"].as_u64()))
}

fn read_path(arguments: &str) -> Option<String> {
    read_args(arguments).map(|(path, _, _)| path)
}

/// Runs `futures` at the same time; their outputs, in order.
async fn all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut waiting = false;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(done) => *output = Some(done),
                    Poll::Pending => waiting = true,
                }
            }
        }
        if waiting { Poll::Pending } else { Poll::Ready(()) }
    })
    .await;
    outputs.into_iter().flatten().collect()
}

/// What a step the model asked for gets instead of running, when the user
/// wrote before it started: what it was for may no longer hold.
pub const SKIPPED: &str = "Not run: the user sent a message before this step started. It follows; act on it.";

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
    /// The most one answer may take, when the model list says.
    pub max_output: Option<u64>,
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
        self.record(Item::User(prompt.clone()))?;
        self.attach(&prompt, ui).await?;
        // Compact at most once per message, so a summary that is itself too
        // big can't start a loop of summaries.
        let mut compacted = false;
        let mut unbound = false;
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
                Err(e) if !compacted && e.code() == Some(rein_llm::CONTEXT_FULL) => {
                    self.compact(None, true, ui).await?;
                    compacted = true;
                    continue;
                }
                // The provider turned the conversation's thinking away (it
                // was signed with a conversation that has changed since,
                // say by an older rein's tools): without it, once.
                Err(e) if !unbound && e.code() == Some(STALE_THINKING) => {
                    self.unbind()?;
                    unbound = true;
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
                // Done, unless the user wrote meanwhile: then that's next.
                if !self.steer(ui.steering(), ui).await? {
                    return Ok(Outcome::Done);
                }
                continue;
            }

            let mut next = 0;
            while next < calls.len() {
                // The user wrote: what's left was planned before that, so
                // it doesn't run. Every call still gets a result, as the
                // conversation needs one for each.
                let steering = ui.steering();
                if !steering.is_empty() {
                    for skipped in &calls[next..] {
                        ui.tool_finished(skipped, SKIPPED);
                        self.record(Item::ToolResult { call_id: skipped.call_id.clone(), output: SKIPPED.to_owned() })?;
                    }
                    self.steer(steering, ui).await?;
                    break;
                }
                // Lookups asked for together run at once: they only look.
                let together = calls[next..]
                    .iter()
                    .take_while(|call| LOOKS.contains(&call.name.as_str()) && self.permissions.decide(call) == Decision::Allow)
                    .count();
                if together > 1 {
                    let batch = &calls[next..next + together];
                    ui.tools_started(batch);
                    let outputs = all(batch.iter().map(|call| self.tools.run(call)).collect()).await;
                    for (call, output) in batch.iter().zip(outputs) {
                        ui.tool_finished(call, &output);
                        self.record(Item::ToolResult { call_id: call.call_id.clone(), output })?;
                    }
                    next += together;
                    continue;
                }
                let call = calls[next].clone();
                next += 1;
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
            // What the user wrote during the last step goes with its result.
            self.steer(ui.steering(), ui).await?;
        }
        Ok(Outcome::TurnLimit)
    }

    /// Adds the messages the user sent to steer the turn, where it is now,
    /// with what they point at. Says whether there were any.
    async fn steer(&mut self, messages: Vec<String>, ui: &mut impl Ui) -> Result<bool> {
        let any = !messages.is_empty();
        for message in messages {
            ui.steered(&message);
            self.record(Item::User(message.clone()))?;
            self.attach(&message, ui).await?;
        }
        Ok(any)
    }

    /// Reads what `message` points at with `@`, as steps of the user's: a
    /// file as `read` gives it, some lines of it, or a folder's files. A
    /// file the conversation already holds as it is now gets a note that
    /// says so instead of a second copy. A mention inside a folder with
    /// its own AGENTS.md brings that along, once.
    async fn attach(&mut self, message: &str, ui: &mut impl Ui) -> Result<()> {
        for mention in mentions::find(message) {
            let full = self.tools.root().join(&mention.path);
            // `@alice` is a word, not a file; `@src/gone.rs` is worth a word.
            if !full.exists() && !mention.looks_like_a_path() {
                continue;
            }
            let folder = full.is_dir();
            for instructions in self.instructions_for(&mention.path, folder) {
                let call = self.mention_call("read", json!({ "path": instructions, "start_line": null, "line_count": null }));
                let text = self.tools.run(&call).await;
                let output = format!("Instructions for working in {}/, from {instructions}; follow them there:\n\n{text}", Path::new(&instructions).parent().map(|p| p.display().to_string()).unwrap_or_default());
                self.attached(call, output, ui)?;
            }
            let call = if folder {
                self.mention_call("glob", json!({ "pattern": "*", "path": mention.path.trim_end_matches('/') }))
            } else {
                let (start, count) = match mention.lines {
                    Some((first, last)) => (json!(first), last.map_or(json!(null), |last| json!(last - first + 1))),
                    None => (json!(null), json!(null)),
                };
                self.mention_call("read", json!({ "path": mention.path, "start_line": start, "line_count": count }))
            };
            let mut output = self.tools.run(&call).await;
            if call.name == "read" && self.last_copy(&call.arguments) == Some(output.as_str()) {
                output = format!("{} {} is as it was when it was last read in this conversation, so that copy is current.]", compact::UNCHANGED, mention.path);
            }
            self.attached(call, output, ui)?;
        }
        Ok(())
    }

    fn attached(&mut self, call: ToolCall, output: String, ui: &mut impl Ui) -> Result<()> {
        ui.attached(&call, &output);
        let call_id = call.call_id.clone();
        self.record(Item::ToolCall(call))?;
        self.record(Item::ToolResult { call_id, output })
    }

    /// A call of `tool` made for the user. Its ID is unique in the
    /// conversation: it grows by at least one item for each.
    fn mention_call(&self, tool: &str, arguments: Value) -> ToolCall {
        ToolCall { call_id: format!("mention_{}", self.window.full.len()), name: tool.to_owned(), arguments: arguments.to_string() }
    }

    /// The AGENTS.md (or CLAUDE.md) files in the folders between the project
    /// and `path`, outermost first, that the conversation doesn't hold yet:
    /// the project's own came with the first message.
    fn instructions_for(&self, path: &str, folder: bool) -> Vec<String> {
        let path = Path::new(path.trim_end_matches('/'));
        let deepest = if folder { Some(path) } else { path.parent() };
        let mut folders: Vec<&Path> = deepest.into_iter().flat_map(Path::ancestors).filter(|dir| !dir.as_os_str().is_empty()).collect();
        folders.reverse();
        folders
            .into_iter()
            .filter_map(|dir| {
                let file = ["AGENTS.md", "CLAUDE.md"].into_iter().map(|name| dir.join(name)).find(|file| self.tools.root().join(file).is_file())?;
                let file = file.to_string_lossy().into_owned();
                let held = self.history.iter().any(|item| matches!(item, Item::ToolCall(call) if call.name == "read" && read_path(&call.arguments) == Some(file.clone())));
                (!held).then_some(file)
            })
            .collect()
    }

    /// What the latest read with these arguments gave, if the model can
    /// still see it: not retired, and not itself a note pointing further back.
    fn last_copy(&self, arguments: &str) -> Option<&str> {
        let wanted = read_args(arguments);
        let mut ids = std::collections::HashSet::new();
        let mut latest = None;
        for item in &self.history {
            match item {
                Item::ToolCall(call) if call.name == "read" && read_args(&call.arguments) == wanted => {
                    ids.insert(call.call_id.as_str());
                }
                Item::ToolResult { call_id, output } if ids.contains(call_id.as_str()) && !output.starts_with(compact::UNCHANGED) => {
                    latest = Some(output.as_str());
                }
                _ => {}
            }
        }
        latest.filter(|output| !output.starts_with(compact::RETIRED))
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
        let mut tries = 0;
        let (items, usage) = loop {
            let mut input = self.history.clone();
            input.push(Item::User(compact::prompt(focus)));
            ui.thinking();
            match self.model.respond(&self.request(input), &mut |streamed| stream_to(ui, streamed)).await {
                Err(e) if tries == 0 && e.code() == Some(STALE_THINKING) => {
                    self.unbind()?;
                    tries += 1;
                }
                answer => break answer?,
            }
        };
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
        // The kept part follows the summary now, not what its thinking was
        // signed with.
        compact::unbind(&mut kept);
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
            max_output_tokens: self.max_output,
        }
    }

    /// Drops the conversation's thinking that only held while what came
    /// before it stayed the same ([`compact::unbind`]), and saves that.
    fn unbind(&mut self) -> Result<()> {
        compact::unbind(&mut self.history);
        if let Some(session) = &mut self.session {
            session.append_unbound()?;
        }
        self.window.measured = None;
        Ok(())
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

/// A provider's API as a [`Model`], optionally saving every raw response
/// stream to `record` for replay tests.
pub struct Llm {
    pub client: Client,
    pub record: Option<File>,
}

fn stream_to(ui: &mut impl Ui, streamed: Streamed) {
    match streamed {
        Streamed::Text(text) => ui.text(text),
        Streamed::Thought(text) => ui.thought(text),
        Streamed::Note(note) => ui.note(note),
    }
}

/// How long to wait before each retry of a failed request.
const RETRY_WAITS: [u64; 3] = [1, 2, 4];

impl Model for Llm {
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
                Event::Note(note) => on(Streamed::Note(&note)),
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

    /// Turns away any request carrying Claude thinking, as Anthropic does
    /// once the conversation before it has changed; answers the rest.
    struct Picky {
        requests: RefCell<Vec<Request>>,
    }

    impl Model for Picky {
        async fn respond(&self, request: &Request, _: &mut dyn FnMut(Streamed)) -> Result<(Vec<Item>, Usage)> {
            self.requests.borrow_mut().push(request.clone());
            if request.input.iter().any(|item| matches!(item, Item::Opaque(o) if o.bound())) {
                let message = "messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.";
                return Err(Error::api(Some(400), Some("invalid_request_error".into()), message.into(), None));
            }
            Ok((vec![Item::Assistant("ok".into())], Usage::default()))
        }
    }

    #[tokio::test]
    async fn thinking_the_provider_turns_away_is_dropped_and_the_turn_goes_on() {
        let thinking = Item::Opaque(rein_llm::Opaque::new(rein_llm::Api::Anthropic, json!({"type": "thinking", "thinking": "", "signature": "old"})));
        let mut agent = base_with(agent("unbind", vec![]));
        agent.history = vec![Item::User("first".into()), thinking.clone(), Item::Assistant("answer".into())];
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("next".into(), &mut person).await.unwrap(), Outcome::Done);
        let requests = agent.model.requests.borrow();
        assert_eq!(requests.len(), 2, "turned away once, then answered");
        assert!(requests[1].input.iter().all(|item| !matches!(item, Item::Opaque(o) if o.bound())));
        assert!(!agent.history.contains(&thinking));
        assert_eq!(agent.history.last(), Some(&Item::Assistant("ok".into())));
    }

    /// `agent`'s parts, for an agent with another model.
    fn base_with(agent: Agent<Scripted>) -> Agent<Picky> {
        let Agent { model: _, tools, permissions, model_name, instructions, reasoning_effort, max_output, session_id, history, session, context, window } = agent;
        Agent { model: Picky { requests: RefCell::new(Vec::new()) }, tools, permissions, model_name, instructions, reasoning_effort, max_output, session_id, history, session, context, window }
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

    /// Approves everything, and writes `message` once `after` steps are done.
    struct Steerer {
        message: Option<String>,
        after: usize,
        finished: usize,
        log: Vec<String>,
    }

    impl Ui for Steerer {
        fn text(&mut self, _: &str) {}
        async fn approve(&mut self, _: &ToolCall) -> Approval {
            Approval::Yes
        }
        fn tool_started(&mut self, call: &ToolCall) {
            self.log.push(format!("ran {}", call.arguments));
        }
        fn tool_blocked(&mut self, _: &ToolCall, _: &str) {}
        fn tool_finished(&mut self, call: &ToolCall, output: &str) {
            self.finished += 1;
            self.log.push(format!("result of {}: {output}", call.call_id));
        }
        fn usage(&mut self, _: &Usage) {}
        fn steering(&mut self) -> Vec<String> {
            if self.finished < self.after { Vec::new() } else { self.message.take().into_iter().collect() }
        }
        fn steered(&mut self, message: &str) {
            self.log.push(format!("steered {message}"));
        }
    }

    #[tokio::test]
    async fn a_message_sent_mid_turn_skips_the_steps_not_started() {
        let mut agent = agent(
            "steer",
            vec![vec![bash("c1", "touch one"), bash("c2", "touch two")], vec![Item::Assistant("Using the helper.".into())]],
        );
        let root = std::env::temp_dir().join(format!("rein-agent-{}-steer", std::process::id()));
        let mut steerer = Steerer { message: Some("stop, use the helper".into()), after: 1, finished: 0, log: Vec::new() };
        assert_eq!(agent.send("make two files".into(), &mut steerer).await.unwrap(), Outcome::Done);
        assert!(root.join("one").exists() && !root.join("two").exists(), "the second step never ran");
        assert_eq!(steerer.log, [
            r#"ran {"command":"touch one"}"#.to_owned(),
            "result of c1: [exit code 0]".to_owned(),
            format!("result of c2: {SKIPPED}"),
            "steered stop, use the helper".to_owned(),
        ]);
        // The model next sees every call answered, then the message, last:
        // the start of the conversation is unchanged, for the cache.
        let requests = agent.model.requests.borrow();
        assert_eq!(
            requests[1].input,
            [
                Item::User("make two files".into()),
                bash("c1", "touch one"),
                bash("c2", "touch two"),
                Item::ToolResult { call_id: "c1".into(), output: "[exit code 0]".into() },
                Item::ToolResult { call_id: "c2".into(), output: SKIPPED.into() },
                Item::User("stop, use the helper".into()),
            ]
        );
    }

    #[tokio::test]
    async fn a_message_sent_as_the_model_finishes_keeps_the_turn_going() {
        let mut agent = agent("steer-end", vec![vec![Item::Assistant("Done.".into())], vec![Item::Assistant("Also renamed it.".into())]]);
        let mut steerer = Steerer { message: Some("and rename it".into()), after: 0, finished: 0, log: Vec::new() };
        assert_eq!(agent.send("fix it".into(), &mut steerer).await.unwrap(), Outcome::Done);
        let requests = agent.model.requests.borrow();
        assert_eq!(requests.len(), 2, "one turn, carried on");
        assert_eq!(requests[1].input.last(), Some(&Item::User("and rename it".into())));
        assert_eq!(agent.history.last(), Some(&Item::Assistant("Also renamed it.".into())));
    }

    fn root_of(dir: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rein-agent-{}-{dir}", std::process::id()))
    }

    fn read_call(id: &str, path: &str, start: Value, count: Value) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "read".into(), arguments: json!({ "path": path, "start_line": start, "line_count": count }).to_string() })
    }

    fn result(id: &str, output: &str) -> Item {
        Item::ToolResult { call_id: id.into(), output: output.into() }
    }

    #[tokio::test]
    async fn what_a_message_points_at_is_read_with_it() {
        let mut agent = agent("mention", vec![vec![Item::Assistant("Seen.".into())]]);
        let root = root_of("mention");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("sub/AGENTS.md"), "Use tabs.\n").unwrap();
        std::fs::write(root.join("sub/b.rs"), "one\ntwo\nthree\n").unwrap();
        let message = "look at @src/a.rs and @sub/b.rs:2-2, ask @alice, and check @gone/x.rs";
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send(message.into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        let input = &requests[0].input;
        assert_eq!(input[..7], [
            Item::User(message.into()),
            read_call("mention_1", "src/a.rs", Value::Null, Value::Null),
            result("mention_1", "fn a() {}\n"),
            read_call("mention_3", "sub/AGENTS.md", Value::Null, Value::Null),
            result("mention_3", "Instructions for working in sub/, from sub/AGENTS.md; follow them there:\n\nUse tabs.\n"),
            read_call("mention_5", "sub/b.rs", json!(2), json!(1)),
            result("mention_5", "two\n[lines 2-2 of 3; read again with start_line 3 for more]"),
        ]);
        assert!(matches!(&input[8], Item::ToolResult { output, .. } if output.starts_with("error: can't read gone/x.rs")), "a path that isn't there is said so: {:?}", input[8]);
        assert_eq!(input.len(), 9, "@alice is just a word");
    }

    #[tokio::test]
    async fn a_file_already_in_the_conversation_isnt_sent_again() {
        let turns = vec![vec![Item::Assistant("1".into())], vec![Item::Assistant("2".into())], vec![Item::Assistant("3".into())]];
        let mut agent = agent("again", turns);
        let root = root_of("again");
        std::fs::write(root.join("a.txt"), "first\n").unwrap();
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("see @a.txt".into(), &mut person).await.unwrap();
        agent.send("again @a.txt".into(), &mut person).await.unwrap();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        agent.send("and now @a.txt".into(), &mut person).await.unwrap();
        let results: Vec<&str> = agent
            .history
            .iter()
            .filter_map(|item| match item {
                Item::ToolResult { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, [
            "first\n",
            "[unchanged: a.txt is as it was when it was last read in this conversation, so that copy is current.]",
            "changed\n",
        ]);
    }

    #[tokio::test]
    async fn lookups_asked_for_together_run_together() {
        let grep = Item::ToolCall(ToolCall {
            call_id: "g".into(),
            name: "grep".into(),
            arguments: json!({ "pattern": "two", "path": null, "glob": null, "ignore_case": false, "output": "files", "context": null }).to_string(),
        });
        let turns = vec![vec![read_call("r1", "a.txt", Value::Null, Value::Null), read_call("r2", "b.txt", Value::Null, Value::Null), grep, bash("c1", "echo after")], vec![Item::Assistant("Done.".into())]];
        let mut agent = agent("together", turns);
        let root = root_of("together");
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("look".into(), &mut person).await.unwrap();
        // The three lookups start together and their results come in order;
        // the command, which asks first, comes after them on its own.
        assert_eq!(person.log, [
            "ran read",
            "ran read",
            "ran grep",
            "result one\n",
            "result two\n",
            "result 1 file with matches (1 matching line)\nb.txt (1 match)",
            r#"asked {"command":"echo after"}"#,
            "ran bash",
            "result after\n[exit code 0]",
            "text Done.",
        ]);
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
            max_output: None,
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
                vec![Item::Opaque(rein_llm::Opaque::new(rein_llm::Api::Responses, json!({"type": "reasoning"}))), bash("c1", "echo hi")],
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
                Item::Opaque(rein_llm::Opaque::new(rein_llm::Api::Responses, json!({"type": "reasoning"}))),
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
        let client = Client::new(rein_llm::reqwest::Client::new(), rein_llm::Api::Responses, format!("http://127.0.0.1:{port}"), rein_llm::Auth::Bearer("test".into()));
        let model = Llm { client, record: None };
        let request = Request {
            model: "m".into(),
            instructions: None,
            input: vec![Item::User("hi".into())],
            tools: Vec::new(),
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
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
