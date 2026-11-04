//! The agent loop: ask the model, run the tools it asks for, send back what
//! they produced, and repeat until it answers without asking for anything.

use std::fs::File;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::Poll;

use serde_json::{Value, json};

use rein_llm::{Client, Error, ErrorKind, Event, Item, Request, Result, Tool, ToolCall, Usage};

use crate::compact;
use crate::context;
use crate::mentions;
use crate::permissions::{Decision, Permissions, Rule};
use crate::session::{Loaded, Session};
use crate::tools::{ToolName, Tools};

/// How many steps a turn nobody is watching may take: in auto mode, or a
/// one-shot task. While you watch there's no limit; you can press Esc.
pub const DEFAULT_MAX_STEPS: usize = 200;

/// How many times in a row the same call may come out the same (one call,
/// or a short cycle of them) before the model is told, and before rein
/// stops the turn: a model going round in circles, not one that's slow.
const REPEATS_NOTED: usize = 3;
const REPEATS_STOP: usize = 5;
/// The longest cycle of calls that counts as repeating: `a, b, a, b`.
const LONGEST_CYCLE: usize = 4;

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
    fn respond(&self, request: &Request<'_>, on: &mut dyn FnMut(Streamed)) -> impl Future<Output = Result<(Vec<Item>, Usage)>>;
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

/// Something that came in for the conversation, and from whom. Only
/// [`Heard::You`] is the user's word: it's what `@` is read in, and the
/// others are framed so the model knows they aren't from the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// What the user typed.
    You(String),
    /// What another rein sent.
    Peer { from: String, text: String },
    /// Another rein in this folder changed `path`. It's said only if this
    /// conversation has read or changed that file.
    Changed { by: String, path: String },
}

/// Whoever is at the keyboard: sees what happens, and decides what may run.
pub trait Ui {
    fn text(&mut self, delta: &str);
    /// A piece of a summary of the model's thinking, like `**Checking the
    /// tests**`.
    fn thought(&mut self, _delta: &str) {}
    /// Something to know about the answer: another model gave it, say.
    fn note(&mut self, _note: &str) {}
    /// Asked when the permissions say to ask, with the rule that asks, if
    /// one does (in trust mode, only a rule asks). It's async so that an
    /// interrupt can cancel the turn even while it waits for an answer.
    fn approve(&mut self, call: &ToolCall, rule: Option<&Rule>) -> impl Future<Output = Approval>;
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
    /// The conversation's first request waits for MCP servers still
    /// starting: their tools go in it.
    fn starting_servers(&mut self) {}
    /// Compaction is starting. A summary, if one is written, streams
    /// through `text`.
    fn compacting(&mut self) {}
    fn compacted(&mut self, _report: &Compacted) {}
    /// What came in while the turn went on, to steer it: the user's
    /// messages, and other reins'. Taken out, oldest first; asked between
    /// steps.
    fn steering(&mut self) -> Vec<Heard> {
        Vec::new()
    }
    /// Something heard has gone into the conversation, here.
    fn steered(&mut self, _heard: &Heard) {}
    /// Does what the model asked of another rein (`peers`, `message`,
    /// `delegate`), and says how it went. Only offered where there are
    /// other reins to reach; see [`Agent::with_peer_tools`].
    fn peer(&mut self, call: &ToolCall) -> impl Future<Output = String> {
        std::future::ready(format!("{}there are no other reins to reach from here: {}", crate::tools::ERROR, call.name))
    }
    /// What the user's message pointed at with `@`, read for them.
    fn attached(&mut self, call: &ToolCall, output: &str) {
        self.tool_started(call);
        self.tool_finished(call, output);
    }
}

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

/// What a step gets when the user says no. Compaction's record knows a
/// declined step by these, so they're said in one place.
pub const DECLINED_COMMAND: &str = "The user declined to run this command.";
pub const DECLINED_CHANGE: &str = "The user declined this change.";
pub const DECLINED_CALL: &str = "The user declined this tool call.";
pub const DECLINED_AND_STOPPED: &str = "The user declined this and stopped, to tell you what to do instead.";
/// What a step cut short by Ctrl-C or Esc gets.
pub const INTERRUPTED: &str = "The user interrupted this before it finished. It may have partly run.";
/// Added to a result that came out the same as the ones before it,
/// `REPEATS_NOTED` times in a row.
pub const REPEATING: &str = "\n[rein: that's the same call, with the same result, several times in a row. \
                             If it isn't getting anywhere, try something different, or stop and ask the user.]";
/// Added to the result that made it `REPEATS_STOP`: the turn stops there.
pub const STOPPED_REPEATING: &str = "\n[rein stopped the turn here: the same call kept giving the same result.]";
/// The result of each call a stuck turn didn't get to.
pub const NOT_RUN_STUCK: &str = "Not run: rein stopped the turn, which was going round in circles.";
/// Added to the last result of the last step a turn may take.
pub const LAST_STEP: &str = "\n[rein: that was the last step this turn may take. Don't call any more tools: \
                             say what you've done, what's left, and how to go on.]";
/// The result of each call asked for after the last step.
pub const NOT_RUN_PAST_LIMIT: &str = "Not run: the turn had taken all the steps it may.";

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
struct Window {
    /// Compact once a request would carry more than this many tokens.
    limit: u64,
    /// The provider's token count for the last request, and how many
    /// history items that request carried.
    measured: Option<(u64, usize)>,
    /// Everything said so far, compacted away or not: rein's record and
    /// `recall` work from it. It's never sent as is.
    full: Vec<Item>,
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
    /// The turn took the steps it may ([`Agent::step_limit`]), and the
    /// model was asked to say where it got to.
    StepLimit(usize),
    /// The model made `call` again and again with the same result: it was
    /// going round in circles, and rein stopped the turn.
    Stuck { call: ToolCall, times: usize },
    /// The user declined a step and stopped the turn, to say what to do
    /// instead.
    Stopped,
}

/// How a step's calls went.
enum Ran {
    /// Each one ran, or was refused, declined or skipped.
    All,
    /// The user declined one and stopped the turn.
    Stopped,
    /// One came out the same as before, too many times in a row.
    Stuck { call: ToolCall, times: usize },
}

/// What each call in a turn gave, in order, so that a model going round in
/// circles shows: the same calls, with the same results, again and again.
#[derive(Default)]
struct Repeats(Vec<u64>);

impl Repeats {
    /// Adds what `call` gave; how many times in a row the calls at the end
    /// (one, or a short cycle of them) have now come out the same.
    fn add(&mut self, call: &ToolCall, output: &str) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        call.name.hash(&mut hasher);
        // The same arguments however they're spaced or ordered.
        serde_json::from_str::<Value>(&call.arguments).map_or_else(|_| call.arguments.clone(), |args| args.to_string()).hash(&mut hasher);
        output.hash(&mut hasher);
        self.0.push(hasher.finish());
        repeated(&self.0)
    }
}

/// How many times the calls at the end of `seen` repeat, as one call or a
/// cycle of up to `LONGEST_CYCLE`: `[x, a, b, a, b, a, b]` is 3.
fn repeated(seen: &[u64]) -> usize {
    (1..=LONGEST_CYCLE.min(seen.len()))
        .map(|cycle| {
            let last = &seen[seen.len() - cycle..];
            let mut times = 1;
            while (times + 1) * cycle <= seen.len() && &seen[seen.len() - (times + 1) * cycle..seen.len() - times * cycle] == last {
                times += 1;
            }
            times
        })
        .max()
        .unwrap_or(0)
}

/// `output` with what rein tells the model about the turn: that the call
/// came out the same `times` in a row, if that's many, and that this was
/// the last result of its last step, when `call` is `closing`.
fn with_note(mut output: String, call: &ToolCall, closing: Option<&str>, times: usize) -> String {
    if times >= REPEATS_STOP {
        output.push_str(STOPPED_REPEATING);
    } else if times >= REPEATS_NOTED {
        output.push_str(REPEATING);
    }
    if closing == Some(call.call_id.as_str()) {
        output.push_str(LAST_STEP);
    }
    output
}

/// The agent: settings anyone may change (the public fields), and the
/// conversation it's working in, which only its methods change, so what has
/// to stay together does: the history, everything said, the session file,
/// and what the window was last measured at.
pub struct Agent<M> {
    pub model: M,
    pub tools: Tools,
    /// What may run without asking.
    pub permissions: Permissions,
    pub model_name: String,
    /// How hard you asked the model to think: `None` for its own default.
    pub reasoning_effort: Option<rein_llm::Effort>,
    /// The levels the model takes, which decide what it's sent for
    /// `reasoning_effort` (see [`Agent::effort`]).
    pub efforts: rein_llm::Efforts,
    /// The most one answer may take, when the model list says.
    pub max_output: Option<u64>,
    /// How many steps (answers from the model) a turn may take before it's
    /// stopped, and the model asked to sum up: `None` while someone is
    /// watching, who can press Esc; a number when nobody is.
    pub step_limit: Option<usize>,
    /// The tools offered: the same in every request, so built once.
    specs: Vec<Tool>,
    /// Whether `specs` has this conversation's MCP tools: they're added
    /// before its first request, and stay as they are after, until you
    /// change the servers.
    listed: bool,
    /// The system prompt, the same for the whole conversation.
    instructions: Option<String>,
    /// One ID for the whole conversation, so its requests share a prompt cache.
    session_id: Option<String>,
    /// The whole conversation. Every request sends all of it.
    history: Vec<Item>,
    /// Where the conversation is saved as it grows, if anywhere.
    session: Option<Session>,
    /// What the model is told before the first message (see
    /// [`crate::context`]); it goes in when the first message does.
    context: Option<String>,
    window: Window,
}

impl<M: Model> Agent<M> {
    /// Adds the user's message and works on it until the model is done.
    /// Dropping the future cancels the turn; [`Agent::repair`] then makes
    /// the history valid again, and `send` calls it first anyway.
    pub async fn send(&mut self, prompt: String, ui: &mut impl Ui) -> Result<Outcome> {
        self.begin()?;
        self.offer_servers_tools(ui).await;
        self.record(Item::User(prompt.clone()))?;
        self.attach(&prompt, ui).await?;
        self.work(ui).await
    }

    /// Works on what another rein, `from`, sent, as [`Agent::send`] does
    /// on the user's message: framed as not the user's, and nothing it
    /// points at with `@` is read.
    pub async fn hear(&mut self, from: &str, text: &str, ui: &mut impl Ui) -> Result<Outcome> {
        self.begin()?;
        self.offer_servers_tools(ui).await;
        self.record(Item::User(crate::peers::heard(from, text)))?;
        self.work(ui).await
    }

    /// What every turn starts with: the history made valid again, a
    /// checkpoint for `/undo`, and the context, before the first message.
    fn begin(&mut self) -> Result<()> {
        self.repair();
        self.tools.checkpoint();
        if let Some(context) = self.context.take() {
            if let Some(session) = &mut self.session {
                session.append_context(&context)?;
            }
            self.window.full.push(Item::User(context.clone()));
            self.history.push(Item::User(context));
        }
        Ok(())
    }

    /// Offers the MCP servers' tools, once in a conversation, before its
    /// first request: once each server has started, or failed to. From then
    /// on the tools stay as they are, though a server starts late or lists
    /// others: a changed list would lose the prompt cache, and a server
    /// mustn't be able to change what the model was told mid-conversation.
    /// Only you do, adding or removing one ([`Agent::servers_changed`]).
    /// What each server came to is said.
    async fn offer_servers_tools(&mut self, ui: &mut impl Ui) {
        let Some(servers) = self.tools.mcp().filter(|_| !self.listed) else { return };
        if servers.starting() {
            ui.starting_servers();
        }
        let listing = servers.list().await;
        self.specs.retain(|tool| !rein_mcp::is_mcp(&tool.name));
        // A server's schema is as it wrote it, not to strict mode's rules.
        self.specs.extend(listing.tools.into_iter().map(|tool| Tool { name: tool.name, description: tool.description, parameters: tool.parameters, strict: false }));
        self.permissions.allow_tools(listing.allowed);
        self.listed = true;
        if let Some(note) = listing.note {
            ui.note(&note);
        }
    }

    /// You added or removed an MCP server, or let its tools run without
    /// asking: the model is offered the servers' tools as they are now
    /// with the next message.
    pub fn servers_changed(&mut self) {
        self.listed = false;
    }

    /// Works until the model is done, the turn is stopped, or it's stuck.
    async fn work(&mut self, ui: &mut impl Ui) -> Result<Outcome> {
        // Compact when the conversation gets too long, as often as a long turn
        // needs, but only after a step since the last time, and never again
        // after one that didn't make room: a summary that is itself too big
        // mustn't start a loop of summaries.
        let (mut steps, mut compacted_at, mut no_room) = (0, None, false);
        let mut repeats = Repeats::default();
        let mut unbound = false;
        loop {
            let may_compact = !no_room && compacted_at != Some(steps);
            if may_compact && self.estimated() > self.window.limit {
                self.compact(None, false, ui).await?;
                (compacted_at, no_room) = (Some(steps), self.estimated() > self.window.limit);
            }
            let request = self.request(&self.history);
            let sent = self.history.len();
            ui.thinking();
            let (items, usage) = match self.model.respond(&request, &mut |streamed| stream_to(ui, streamed)).await {
                // The provider says it's too long after all: make room and
                // try again.
                Err(e) if !no_room && compacted_at != Some(steps) && e.kind() == Some(ErrorKind::ContextFull) => {
                    self.compact(None, true, ui).await?;
                    compacted_at = Some(steps);
                    continue;
                }
                // The provider turned the conversation's thinking away (it
                // was signed with a conversation that has changed since,
                // say by an older rein's tools): without it, once.
                Err(e) if !unbound && e.kind() == Some(ErrorKind::StaleThinking) => {
                    self.unbind()?;
                    unbound = true;
                    continue;
                }
                other => other?,
            };
            ui.usage(&usage);
            self.window.measured = Some((usage.input_tokens, sent));
            steps += 1;

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
            // Past the limit: the model was told its last step was its last.
            let past = self.step_limit.filter(|limit| steps > *limit);
            if calls.is_empty() {
                if let Some(limit) = past {
                    return Ok(Outcome::StepLimit(limit));
                }
                // Done, unless the user wrote meanwhile: then that's next.
                if !self.steer(ui.steering(), ui).await? {
                    return Ok(Outcome::Done);
                }
                continue;
            }
            if let Some(limit) = past {
                for call in &calls {
                    ui.tool_finished(call, NOT_RUN_PAST_LIMIT);
                    self.record(Item::ToolResult { call_id: call.call_id.clone(), output: NOT_RUN_PAST_LIMIT.to_owned() })?;
                }
                return Ok(Outcome::StepLimit(limit));
            }
            let last = self.step_limit == Some(steps);
            match self.run_step(&calls, last, &mut repeats, ui).await? {
                Ran::All => {}
                Ran::Stopped => return Ok(Outcome::Stopped),
                Ran::Stuck { call, times } => return Ok(Outcome::Stuck { call, times }),
            }
            // What the user wrote during the last step goes with its result.
            self.steer(ui.steering(), ui).await?;
        }
    }

    /// Runs one step's `calls`, lookups asked for together at once, and
    /// records each result. On the turn's `last` step, the last result tells
    /// the model so.
    async fn run_step(&mut self, calls: &[ToolCall], last: bool, repeats: &mut Repeats, ui: &mut impl Ui) -> Result<Ran> {
        let closing = if last { calls.last().map(|call| call.call_id.clone()) } else { None };
        let mut next = 0;
        while next < calls.len() {
            // The user wrote: what's left was planned before that, so it
            // doesn't run. Every call still gets a result, as the
            // conversation needs one for each.
            let steering = ui.steering();
            if !steering.is_empty() {
                for skipped in &calls[next..] {
                    let output = with_note(SKIPPED.to_owned(), skipped, closing.as_deref(), 0);
                    ui.tool_finished(skipped, &output);
                    self.record(Item::ToolResult { call_id: skipped.call_id.clone(), output })?;
                }
                self.steer(steering, ui).await?;
                return Ok(Ran::All);
            }
            // Lookups asked for together run at once: they only look.
            let together = calls[next..].iter().take_while(|call| ToolName::of(call).is_some_and(ToolName::only_looks) && self.permissions.decide(call) == Decision::Allow).count();
            if together > 1 {
                let batch = &calls[next..next + together];
                ui.tools_started(batch);
                let outputs = all(batch.iter().map(|call| self.tools.run(call, self.permissions.how(call))).collect()).await;
                for (i, (call, output)) in batch.iter().zip(outputs).enumerate() {
                    if let Some(stuck) = self.finish(call, output, repeats, closing.as_deref(), ui)? {
                        return self.stop_stuck(stuck, &calls[next + i + 1..], ui);
                    }
                }
                next += together;
                continue;
            }
            let call = calls[next].clone();
            next += 1;
            // Other reins are reached by whoever runs the agent. Nothing
            // runs here: what the other rein does, it asks its own user about.
            if ToolName::of(&call).is_some_and(ToolName::reaches_peers) {
                ui.tool_started(&call);
                let output = ui.peer(&call).await;
                if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            // Searching the conversation is rein's own business: no
            // permission needed, nothing outside is touched.
            if ToolName::of(&call) == Some(ToolName::Recall) {
                ui.tool_started(&call);
                let output = compact::recall(&self.window.full, &call.arguments);
                if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            let approved = match self.permissions.decide(&call) {
                Decision::Allow => Ok(()),
                Decision::Deny(reason) => {
                    ui.tool_blocked(&call, &reason);
                    Err(reason)
                }
                Decision::Ask => match ui.approve(&call, self.permissions.asked_by(&call)).await {
                    Approval::Yes => Ok(()),
                    Approval::Always => {
                        self.permissions.remember(&call);
                        Ok(())
                    }
                    Approval::No => Err(match ToolName::of(&call) {
                        Some(ToolName::Bash) => DECLINED_COMMAND.to_owned(),
                        Some(_) => DECLINED_CHANGE.to_owned(),
                        None => DECLINED_CALL.to_owned(),
                    }),
                    Approval::Stop => {
                        let output = DECLINED_AND_STOPPED.to_owned();
                        ui.tool_finished(&call, &output);
                        self.record(Item::ToolResult { call_id: call.call_id, output })?;
                        return Ok(Ran::Stopped);
                    }
                },
            };
            let output = match approved {
                Ok(()) => {
                    ui.tool_started(&call);
                    self.tools.run(&call, self.permissions.how(&call)).await
                }
                Err(reason) => reason,
            };
            if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                return self.stop_stuck(stuck, &calls[next..], ui);
            }
        }
        Ok(Ran::All)
    }

    /// Records what `call` gave, noting when it has come out the same many
    /// times in a row, and when it's the last of the turn's last step.
    /// `Some` when it came out the same too many times: the turn stops.
    fn finish(&mut self, call: &ToolCall, output: String, repeats: &mut Repeats, closing: Option<&str>, ui: &mut impl Ui) -> Result<Option<Ran>> {
        let times = repeats.add(call, &output);
        let output = with_note(output, call, closing, times);
        ui.tool_finished(call, &output);
        self.record(Item::ToolResult { call_id: call.call_id.clone(), output })?;
        Ok((times >= REPEATS_STOP).then(|| Ran::Stuck { call: call.clone(), times }))
    }

    /// Stops a turn that's going round in circles: what it didn't get to
    /// gets a result, as every call needs one.
    fn stop_stuck(&mut self, stuck: Ran, left: &[ToolCall], ui: &mut impl Ui) -> Result<Ran> {
        for call in left {
            ui.tool_finished(call, NOT_RUN_STUCK);
            self.record(Item::ToolResult { call_id: call.call_id.clone(), output: NOT_RUN_STUCK.to_owned() })?;
        }
        Ok(stuck)
    }

    /// Adds what came in to steer the turn, where it is now: the user's
    /// messages with what they point at, other reins' framed as theirs, and
    /// notes of files changed by others that this conversation has seen.
    /// Says whether there was a message, which keeps the turn going.
    async fn steer(&mut self, heard: Vec<Heard>, ui: &mut impl Ui) -> Result<bool> {
        let mut any = false;
        for heard in heard {
            match &heard {
                Heard::You(message) => {
                    any = true;
                    ui.steered(&heard);
                    self.record(Item::User(message.clone()))?;
                    self.attach(message, ui).await?;
                }
                Heard::Peer { from, text } => {
                    any = true;
                    ui.steered(&heard);
                    self.record(Item::User(crate::peers::heard(from, text)))?;
                }
                // Only what matters here: a file this conversation knows.
                Heard::Changed { by, path } if self.has_seen(path) => {
                    ui.steered(&heard);
                    self.record(Item::User(crate::peers::changed(by, path)))?;
                }
                Heard::Changed { .. } => {}
            }
        }
        Ok(any)
    }

    /// This conversation has read, written or edited `path`.
    fn has_seen(&self, path: &str) -> bool {
        let path = path.trim_start_matches("./");
        self.window.full.iter().any(|item| match item {
            Item::ToolCall(call) => matches!(ToolName::of(call), Some(ToolName::Read | ToolName::Write | ToolName::Edit)) && read_path(&call.arguments).as_deref() == Some(path),
            _ => false,
        })
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
                let text = self.tools.run(&call, self.permissions.how(&call)).await;
                let output = format!(
                    "Instructions for working in {}/, from {instructions}; follow them there:\n\n{text}",
                    Path::new(&instructions).parent().map(|p| p.display().to_string()).unwrap_or_default()
                );
                self.attached(call, output, ui)?;
            }
            let call = if folder {
                self.mention_call("glob", json!({ "pattern": "*", "path": mention.path.trim_end_matches('/') }))
            } else {
                let (start, count) = match mention.lines {
                    Some((first, last)) => (json!(first), last.and_then(|last| last.checked_sub(first)).map_or(json!(null), |more| json!(more + 1))),
                    None => (json!(null), json!(null)),
                };
                self.mention_call("read", json!({ "path": mention.path, "start_line": start, "line_count": count }))
            };
            let mut output = self.tools.run(&call, self.permissions.how(&call)).await;
            if ToolName::of(&call) == Some(ToolName::Read) && self.last_copy(&call.arguments) == Some(output.as_str()) {
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
                let held = self
                    .history
                    .iter()
                    .any(|item| matches!(item, Item::ToolCall(call) if ToolName::of(call) == Some(ToolName::Read) && read_path(&call.arguments) == Some(file.clone())));
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
                Item::ToolCall(call) if ToolName::of(call) == Some(ToolName::Read) && read_args(&call.arguments) == wanted => {
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
            match self.model.respond(&self.request(&input), &mut |streamed| stream_to(ui, streamed)).await {
                Err(e) if tries == 0 && e.kind() == Some(ErrorKind::StaleThinking) => {
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
    /// An agent working in `conversation` (saved or new), compacting once a
    /// request would carry `limit` tokens. `context` is what a new one tells
    /// the model first.
    pub fn new(model: M, tools: Tools, permissions: Permissions, model_name: String, limit: u64, conversation: Loaded, context: Option<String>) -> Self {
        let header = conversation.session.header().clone();
        Agent {
            specs: tool_specs(&tools),
            listed: false,
            model,
            tools,
            permissions,
            model_name,
            reasoning_effort: None,
            efforts: rein_llm::Efforts::Unknown,
            max_output: None,
            step_limit: None,
            instructions: header.instructions,
            session_id: Some(header.id),
            history: conversation.history,
            session: Some(conversation.session),
            context,
            window: Window { limit, measured: None, full: conversation.full },
        }
    }

    /// Moves to another conversation. The settings stay; what was only
    /// true of the last one goes with it: what you said "always" to, and
    /// the file changes there are to undo.
    pub fn resume(&mut self, conversation: Loaded, context: Option<String>) {
        let header = conversation.session.header().clone();
        self.session_id = Some(header.id);
        self.instructions = header.instructions;
        self.history = conversation.history;
        self.session = Some(conversation.session);
        self.context = context;
        self.window.full = conversation.full;
        self.window.measured = None;
        self.permissions.forget_approvals();
        self.tools.forget_changes();
        // A new conversation, a new list: servers that started late, or
        // list other tools now, are in it.
        self.listed = false;
    }

    /// The same agent, in the same conversation, answered by `model`.
    pub fn with_model<N>(self, model: N) -> Agent<N> {
        let Agent {
            model: _,
            tools,
            permissions,
            model_name,
            reasoning_effort,
            efforts,
            max_output,
            step_limit,
            specs,
            listed,
            instructions,
            session_id,
            history,
            session,
            context,
            window,
        } = self;
        Agent {
            model,
            tools,
            permissions,
            model_name,
            reasoning_effort,
            efforts,
            max_output,
            step_limit,
            specs,
            listed,
            instructions,
            session_id,
            history,
            session,
            context,
            window,
        }
    }

    /// The level the model is sent for the one you chose: the same where it
    /// has it, else the closest it has below (see [`rein_llm::Efforts::sent`]).
    pub fn effort(&self) -> Option<rein_llm::Effort> {
        self.efforts.sent(self.reasoning_effort)
    }

    /// Also offers the tools that reach other reins: `peers`, `message` and
    /// `delegate`, which [`Ui::peer`] does. Before the first request, so
    /// the tools stay the same all conversation, for the cache.
    pub fn with_peer_tools(mut self) -> Self {
        self.specs.extend(crate::peers::specs());
        self
    }

    /// The peer tools for a sub-agent: no `spawn`, so it stays one level deep.
    pub fn with_child_peer_tools(mut self) -> Self {
        self.specs.extend(crate::peers::child_specs());
        self
    }

    /// Compact once a request would carry more than this many tokens.
    pub fn limit(&self) -> u64 {
        self.window.limit
    }

    pub fn set_limit(&mut self, limit: u64) {
        self.window.limit = limit;
    }

    /// Everything said in the conversation, compacted away or not.
    pub fn full(&self) -> &[Item] {
        &self.window.full
    }

    /// The conversation's file, if it's saved.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Saves that the conversation uses `model` from here on.
    pub fn save_model(&mut self, model: &str) -> Result<()> {
        match &mut self.session {
            Some(session) => session.append_model(model),
            None => Ok(()),
        }
    }

    /// The tools offered. `recall` is there from the start, though it only
    /// matters after a compaction: the tool list comes before everything
    /// else in a request, so changing it would lose the whole cache.
    fn specs(&self) -> &[Tool] {
        &self.specs
    }

    /// A request carrying `input`, borrowed: nothing is copied.
    fn request<'a>(&'a self, input: &'a [Item]) -> Request<'a> {
        Request {
            model: &self.model_name,
            instructions: self.instructions.as_deref(),
            input,
            tools: &self.specs,
            reasoning_effort: self.effort(),
            reasoning_summary: true,
            session_id: self.session_id.as_deref(),
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
                let output = INTERRUPTED.to_owned();
                repaired.push(Item::ToolResult { call_id, output });
            }
        }
        self.history = repaired;
    }
}

/// The tools offered. `recall` is there from the start, though it only
/// matters after a compaction: the tool list comes before everything else
/// in a request, so changing it would lose the whole cache.
fn tool_specs(tools: &Tools) -> Vec<Tool> {
    let mut specs = tools.specs();
    specs.push(compact::recall_spec());
    specs
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
    async fn respond(&self, request: &Request<'_>, on: &mut dyn FnMut(Streamed)) -> Result<(Vec<Item>, Usage)> {
        // Only the request itself is retried: once the answer has started,
        // trying again would show it twice.
        let mut waits = RETRY_WAITS.iter();
        let mut stream = loop {
            match self.client.stream(request).await {
                Err(e)
                    if e.retryable()
                        && let Some(&seconds) = waits.next() =>
                {
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

    /// What a test model kept of a request it was sent: requests borrow,
    /// so it keeps its own copy.
    #[derive(Clone)]
    struct Seen {
        input: Vec<Item>,
        tools: Vec<Tool>,
        session_id: Option<String>,
    }

    impl From<&Request<'_>> for Seen {
        fn from(request: &Request<'_>) -> Self {
            Seen { input: request.input.to_vec(), tools: request.tools.to_vec(), session_id: request.session_id.map(str::to_owned) }
        }
    }

    /// Plays back scripted turns and remembers every request it was sent.
    struct Scripted {
        turns: RefCell<VecDeque<Vec<Item>>>,
        requests: RefCell<Vec<Seen>>,
    }

    impl Scripted {
        fn new(turns: Vec<Vec<Item>>) -> Self {
            Scripted { turns: RefCell::new(turns.into()), requests: RefCell::new(Vec::new()) }
        }
    }

    impl Model for Scripted {
        async fn respond(&self, request: &Request<'_>, on: &mut dyn FnMut(Streamed)) -> Result<(Vec<Item>, Usage)> {
            self.requests.borrow_mut().push(request.into());
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
        requests: RefCell<Vec<Seen>>,
    }

    impl Model for Picky {
        async fn respond(&self, request: &Request<'_>, _: &mut dyn FnMut(Streamed)) -> Result<(Vec<Item>, Usage)> {
            self.requests.borrow_mut().push(request.into());
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
        let mut agent = agent("unbind", vec![]).with_model(Picky { requests: RefCell::new(Vec::new()) });
        agent.history = vec![Item::User("first".into()), thinking.clone(), Item::Assistant("answer".into())];
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("next".into(), &mut person).await.unwrap(), Outcome::Done);
        let requests = agent.model.requests.borrow();
        assert_eq!(requests.len(), 2, "turned away once, then answered");
        assert!(requests[1].input.iter().all(|item| !matches!(item, Item::Opaque(o) if o.bound())));
        assert!(!agent.history.contains(&thinking));
        assert_eq!(agent.history.last(), Some(&Item::Assistant("ok".into())));
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
        async fn approve(&mut self, call: &ToolCall, _: Option<&Rule>) -> Approval {
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
        message: Option<Heard>,
        after: usize,
        finished: usize,
        log: Vec<String>,
    }

    impl Ui for Steerer {
        fn text(&mut self, _: &str) {}
        async fn approve(&mut self, _: &ToolCall, _: Option<&Rule>) -> Approval {
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
        fn steering(&mut self) -> Vec<Heard> {
            if self.finished < self.after { Vec::new() } else { self.message.take().into_iter().collect() }
        }
        fn steered(&mut self, heard: &Heard) {
            self.log.push(match heard {
                Heard::You(message) => format!("steered {message}"),
                other => format!("steered {other:?}"),
            });
        }
    }

    /// An MCP server's tools are in the conversation's first request, and
    /// the same in every one after; a call asks, but for what your mcp.json
    /// allows, and what it gives has the keys rein knows hidden.
    #[tokio::test]
    async fn an_mcp_servers_tools_are_offered_from_the_first_request_and_ask_when_called() {
        let call = |id: &str, name: &str, arguments: Value| Item::ToolCall(ToolCall { call_id: id.into(), name: name.into(), arguments: arguments.to_string() });
        let turns = vec![
            vec![call("c1", "mcp__fake__echo", json!({ "text": "hello" })), call("c2", "mcp__fake__given", json!({}))],
            vec![Item::Assistant("Done.".into())],
            vec![Item::Assistant("Still here.".into())],
        ];
        let mut agent = agent("mcp", turns);
        let root = agent.tools.root().to_owned();
        std::fs::write(root.join("server.sh"), rein_mcp::FAKE_SERVER).unwrap();
        let local = rein_mcp::Local {
            command: "sh".into(),
            args: vec!["server.sh".into()],
            env: [("GIVEN".to_owned(), "FAKE-mcp-key-123".to_owned())].into(),
            access: crate::sandbox::Access::Outside,
        };
        let server = rein_mcp::Server { name: "fake".into(), origin: rein_mcp::Origin::Yours, reach: rein_mcp::Reach::Local(local), allow: vec!["echo".into()] };
        let launch = rein_mcp::Launch { sandbox: Default::default(), root: root.clone(), caches: root.join("caches"), keys: Default::default(), web: None, tokens: None };
        let servers = std::sync::Arc::new(rein_mcp::Servers::start(vec![server], launch));
        agent.tools = Tools::new(&root).with_mcp(servers).with_secrets(crate::redact::Secrets::new(["FAKE-mcp-key-123".to_owned()]));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("use the server".into(), &mut person).await.unwrap();
        assert_eq!(
            person.log,
            ["ran mcp__fake__echo", "result hello", "asked {}", "ran mcp__fake__given", &format!("result {}", crate::redact::HIDDEN), "text Done."],
            "echo is allowed in your file; given asked"
        );
        agent.send("and now?".into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        let names = |seen: &Seen| seen.tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
        assert!(names(&requests[0]).contains(&"mcp__fake__echo".to_owned()), "{:?}", names(&requests[0]));
        assert!(requests.iter().all(|seen| names(seen) == names(&requests[0])), "the same tools all conversation");
        assert!(requests[0].tools.iter().filter(|tool| rein_mcp::is_mcp(&tool.name)).all(|tool| !tool.strict));
    }

    /// A server you add or remove as rein runs changes the tools from the
    /// next message on, and only then.
    #[tokio::test]
    async fn the_servers_tools_change_with_the_next_message_once_you_change_the_servers() {
        let says = |text: &str| vec![Item::Assistant(text.into())];
        let mut agent = agent("mcp-changed", vec![says("One."), says("Two."), says("Three."), says("Four.")]);
        let root = agent.tools.root().to_owned();
        std::fs::write(root.join("server.sh"), rein_mcp::FAKE_SERVER).unwrap();
        let launch = rein_mcp::Launch { sandbox: Default::default(), root: root.clone(), caches: root.join("caches"), keys: Default::default(), web: None, tokens: None };
        let servers = std::sync::Arc::new(rein_mcp::Servers::start(Vec::new(), launch));
        agent.tools = Tools::new(&root).with_mcp(servers.clone());
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("one".into(), &mut person).await.unwrap();
        let local = rein_mcp::Local { command: "sh".into(), args: vec!["server.sh".into()], env: Default::default(), access: crate::sandbox::Access::Outside };
        let server = rein_mcp::Server { name: "fake".into(), origin: rein_mcp::Origin::Yours, reach: rein_mcp::Reach::Local(local), allow: Vec::new() };
        servers.add(server);
        agent.send("two".into(), &mut person).await.unwrap();
        agent.servers_changed();
        agent.send("three".into(), &mut person).await.unwrap();
        servers.remove("fake");
        agent.servers_changed();
        agent.send("four".into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        let servers_tools = |seen: &Seen| seen.tools.iter().filter(|tool| rein_mcp::is_mcp(&tool.name)).count();
        assert_eq!(requests.iter().map(servers_tools).collect::<Vec<_>>(), [0, 0, 5, 0], "added: not until you said so; then in; removed: out");
    }

    #[tokio::test]
    async fn a_message_sent_mid_turn_skips_the_steps_not_started() {
        let mut agent = agent("steer", vec![vec![bash("c1", "touch one"), bash("c2", "touch two")], vec![Item::Assistant("Using the helper.".into())]]);
        let root = std::env::temp_dir().join(format!("rein-agent-{}-steer", std::process::id()));
        let mut steerer = Steerer { message: Some(Heard::You("stop, use the helper".into())), after: 1, finished: 0, log: Vec::new() };
        assert_eq!(agent.send("make two files".into(), &mut steerer).await.unwrap(), Outcome::Done);
        assert!(root.join("one").exists() && !root.join("two").exists(), "the second step never ran");
        assert_eq!(
            steerer.log,
            [r#"ran {"command":"touch one"}"#.to_owned(), "result of c1: [exit code 0]".to_owned(), format!("result of c2: {SKIPPED}"), "steered stop, use the helper".to_owned(),]
        );
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
        let mut steerer = Steerer { message: Some(Heard::You("and rename it".into())), after: 0, finished: 0, log: Vec::new() };
        assert_eq!(agent.send("fix it".into(), &mut steerer).await.unwrap(), Outcome::Done);
        let requests = agent.model.requests.borrow();
        assert_eq!(requests.len(), 2, "one turn, carried on");
        assert_eq!(requests[1].input.last(), Some(&Item::User("and rename it".into())));
        assert_eq!(agent.history.last(), Some(&Item::Assistant("Also renamed it.".into())));
    }

    #[tokio::test]
    async fn another_reins_message_is_framed_and_what_it_points_at_isnt_read() {
        let mut agent = agent("peer-heard", vec![vec![Item::Assistant("On it.".into())]]);
        std::fs::write(root_of("peer-heard").join("secret.txt"), "not for peers\n").unwrap();
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.hear("api", "look at @secret.txt", &mut person).await.unwrap(), Outcome::Done);
        assert_eq!(agent.history[0], Item::User(crate::peers::heard("api", "look at @secret.txt")));
        assert!(matches!(&agent.history[0], Item::User(text) if text.contains("It's not from the user")));
        assert!(!agent.history.iter().any(|item| matches!(item, Item::ToolCall(_) | Item::ToolResult { .. })), "nothing was read for it: {:?}", agent.history);
    }

    #[tokio::test]
    async fn a_peers_change_is_noted_only_for_a_file_this_conversation_has_seen() {
        let read =
            Item::ToolCall(ToolCall { call_id: "r".into(), name: "read".into(), arguments: json!({ "path": "src/a.rs", "start_line": null, "line_count": null }).to_string() });
        let turns = vec![vec![read], vec![Item::Assistant("Done.".into())]];
        let mut first = agent("peer-changed", turns);
        let mut seen = Steerer { message: Some(Heard::Changed { by: "api".into(), path: "./src/a.rs".into() }), after: 1, finished: 0, log: Vec::new() };
        first.send("read it".into(), &mut seen).await.unwrap();
        assert!(first.history.contains(&Item::User(crate::peers::changed("api", "./src/a.rs"))), "{:?}", first.history);

        let mut agent = agent("peer-unseen", vec![vec![Item::Assistant("Done.".into())]]);
        let mut unseen = Steerer { message: Some(Heard::Changed { by: "api".into(), path: "src/b.rs".into() }), after: 0, finished: 0, log: Vec::new() };
        agent.send("hi".into(), &mut unseen).await.unwrap();
        assert_eq!(agent.history.len(), 2, "a file it never saw isn't mentioned, nor does it keep the turn going: {:?}", agent.history);
        assert_eq!(agent.model.requests.borrow().len(), 1);
    }

    #[tokio::test]
    async fn reaching_another_rein_is_the_uis_and_never_asks() {
        let message = Item::ToolCall(ToolCall { call_id: "m".into(), name: "message".into(), arguments: json!({ "to": "api", "text": "hi" }).to_string() });
        let mut agent = agent("peer-tool", vec![vec![message], vec![Item::Assistant("Sent.".into())]]).with_peer_tools();
        let mut person = Person { answer: Approval::No, log: Vec::new() };
        agent.send("tell api hi".into(), &mut person).await.unwrap();
        assert!(!person.log.iter().any(|line| line.starts_with("asked")), "{:?}", person.log);
        let result = result_of(&agent, "m");
        assert!(result.contains("no other reins"), "the default ui has no peers: {result}");
        let offered: Vec<String> = agent.model.requests.borrow()[0].tools.iter().map(|tool| tool.name.clone()).collect();
        for name in ["peers", "message", "delegate"] {
            assert!(offered.iter().any(|tool| tool == name), "{name} is offered: {offered:?}");
        }
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
        assert_eq!(
            input[..7],
            [
                Item::User(message.into()),
                read_call("mention_1", "src/a.rs", Value::Null, Value::Null),
                result("mention_1", "fn a() {}\n"),
                read_call("mention_3", "sub/AGENTS.md", Value::Null, Value::Null),
                result("mention_3", "Instructions for working in sub/, from sub/AGENTS.md; follow them there:\n\nUse tabs.\n"),
                read_call("mention_5", "sub/b.rs", json!(2), json!(1)),
                result("mention_5", "two\n[lines 2-2 of 3; read again with start_line 3 for more]"),
            ]
        );
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
        assert_eq!(results, ["first\n", "[unchanged: a.txt is as it was when it was last read in this conversation, so that copy is current.]", "changed\n",]);
    }

    #[tokio::test]
    async fn lookups_asked_for_together_run_together() {
        let grep = Item::ToolCall(ToolCall {
            call_id: "g".into(),
            name: "grep".into(),
            arguments: json!({ "pattern": "two", "path": null, "glob": null, "ignore_case": false, "output": "files", "context": null }).to_string(),
        });
        let turns = vec![
            vec![read_call("r1", "a.txt", Value::Null, Value::Null), read_call("r2", "b.txt", Value::Null, Value::Null), grep, bash("c1", "echo after")],
            vec![Item::Assistant("Done.".into())],
        ];
        let mut agent = agent("together", turns);
        let root = root_of("together");
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("look".into(), &mut person).await.unwrap();
        // The three lookups start together and their results come in order;
        // the command, which asks first, comes after them on its own.
        assert_eq!(
            person.log,
            [
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
            ]
        );
    }

    fn bash(id: &str, command: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "bash".into(), arguments: json!({ "command": command }).to_string() })
    }

    fn agent(dir: &str, turns: Vec<Vec<Item>>) -> Agent<Scripted> {
        let root = std::env::temp_dir().join(format!("rein-agent-{}-{dir}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let tools = Tools::new(root);
        Agent {
            model: Scripted::new(turns),
            specs: tool_specs(&tools),
            listed: false,
            tools,
            // Asking, so there's something for the person to answer.
            permissions: Permissions::new(Mode::Ask, "").unwrap(),
            model_name: "m".into(),
            instructions: None,
            reasoning_effort: None,
            efforts: rein_llm::Efforts::Unknown,
            max_output: None,
            step_limit: None,
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
        let mut agent = agent("save", vec![vec![bash("c1", "echo hi")], vec![Item::Assistant("Done.".into())]]);
        let folder = std::env::temp_dir().join(format!("rein-agent-{}-save-sessions", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        agent.session =
            Some(Session::new(&folder, "t", crate::session::Header { id: "session-1".into(), cwd: "/p".into(), started: "now".into(), model: "m".into(), instructions: None }));
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
        agent.session =
            Some(Session::new(&folder, "t", crate::session::Header { id: "session-1".into(), cwd: "/p".into(), started: "now".into(), model: "m".into(), instructions: None }));
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
        let input = [Item::User("hi".into())];
        let request =
            Request { model: "m", instructions: None, input: &input, tools: &[], reasoning_effort: None, reasoning_summary: false, session_id: None, max_output_tokens: None };
        let started = std::time::Instant::now();
        let (items, _) = model.respond(&request, &mut |_| {}).await.unwrap();
        assert!(items.iter().any(|item| matches!(item, Item::Assistant(_))));
        assert!(started.elapsed() >= std::time::Duration::from_secs(3), "waited 1 s, then 2 s");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_refused_command_never_runs() {
        let mut agent = agent("refuse", vec![vec![bash("c1", "touch made-it")], vec![Item::Assistant("Okay.".into())]]);
        let mut person = Person { answer: Approval::No, log: Vec::new() };
        agent.send("make a file".into(), &mut person).await.unwrap();

        assert!(!person.log.contains(&"ran bash".to_owned()));
        let root = std::env::temp_dir().join(format!("rein-agent-{}-refuse", std::process::id()));
        assert!(!root.join("made-it").exists());
        assert!(agent.history.contains(&Item::ToolResult { call_id: "c1".into(), output: "The user declined to run this command.".into() }));
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
        let write = Item::ToolCall(ToolCall { call_id: "c1".into(), name: "write".into(), arguments: json!({"path": "notes.txt", "content": "hi"}).to_string() });
        let mut agent = agent("refuse-write", vec![vec![write], vec![Item::Assistant("Okay.".into())]]);
        let mut person = Person { answer: Approval::No, log: Vec::new() };
        agent.send("write a note".into(), &mut person).await.unwrap();

        assert!(person.log[0].starts_with("asked"), "{:?}", person.log);
        let root = std::env::temp_dir().join(format!("rein-agent-{}-refuse-write", std::process::id()));
        assert!(!root.join("notes.txt").exists());
        assert!(agent.history.contains(&Item::ToolResult { call_id: "c1".into(), output: "The user declined this change.".into() }));
    }

    #[tokio::test]
    async fn deny_rules_block_without_asking_and_allow_rules_need_the_walls() {
        let turns = vec![vec![bash("c1", "echo allowed"), bash("c2", "git push")], vec![Item::Assistant("Done.".into())]];
        let mut agent = agent("rules", turns);
        agent.permissions = Permissions::new(Mode::Ask, "allow echo\ndeny git push").unwrap();
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("go".into(), &mut person).await.unwrap();

        // A deny rule blocks without asking. With no sandbox here, an
        // allow rule, which came with the project, doesn't spare asking.
        assert!(person.log.iter().any(|l| l == "asked {\"command\":\"echo allowed\"}"), "{:?}", person.log);
        assert!(person.log.contains(&"result allowed\n[exit code 0]".to_owned()));
        assert_eq!(person.log.iter().filter(|l| l.starts_with("asked")).count(), 1, "git push wasn't asked about: {:?}", person.log);
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
        agent.history = vec![Item::User("go".into()), bash("c1", "echo done"), Item::ToolResult { call_id: "c1".into(), output: "done".into() }, bash("c2", "sleep 100")];
        agent.repair();
        assert_eq!(agent.history.len(), 5);
        assert!(matches!(&agent.history[4], Item::ToolResult { call_id, output } if call_id == "c2" && output.contains("interrupted")));
        // Repairing twice changes nothing.
        agent.repair();
        assert_eq!(agent.history.len(), 5);
    }

    /// The result the model got for `call_id`, as the history has it.
    fn result_of<'a>(agent: &'a Agent<Scripted>, call_id: &str) -> &'a str {
        agent
            .history
            .iter()
            .find_map(|item| match item {
                Item::ToolResult { call_id: id, output } if id == call_id => Some(output.as_str()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {call_id}"))
    }

    #[tokio::test]
    async fn while_youre_watching_a_turn_takes_as_many_steps_as_it_needs() {
        let mut turns: Vec<Vec<Item>> = (0..40).map(|i| vec![bash(&format!("c{i}"), &format!("echo {i}"))]).collect();
        turns.push(vec![Item::Assistant("Done, in 40 steps.".into())]);
        let mut agent = agent("unlimited", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("work".into(), &mut person).await.unwrap(), Outcome::Done);
        assert_eq!(agent.model.requests.borrow().len(), 41);
    }

    #[tokio::test]
    async fn a_turn_nobody_watches_stops_at_its_limit_and_sums_up() {
        let mut turns: Vec<Vec<Item>> = (0..3).map(|i| vec![bash(&format!("c{i}"), &format!("echo {i}"))]).collect();
        turns.push(vec![Item::Assistant("Three of five done; the parser is left.".into())]);
        let mut agent = agent("limit", turns);
        agent.step_limit = Some(3);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("work".into(), &mut person).await.unwrap(), Outcome::StepLimit(3));
        // The last step's result said so, and one more answer summed up.
        assert!(result_of(&agent, "c2").ends_with(LAST_STEP), "{}", result_of(&agent, "c2"));
        assert!(!result_of(&agent, "c1").contains(LAST_STEP));
        assert_eq!(agent.model.requests.borrow().len(), 4);
        assert_eq!(agent.history.last(), Some(&Item::Assistant("Three of five done; the parser is left.".into())));
    }

    #[tokio::test]
    async fn what_a_model_asks_for_after_its_last_step_doesnt_run() {
        let mut turns: Vec<Vec<Item>> = (0..2).map(|i| vec![bash(&format!("c{i}"), &format!("echo {i}"))]).collect();
        turns.push(vec![bash("late", "touch ran.txt")]);
        let mut agent = agent("past-limit", turns);
        agent.step_limit = Some(2);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("work".into(), &mut person).await.unwrap(), Outcome::StepLimit(2));
        assert_eq!(result_of(&agent, "late"), NOT_RUN_PAST_LIMIT);
        assert!(!root_of("past-limit").join("ran.txt").exists());
        assert_eq!(agent.model.requests.borrow().len(), 3, "and nothing more is asked");
    }

    #[tokio::test]
    async fn a_model_going_round_in_circles_is_told_then_stopped() {
        let turns = (0..6).map(|i| vec![bash(&format!("c{i}"), "echo same")]).collect();
        let mut agent = agent("circles", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        let outcome = agent.send("work".into(), &mut person).await.unwrap();
        let ToolCall { call_id, .. } = match &outcome {
            Outcome::Stuck { call, times: 5 } => call.clone(),
            other => panic!("not stopped as stuck: {other:?}"),
        };
        assert_eq!(call_id, "c4", "the fifth time");
        assert!(!result_of(&agent, "c1").contains(REPEATING));
        assert!(result_of(&agent, "c2").ends_with(REPEATING), "told the third time: {}", result_of(&agent, "c2"));
        assert!(result_of(&agent, "c4").ends_with(STOPPED_REPEATING));
        assert_eq!(agent.model.requests.borrow().len(), 5, "the sixth step never came");
    }

    #[tokio::test]
    async fn a_cycle_of_calls_going_nowhere_counts_too() {
        let turns = (0..10).map(|i| vec![bash(&format!("c{i}"), if i % 2 == 0 { "echo a" } else { "echo b" })]).collect();
        let mut agent = agent("cycle", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert!(matches!(agent.send("work".into(), &mut person).await.unwrap(), Outcome::Stuck { times: 5, .. }));
        assert_eq!(agent.model.requests.borrow().len(), 10);
    }

    #[tokio::test]
    async fn the_same_call_getting_somewhere_isnt_going_round_in_circles() {
        // Each time, the same command, and something new: a build making progress.
        let mut turns: Vec<Vec<Item>> = (0..8).map(|i| vec![bash(&format!("c{i}"), "echo x >> log.txt && wc -l < log.txt")]).collect();
        turns.push(vec![Item::Assistant("It got there.".into())]);
        let mut agent = agent("progress", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("work".into(), &mut person).await.unwrap(), Outcome::Done);
        assert!(!result_of(&agent, "c7").contains(REPEATING));
    }

    #[tokio::test]
    async fn a_long_turn_compacts_as_often_as_it_needs() {
        let read = |id: &str, path: &str| Item::ToolCall(ToolCall { call_id: id.into(), name: "read".into(), arguments: json!({ "path": path }).to_string() });
        let mut agent = agent(
            "compact-twice",
            vec![
                vec![read("r1", "one.txt")],
                vec![Item::Assistant("Goal: read both. Read one.".into())],
                vec![read("r2", "two.txt")],
                vec![Item::Assistant("Goal: read both. Read both.".into())],
                vec![Item::Assistant("Both read.".into())],
            ],
        );
        let root = root_of("compact-twice");
        std::fs::write(root.join("one.txt"), "hay one\n".repeat(2000)).unwrap();
        std::fs::write(root.join("two.txt"), "hay two\n".repeat(2000)).unwrap();
        agent.window.limit = 2_000;
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("read both".into(), &mut person).await.unwrap(), Outcome::Done);
        let requests = agent.model.requests.borrow().clone();
        let asked_to_sum_up = requests.iter().filter(|r| matches!(r.input.last(), Some(Item::User(text)) if text.starts_with("Pause the work"))).count();
        assert_eq!(asked_to_sum_up, 2, "once for each file that filled the window");
        assert_eq!(requests.len(), 5);
    }

    #[test]
    fn counts_how_often_the_calls_at_the_end_repeat() {
        assert_eq!(repeated(&[]), 0);
        assert_eq!(repeated(&[1]), 1);
        assert_eq!(repeated(&[2, 1, 1, 1]), 3);
        assert_eq!(repeated(&[9, 1, 2, 1, 2, 1, 2]), 3);
        assert_eq!(repeated(&[1, 2, 3, 1, 2, 3]), 2);
        assert_eq!(repeated(&[1, 2, 3, 4, 5]), 1);
    }
}
