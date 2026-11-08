//! The agent loop: ask the model, run the tools it asks for, send back what
//! they produced, and repeat until it answers without asking for anything.

use std::fs::File;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::Poll;

use serde_json::{Value, json};

use rein_llm::{Client, Error, ErrorKind, Event, Item, Loading, Request, Result, Tool, ToolCall, Usage};

use crate::compact;
use crate::context;
use crate::finding;
use crate::hooks::{self, By, Hooks};
use crate::mentions;
use crate::permissions::{Decision, Permissions, Rule};
use crate::session::{Loaded, Session};
use crate::tools::{BashCall, ToolName, Tools};
use crate::undo::{Listed, Plan, Watched};

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
    /// What the provider said, with the answer, of how much of your limits
    /// is used.
    Meters(&'a [rein_llm::Meter]),
}

/// Something that answers requests. The real one is a provider's API
/// ([`Llm`]); tests script one.
pub trait Model {
    /// Sends one request, passing what streams in to `on` as it arrives,
    /// and returns the finished items and the token counts.
    fn respond(&self, request: &Request<'_>, on: &mut dyn FnMut(Streamed)) -> impl Future<Output = Result<(Vec<Item>, Usage)>>;

    /// Whether `request`, as this model is sent it, holds back the tools
    /// found with the search: they cost nothing until they're found.
    fn holds_back(&self, request: &Request<'_>) -> bool {
        request.holds_back()
    }
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
    /// A sub-agent's report, shown whole where you saw it come: it goes
    /// in as another rein's word, and the model answers it.
    Reported { from: String, text: String },
    /// What another rein told while no turn ran, shown as it came (what
    /// you did in a sub-agent's tab): it goes in as the next turn starts,
    /// and starts nothing itself.
    Told { from: String, text: String },
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
    /// What the provider said of how much of your limits is used.
    fn meters(&mut self, _meters: &[rein_llm::Meter]) {}
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
    /// What a command did to the project's files, before it's finished.
    fn watched(&mut self, _call: &ToolCall, _what: &Watched) {}
    /// What one of your hooks did to `call`, before it's finished: changed
    /// it, or added to what the model is told of it.
    fn hooked(&mut self, _call: &ToolCall, _what: &str) {}
    /// Whether you're away, as the terminal tells: hooks on notifications
    /// run then, as the bell rings.
    fn away(&self) -> bool {
        false
    }
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
    /// What came in while no turn ran, taken as one starts: it goes in
    /// before what starts it, so the model knows it from its first step.
    fn waiting(&mut self) -> Vec<Heard> {
        Vec::new()
    }
    /// A background command ended, and the model is told, here.
    fn background_ended(&mut self, _ended: &crate::background::Ended) {}
    /// The project's check runs: before the turn's first change, or after
    /// a step's changes.
    fn checking(&mut self, _check: &crate::check::Check) {}
    /// What the check after a step's changes said, and `printed`, the
    /// errors it found, all of them, to read whole.
    fn checked(&mut self, _checked: &crate::check::Checked, _printed: &str) {}
    /// Does what the model asked of another rein (`peers`, `message`,
    /// `delegate`), and says how it went. Only offered where there are
    /// other reins to reach; see [`Agent::with_peer_tools`].
    fn peer(&mut self, call: &ToolCall) -> impl Future<Output = String> {
        std::future::ready(format!("{}there are no other reins to reach from here: {}", crate::tools::ERROR, call.name))
    }
    /// Asks you `questions`, and gives how you answered each, in order;
    /// `None` where nobody's there to answer.
    fn ask(&mut self, _questions: &[crate::ask::Question]) -> impl Future<Output = Option<Vec<crate::ask::Answer>>> {
        std::future::ready(None)
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

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// About how many tokens `text` takes, in words.
fn about_tokens(text: &str) -> String {
    match text.chars().count().div_ceil(4) {
        1 => "about 1 token".to_owned(),
        n => format!("about {n} tokens"),
    }
}

/// What the model asked for, before a hook rewrote it: a command's line.
fn was(call: &ToolCall) -> String {
    BashCall::of(call).map_or_else(|| "the model's own input".to_owned(), |bash| bash.command().to_owned())
}

/// Whether a tool's result says it failed.
pub fn failed(output: &str) -> bool {
    output.starts_with(crate::tools::ERROR) || exit_code(output).is_some_and(|code| code != 0)
}

/// The exit code a command's result ends with, if it does.
fn exit_code(output: &str) -> Option<i64> {
    output.rsplit_once("[exit code ").and_then(|(_, rest)| rest.split(']').next()?.parse().ok())
}

/// Runs `futures` at the same time; their outputs, in order.
pub(crate) async fn all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
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

/// The skills, as this conversation has them.
#[derive(Default)]
struct SkillsHere {
    /// Those found now: the model loads them with `skill`.
    found: crate::skills::Skills,
    /// Those the tool lists, as they were when it was made: `None` until
    /// it's made for this conversation.
    listed: Option<Vec<crate::skills::Skill>>,
    /// What the model was last told had changed since, if anything.
    noted: Option<String>,
    /// Those you called with `/name`, with what you gave them, to load
    /// with your next message.
    called: Vec<(String, Option<String>)>,
    /// Those loaded since you last asked, to count.
    loaded: Vec<String>,
}

/// What the model is told of skills, when there are some.
const SKILLS: &str = "# Skills\nThe skill tool lists skills: instructions for kinds of task, the user's own or the project's. \
                      Before you start a task, small ones too, check whether one fits it; if one does, load it first and \
                      follow it.";

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
/// What a call gets that a hook's stopping the turn kept from running.
pub const NOT_RUN_HOOK: &str = "Not run: one of the user's hooks stopped the turn.";

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

/// What's taking room in the conversation, in about how many tokens.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Room {
    pub tool_output: u64,
    pub reins_words: u64,
    pub your_messages: u64,
    pub project: u64,
    pub tools: u64,
    /// How many tools the model is shown, and how many it found.
    pub tools_shown: usize,
    pub tools_found: usize,
    pub instructions: u64,
    /// The biggest outputs, by what they were, and whether they're stale.
    pub biggest: Vec<(String, u64, bool)>,
    /// What retiring what's out of date would free.
    pub retiring: u64,
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
    /// A hook of yours said no to your message: it wasn't sent.
    Refused(By),
    /// A hook of yours stopped the turn (`"continue": false`).
    HookStopped(By),
}

/// The most times in a row hooks keep a turn going once the model is
/// done, as Claude Code's do: past it, the turn ends anyway.
const STOP_HOOKS_MOST: usize = 8;

/// How a step's calls went.
enum Ran {
    /// Each one ran, or was refused, declined or skipped.
    All,
    /// The user declined one and stopped the turn.
    Stopped,
    /// One came out the same as before, too many times in a row.
    Stuck { call: ToolCall, times: usize },
    /// A hook of yours stopped the turn.
    HookStopped(By),
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
    /// Whether the model can have tools held back until it finds them, as
    /// its list says: one that can't is shown every tool from the start.
    pub finds_tools: rein_llm::FindsTools,
    /// Whose tools the model is given: see [`Agent::set_harness`].
    harness: rein_llm::Harness,
    /// Who searches the web for the model, if anyone: `web_search` is
    /// offered only then (see [`Agent::set_searcher`]).
    searcher: Option<crate::web::Searcher>,
    /// Whether the model can see images, as its list says: one that can't
    /// is told of them in words.
    pub sees: rein_llm::Sees,
    /// Images you pasted, to go with your next message.
    pasted: Vec<rein_llm::Image>,
    /// The skills: those found, those the tool lists, and those called.
    skills: SkillsHere,
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
    /// What each MCP server whose tools are offered says of using them, by
    /// its name: added to the system prompt (see [`Agent::system`]).
    servers_say: Vec<(String, String)>,
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
    /// Your hooks: commands of yours run when something happens.
    hooks: std::sync::Arc<Hooks>,
    /// What hooks said as the session started, for the model with your
    /// next message.
    hooks_said: Vec<String>,
    /// A hook stopped the turn after a tool ran: it ends once the step does.
    hook_stopped: Option<By>,
    /// The project's check after edits, and what this turn's checks know.
    checking: crate::check::Checking,
    /// The step under way changed the project's files: it's checked once
    /// it's done.
    changed_in_step: bool,
}

impl<M: Model> Agent<M> {
    /// Adds the user's message and works on it until the model is done.
    /// Dropping the future cancels the turn; [`Agent::repair`] then makes
    /// the history valid again, and `send` calls it first anyway.
    pub async fn send(&mut self, prompt: String, ui: &mut impl Ui) -> Result<Outcome> {
        // A hook may say no to it: then it isn't sent, nor kept.
        let heard = self.hook(hooks::Event::UserPromptSubmit(&prompt), ui).await;
        if let Some(by) = heard.no.or(heard.stop) {
            return Ok(Outcome::Refused(by));
        }
        self.begin(&prompt).await?;
        self.offer_servers_tools(ui).await;
        self.note_skills()?;
        // What ended unannounced (one you stopped), before what you say of it.
        self.note_ended(ui).await?;
        self.steer(ui.waiting(), ui).await?;
        self.record(Item::User(prompt.clone()))?;
        self.attach(&prompt, ui).await?;
        self.hooks_add(&heard.context, ui)?;
        let worked = self.work(ui).await;
        if let Err(e) = &worked {
            self.turn_failed(e, ui).await;
        }
        worked
    }

    /// Runs your hooks on `event`, saying where you see it what they said
    /// for you, and, once each, those that didn't work.
    pub async fn hook(&self, event: hooks::Event<'_>, ui: &mut impl Ui) -> hooks::Heard {
        if !self.hooks.runs_on(&event, self.tools.root()) {
            return hooks::Heard::default();
        }
        let hooks = self.hooks.clone();
        let heard = hooks.on(event, &self.here()).await;
        for by in &heard.messages {
            ui.note(&format!("{}: {}", by.hook, first_line(&by.said)));
        }
        for by in &heard.failed {
            if hooks.first_time(by) {
                ui.note(&format!("your hook {} didn't work: {}", by.hook, first_line(&by.said)));
            }
        }
        heard
    }

    /// Where your hooks run, and what they're told of it.
    fn here(&self) -> hooks::Here<'_> {
        hooks::Here { root: self.tools.root(), session_id: self.session_id.as_deref(), transcript: self.session.as_ref().map(Session::path), mode: self.permissions.mode }
    }

    /// What your hooks added, for the model: after what's there, as their
    /// words, not yours, said where you see it.
    fn hooks_add(&mut self, added: &[By], ui: &mut impl Ui) -> Result<()> {
        for by in added {
            ui.note(&format!("{} added {}", by.hook, about_tokens(&by.said)));
            self.record(Item::User(hooks::note(by)))?;
        }
        Ok(())
    }

    /// Runs your hooks on a session's start, from `source`: what they add,
    /// the model is told with your next message.
    pub async fn session_started(&mut self, source: hooks::Source, ui: &mut impl Ui) {
        let heard = self.hook(hooks::Event::SessionStart(source), ui).await;
        for by in &heard.context {
            ui.note(&format!("{} added {}", by.hook, about_tokens(&by.said)));
            self.hooks_said.push(hooks::note(by));
        }
    }

    /// Runs your hooks on the session's end, as `ended`.
    pub async fn session_ended(&self, ended: hooks::Ended, ui: &mut impl Ui) {
        // What it started in the background goes with it.
        let stopped = self.tools.jobs().stop_all().await;
        if !stopped.is_empty() {
            let names: Vec<String> = stopped.iter().map(|listed| format!("{} ({})", listed.id, listed.command)).collect();
            let s = if names.len() == 1 { "" } else { "s" };
            ui.note(&format!("stopped the background command{s} still running: {}", names.join(", ")));
        }
        self.hook(hooks::Event::SessionEnd(ended), ui).await;
    }

    /// Works on what's ended in the background while nobody was talking,
    /// as [`Agent::send`] does on the user's message.
    pub async fn hear_ended(&mut self, ui: &mut impl Ui) -> Result<Outcome> {
        self.begin("[a background command ended]").await?;
        self.offer_servers_tools(ui).await;
        self.steer(ui.waiting(), ui).await?;
        if !self.note_ended(ui).await? {
            return Ok(Outcome::Done);
        }
        self.work(ui).await
    }

    /// How the project's check runs: in the sandbox's write walls when it's
    /// on, as a command that builds would.
    fn check_how(&self) -> crate::sandbox::Run {
        use crate::sandbox::Run;
        match self.permissions.sandboxing() {
            crate::permissions::Sandboxing::On => crate::sandbox::Access::Write.walls().map_or(Run::Unsandboxed, Run::Sandboxed),
            _ => Run::Unsandboxed,
        }
    }

    /// Looks at the errors there are before the turn's first change, once
    /// a turn.
    async fn check_baseline(&mut self, ui: &mut impl Ui) {
        if !self.checking.turn.wants_baseline() {
            return;
        }
        let Some(check) = self.checking.active().cloned() else { return };
        ui.checking(&check);
        match self.tools.check(&check, self.check_how()).await {
            crate::check::Ran::Done { problems, .. } => self.checking.turn.set_baseline(Some(problems)),
            // Not this turn, then: what's new can't be told without it.
            crate::check::Ran::Busy => self.checking.turn.set_baseline(None),
            ran => {
                self.checking.turn.set_baseline(None);
                self.check_paused(&check, &ran, ui);
            }
        }
    }

    /// Checks the project after a step that changed its files, and tells
    /// the model what the turn's changes broke, or that it's fixed.
    async fn check_after(&mut self, ui: &mut impl Ui) -> Result<()> {
        if !std::mem::take(&mut self.changed_in_step) {
            return Ok(());
        }
        let Some(check) = self.checking.active().cloned() else { return Ok(()) };
        if !self.checking.turn.has_baseline() {
            return Ok(());
        }
        ui.checking(&check);
        let ran = self.tools.check(&check, self.check_how()).await;
        match ran {
            crate::check::Ran::Done { problems, took, printed } => {
                if let Some((checked, note)) = self.checking.turn.after(&check, &problems, took) {
                    ui.checked(&checked, &self.tools.hidden(printed));
                    if let Some(note) = note {
                        self.record(Item::User(self.tools.hidden(note)))?;
                    }
                }
            }
            crate::check::Ran::Busy => {
                let checked = crate::check::Checked { name: check.name(), took: crate::check::BUDGET, verdict: crate::check::Verdict::Busy, shown: Vec::new(), before: 0 };
                ui.checked(&checked, "");
            }
            ran => self.check_paused(&check, &ran, ui),
        }
        Ok(())
    }

    /// The check was too slow, or wouldn't start: it's paused for the
    /// conversation, and you're told why, and how to run it again.
    fn check_paused(&mut self, check: &crate::check::Check, ran: &crate::check::Ran, ui: &mut impl Ui) {
        self.checking.pause();
        let why = match ran {
            crate::check::Ran::Slow => format!("`{}` took longer than {}s", check.name(), crate::check::BUDGET.as_secs()),
            crate::check::Ran::Couldnt(why) => why.clone(),
            _ => return,
        };
        ui.note(&format!("{why}, so rein stopped checking the project after edits in this conversation: /check on runs it again"));
    }

    /// Tells the model, once a turn, as it says it's done, of the errors
    /// its changes brought in that are still there: whether it did.
    fn note_still_broken(&mut self) -> Result<bool> {
        let Some(check) = self.checking.active().cloned() else { return Ok(false) };
        let Some(note) = self.checking.turn.at_end(&check) else { return Ok(false) };
        self.record(Item::User(self.tools.hidden(note)))?;
        Ok(true)
    }

    /// Tells the model of the background commands that ended since it was
    /// last told: whether there were any.
    async fn note_ended(&mut self, ui: &mut impl Ui) -> Result<bool> {
        let ended = self.tools.ended().await;
        for one in &ended {
            ui.background_ended(one);
            self.record(Item::User(one.note.clone()))?;
        }
        Ok(!ended.is_empty())
    }

    /// Runs your hooks on a turn that ended on `error`.
    async fn turn_failed(&self, error: &Error, ui: &mut impl Ui) {
        let kind = match error {
            Error::Auth(_) | Error::Api { status: Some(401 | 403), .. } => "authentication_failed",
            Error::Api { kind: ErrorKind::UsageLimit, .. } | Error::Api { status: Some(429), .. } => "rate_limit",
            Error::Api { status: Some(503 | 529), .. } => "overloaded",
            _ => "unknown",
        };
        self.hook(hooks::Event::StopFailure { kind, error: &error.to_string() }, ui).await;
    }

    /// What the model said last.
    fn last_said(&self) -> Option<String> {
        self.history.iter().rev().find_map(|item| match item {
            Item::Assistant(text) => Some(text.clone()),
            _ => None,
        })
    }

    /// Works on what another rein, `from`, sent, as [`Agent::send`] does
    /// on the user's message: framed as not the user's, and nothing it
    /// points at with `@` is read.
    pub async fn hear(&mut self, from: &str, text: &str, ui: &mut impl Ui) -> Result<Outcome> {
        self.begin(&format!("{from}: {text}")).await?;
        self.offer_servers_tools(ui).await;
        self.note_ended(ui).await?;
        self.steer(ui.waiting(), ui).await?;
        self.record(Item::User(crate::peers::heard(from, text)))?;
        self.work(ui).await
    }

    /// What every turn starts with: the history made valid again, a turn
    /// of its own for `/undo`, started by `said`, and the context, before
    /// the first message.
    async fn begin(&mut self, said: &str) -> Result<()> {
        self.repair();
        self.checking.turn = crate::check::Turn::default();
        self.tools.turn(said).await;
        if let Some(context) = self.context.take() {
            if let Some(session) = &mut self.session {
                session.append_context(&context)?;
            }
            self.window.full.push(Item::User(context.clone()));
            self.history.push(Item::User(context));
        }
        for said in std::mem::take(&mut self.hooks_said) {
            self.record(Item::User(said))?;
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
        self.servers_say = listing.instructions;
        self.specs.retain(|tool| !rein_mcp::is_mcp(&tool.name) && tool.loading != Loading::Search);
        // A server's schema is as it wrote it, not to strict mode's rules.
        let loading = |held_back: bool| if held_back { Loading::Found } else { Loading::UpFront };
        self.specs.extend(listing.tools.into_iter().map(|tool| Tool {
            grammar: None,
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
            strict: false,
            loading: loading(tool.held_back),
        }));
        // Last, so what comes before it in every request stays the same.
        self.specs.extend(finding::spec(&self.specs));
        self.permissions.allow_tools(listing.allowed);
        self.listed = true;
        if let Some(note) = listing.note {
            ui.note(&note);
        }
    }

    /// Searches the web for the model with `searcher`, or with nothing:
    /// `web_search` is offered from the next request, or taken away. Set
    /// before the first request, it changes nothing that's cached.
    pub fn set_searcher(&mut self, searcher: Option<crate::web::Searcher>) {
        let named = ToolName::WebSearch.as_str();
        self.specs.retain(|tool| tool.name != named);
        if searcher.is_some() {
            // With rein's own, before the servers' and the tool search.
            let at = self.specs.iter().position(|tool| rein_mcp::is_mcp(&tool.name) || tool.loading == Loading::Search).unwrap_or(self.specs.len());
            self.specs.insert(at, crate::web::spec());
        }
        self.searcher = searcher;
    }

    /// The skills the model may load with `skill`, and you with `/name`.
    /// The tool's list of them is made once for a conversation, before its
    /// first request, in `room` characters; from then on what changes is
    /// told at the end of the conversation, with your next message, so
    /// what's cached stays. Only a list that had none to offer, and has
    /// some now, is made again.
    pub fn set_skills(&mut self, skills: crate::skills::Skills, room: usize) {
        let named = ToolName::Skill.as_str();
        let offered = || skills.found.iter().filter(|skill| skill.model).cloned().collect::<Vec<_>>();
        let none_listed = !self.specs.iter().any(|tool| tool.name == named);
        if self.skills.listed.is_none() || (none_listed && !offered().is_empty()) {
            self.specs.retain(|tool| tool.name != named);
            if let Some(spec) = crate::skills::spec(&skills, room) {
                let at = self.specs.iter().position(|tool| rein_mcp::is_mcp(&tool.name) || tool.loading == Loading::Search).unwrap_or(self.specs.len());
                self.specs.insert(at, spec);
            }
            self.skills.listed = Some(offered());
            self.skills.noted = None;
        }
        self.skills.found = skills;
    }

    /// The skills found, and those left out.
    pub fn skills(&self) -> &crate::skills::Skills {
        &self.skills.found
    }

    /// The skill `name`, which you called, goes with your next message,
    /// given `arguments`.
    pub fn call_skill(&mut self, name: &str, arguments: Option<String>) {
        self.skills.called.push((name.to_owned(), arguments));
    }

    /// The skills loaded, by the model or for you, since this was last
    /// asked: how often each is used.
    pub fn take_loaded(&mut self) -> Vec<String> {
        std::mem::take(&mut self.skills.loaded)
    }

    /// Tells the model what's changed of the skills since its tool listed
    /// them, when that's not what it was last told: at the end of the
    /// conversation, where it costs nothing cached.
    fn note_skills(&mut self) -> Result<()> {
        let Some(listed) = &self.skills.listed else { return Ok(()) };
        let now = crate::skills::changes(listed, &self.skills.found);
        if now == self.skills.noted {
            return Ok(());
        }
        let said = now.clone().unwrap_or_else(|| crate::skills::AS_LISTED.to_owned());
        self.record(Item::User(said))?;
        self.skills.noted = now;
        Ok(())
    }

    /// Who searches the web for the model, if anyone.
    pub fn searcher(&self) -> Option<&crate::web::Searcher> {
        self.searcher.as_ref()
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
        // How many times hooks kept the turn going once the model was done.
        let mut kept_going = 0;
        loop {
            let may_compact = !no_room && compacted_at != Some(steps);
            if may_compact && self.estimated() > self.window.limit {
                self.compacting(hooks::Trigger::Auto, None, false, ui).await?;
                (compacted_at, no_room) = (Some(steps), self.estimated() > self.window.limit);
            }
            let system = self.system();
            let request = self.request(&self.history, system.as_deref());
            let sent = self.history.len();
            ui.thinking();
            let (items, usage) = match self.model.respond(&request, &mut |streamed| stream_to(ui, streamed)).await {
                // The provider says it's too long after all: make room and
                // try again.
                Err(e) if !no_room && compacted_at != Some(steps) && e.kind() == Some(ErrorKind::ContextFull) => {
                    self.compacting(hooks::Trigger::Auto, None, true, ui).await?;
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
                // Or a command ended in the background.
                let ended = self.note_ended(ui).await?;
                // Or it broke the build and didn't say: it's told once.
                let broken = self.note_still_broken()?;
                if self.steer(ui.steering(), ui).await? || ended || broken {
                    continue;
                }
                // Or a hook of yours says the work isn't done.
                let last = self.last_said();
                let heard = self.hook(hooks::Event::Stop { again: kept_going > 0, last: last.as_deref() }, ui).await;
                if let Some(by) = heard.stop {
                    return Ok(Outcome::HookStopped(by));
                }
                let Some(by) = heard.no.or_else(|| heard.context.into_iter().next()) else { return Ok(Outcome::Done) };
                if kept_going >= STOP_HOOKS_MOST {
                    ui.note(&format!("{} still says the work isn't done; hooks keep a turn going {STOP_HOOKS_MOST} times at most", by.hook));
                    return Ok(Outcome::Done);
                }
                kept_going += 1;
                ui.note(&format!("{} says the work isn't done: {}", by.hook, first_line(&by.said)));
                self.record(Item::User(hooks::not_done(&by)))?;
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
                Ran::HookStopped(by) => return Ok(Outcome::HookStopped(by)),
            }
            if let Some(by) = self.hook_stopped.take() {
                return Ok(Outcome::HookStopped(by));
            }
            // What the step's changes broke, what ended in the background,
            // and what the user wrote, during the last step goes with its
            // result.
            self.check_after(ui).await?;
            self.note_ended(ui).await?;
            self.steer(ui.steering(), ui).await?;
        }
    }

    /// Runs one step's `calls`, lookups asked for together at once, and
    /// records each result. On the turn's `last` step, the last result tells
    /// the model so. Your hooks run before each, and after.
    async fn run_step(&mut self, calls: &[ToolCall], last: bool, repeats: &mut Repeats, ui: &mut impl Ui) -> Result<Ran> {
        let closing = if last { calls.last().map(|call| call.call_id.clone()) } else { None };
        self.changed_in_step = false;
        // The errors there are before the turn changes anything: what a
        // check after it finds beyond them is what the turn broke.
        if calls.iter().any(may_change_files) {
            self.check_baseline(ui).await;
        }
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
            // Lookups asked for together run at once: they only look. One
            // a hook runs before goes on its own.
            let together = calls[next..]
                .iter()
                .take_while(|call| {
                    ToolName::of(call).is_some_and(ToolName::only_looks)
                        && self.permissions.decide(call) == Decision::Allow
                        && !self.hooks.runs_on(&hooks::Event::PreToolUse(call), self.tools.root())
                })
                .count();
            if together > 1 {
                let batch = &calls[next..next + together];
                ui.tools_started(batch);
                let began = std::time::Instant::now();
                let outputs = all(batch.iter().map(|call| self.tools.run(call, self.permissions.how(call))).collect()).await;
                for (i, (call, output)) in batch.iter().zip(outputs).enumerate() {
                    let output = self.after(call, output, began, &[], ui).await;
                    if let Some(stuck) = self.finish(call, output, repeats, closing.as_deref(), ui)? {
                        return self.stop_stuck(stuck, &calls[next + i + 1..], ui);
                    }
                }
                next += together;
                continue;
            }
            let original = calls[next].clone();
            next += 1;
            // Your hooks first: one may say no, change the call, or add to
            // what the model is told of it.
            let pre = self.hook(hooks::Event::PreToolUse(&original), ui).await;
            if let Some(by) = pre.stop {
                let output = format!("{}one of the user's hooks, {}, stopped the turn: {}", crate::tools::ERROR, by.hook, by.said);
                ui.tool_finished(&original, &output);
                self.record(Item::ToolResult { call_id: original.call_id.clone(), output })?;
                for left in &calls[next..] {
                    ui.tool_finished(left, NOT_RUN_HOOK);
                    self.record(Item::ToolResult { call_id: left.call_id.clone(), output: NOT_RUN_HOOK.to_owned() })?;
                }
                return Ok(Ran::HookStopped(by));
            }
            if let Some(by) = &pre.no {
                let reason = format!("{}one of the user's hooks, {}, said no: {}", crate::tools::ERROR, by.hook, by.said);
                ui.tool_blocked(&original, &reason);
                if let Some(stuck) = self.finish(&original, reason, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            let call = match &pre.call {
                Some((rewritten, by)) => {
                    ui.hooked(rewritten, &format!("{} rewrote it, from: {}", by.join(" and "), was(&original)));
                    rewritten.clone()
                }
                None => original.clone(),
            };
            let began = std::time::Instant::now();
            // Other reins are reached by whoever runs the agent. Nothing
            // runs here: what the other rein does, it asks its own user about.
            if ToolName::of(&call).is_some_and(ToolName::reaches_peers) {
                ui.tool_started(&call);
                let output = ui.peer(&call).await;
                let output = self.after(&call, output, began, &pre.context, ui).await;
                if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            // Asking you is the user's side's to do: the model waits.
            if ToolName::of(&call).is_some_and(ToolName::asks_user) {
                ui.tool_started(&call);
                let output = match crate::ask::questions(&call) {
                    Ok(questions) => match ui.ask(&questions).await {
                        Some(answers) => crate::ask::told(&call, &questions, &answers),
                        None => crate::ask::NOBODY.to_owned(),
                    },
                    Err(why) => format!("{}{why}", crate::tools::ERROR),
                };
                let output = self.after(&call, output, began, &pre.context, ui).await;
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
                let output = self.after(&call, output, began, &pre.context, ui).await;
                if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            // And searching the web, with the model chosen for it: as far
            // as the mode lets a command reach.
            if ToolName::of(&call) == Some(ToolName::WebSearch) {
                ui.tool_started(&call);
                let web = if self.permissions.mode == crate::permissions::Mode::Trust { rein_llm::search::Web::Live } else { rein_llm::search::Web::Cached };
                let output = match &self.searcher {
                    Some(searcher) => searcher.run(&call.arguments, web).await,
                    None => format!("{}nothing searches the web here now", crate::tools::ERROR),
                };
                let output = self.after(&call, output, began, &pre.context, ui).await;
                if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            // And loading a skill: what it says, from its own folder.
            if ToolName::of(&call) == Some(ToolName::Skill) {
                ui.tool_started(&call);
                let output = self.skill_output(&call.arguments);
                let output = self.after(&call, output, began, &pre.context, ui).await;
                if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            // So is finding the tools held back: what it finds, the model
            // is shown from then on, as its provider loads them.
            if ToolName::of(&call) == Some(ToolName::ToolSearch) {
                ui.tool_started(&call);
                let found = finding::search(&self.specs, &call.arguments);
                if let Some(stuck) = self.found(&call, found, repeats, closing.as_deref(), ui)? {
                    return self.stop_stuck(stuck, &calls[next..], ui);
                }
                continue;
            }
            let approved = match self.decide_hooked(&call, &original, &pre) {
                Decision::Allow => Ok(()),
                Decision::Deny(reason) => {
                    ui.tool_blocked(&call, &reason);
                    Err(reason)
                }
                Decision::Ask => {
                    // Your hooks on a question may answer it: never over
                    // an ask rule of rein's.
                    let asked = self.hook(hooks::Event::PermissionRequest(&call), ui).await;
                    let ruled = self.permissions.asked_by(&call).is_some() || self.permissions.asked_by(&original).is_some();
                    if let Some(by) = asked.no {
                        let reason = format!("{}one of the user's hooks, {}, said no: {}", crate::tools::ERROR, by.hook, by.said);
                        ui.tool_blocked(&call, &reason);
                        Err(reason)
                    } else if asked.yes.is_some() && !ruled {
                        Ok(())
                    } else {
                        if let Some(by) = &pre.ask {
                            ui.note(&format!("{} asks you first: {}", by.hook, first_line(&by.said)));
                        }
                        if ui.away() {
                            let message = format!("rein needs your permission to use {}", hooks::tool_name(&call));
                            self.hook(hooks::Event::Notification(hooks::Notice::Permission, &message), ui).await;
                        }
                        let rule = self.permissions.asked_by(&call).or_else(|| self.permissions.asked_by(&original));
                        match ui.approve(&call, rule).await {
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
                        }
                    }
                }
            };
            let output = match approved {
                Ok(()) => {
                    let began = std::time::Instant::now();
                    ui.tool_started(&call);
                    let output = self.tools.run(&call, self.permissions.how(&call)).await;
                    if let Some(watched) = self.tools.take_watched() {
                        self.changed_in_step |= matches!(&watched, Watched::Changed(changed) if !changed.is_empty());
                        ui.watched(&call, &watched);
                    }
                    self.changed_in_step |= ToolName::of(&call).is_some_and(ToolName::changes_files) && !output.starts_with(crate::tools::ERROR);
                    self.after(&call, output, began, &pre.context, ui).await
                }
                Err(reason) => reason,
            };
            if let Some(stuck) = self.finish(&call, output, repeats, closing.as_deref(), ui)? {
                return self.stop_stuck(stuck, &calls[next..], ui);
            }
        }
        Ok(Ran::All)
    }

    /// How rein decides on `call`, as your hooks before it left it from
    /// `original`, the model's: as on the model's own, and rein's ask and
    /// deny rules on the original too, so a rewrite can't get past them. A
    /// hook may make rein ask; one of yours saying yes spares a question,
    /// but never one an ask rule of rein's asks.
    fn decide_hooked(&self, call: &ToolCall, original: &ToolCall, pre: &hooks::Heard) -> Decision {
        let mut decision = self.permissions.decide(call);
        if call.arguments != original.arguments {
            if let Decision::Deny(why) = self.permissions.decide(original) {
                return Decision::Deny(why);
            }
            if decision == Decision::Allow && self.permissions.asked_by(original).is_some() {
                decision = Decision::Ask;
            }
        }
        let ruled = self.permissions.asked_by(call).is_some() || self.permissions.asked_by(original).is_some();
        match decision {
            Decision::Allow if pre.ask.is_some() => Decision::Ask,
            Decision::Ask if pre.yes.is_some() && pre.ask.is_none() && !ruled => Decision::Allow,
            other => other,
        }
    }

    /// What `call` gave, `output`, as your hooks after it leave it: one may
    /// put another result in its place, add to what the model is told
    /// (`before`, what hooks before it added, too), or stop the turn.
    async fn after(&mut self, call: &ToolCall, output: String, began: std::time::Instant, before: &[By], ui: &mut impl Ui) -> String {
        let failed = failed(&output);
        let heard = self.hook(hooks::Event::PostToolUse { call, output: &output, took: began.elapsed(), failed }, ui).await;
        let mut output = output;
        if let Some(by) = heard.output {
            ui.hooked(call, &format!("{} gave the model another result", by.hook));
            output = by.said;
        }
        for by in before.iter().chain(&heard.context).chain(&heard.no) {
            ui.hooked(call, &format!("{} added {}", by.hook, about_tokens(&by.said)));
            output.push('\n');
            output.push_str(&hooks::note(by));
        }
        if let Some(by) = heard.stop {
            self.hook_stopped = Some(by);
        }
        output
    }

    /// Records what `call` gave, noting when it has come out the same many
    /// times in a row, and when it's the last of the turn's last step.
    /// `Some` when it came out the same too many times: the turn stops.
    fn finish(&mut self, call: &ToolCall, output: String, repeats: &mut Repeats, closing: Option<&str>, ui: &mut impl Ui) -> Result<Option<Ran>> {
        let times = repeats.add(call, &output);
        let output = with_note(output, call, closing, times);
        ui.tool_finished(call, &output);
        self.record(Item::ToolResult { call_id: call.call_id.clone(), output })?;
        self.record_images(&call.call_id)?;
        Ok((times >= REPEATS_STOP).then(|| Ran::Stuck { call: call.clone(), times }))
    }

    /// Records what the tool search found for `call`, or why it found
    /// nothing, as [`Agent::finish`] records a tool's result. What rein
    /// has to tell the model with it, it can only say in words: then the
    /// finding is said in words too, and loads nothing.
    fn found(&mut self, call: &ToolCall, found: std::result::Result<Vec<String>, String>, repeats: &mut Repeats, closing: Option<&str>, ui: &mut impl Ui) -> Result<Option<Ran>> {
        let names = match found {
            Ok(names) => names,
            Err(why) => return self.finish(call, why, repeats, closing, ui),
        };
        let times = repeats.add(call, &rein_llm::found_text(&names));
        ui.tool_finished(call, &finding::said(&names));
        if times >= REPEATS_NOTED || closing == Some(call.call_id.as_str()) {
            let output = with_note(rein_llm::found_text(&names), call, closing, times);
            self.record(Item::ToolResult { call_id: call.call_id.clone(), output })?;
        } else {
            self.record(Item::Found { call_id: call.call_id.clone(), names })?;
        }
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
    /// Says whether there was a message, which keeps the turn going; what
    /// another rein told while no turn ran doesn't.
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
                Heard::Peer { from, text } | Heard::Reported { from, text } => {
                    any = true;
                    ui.steered(&heard);
                    self.record(Item::User(crate::peers::heard(from, text)))?;
                }
                Heard::Told { from, text } => {
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
            Item::ToolCall(call) if ToolName::of(call) == Some(ToolName::ApplyPatch) => {
                crate::tools::patch_of(call).is_ok_and(|patch| patch.paths().iter().any(|p| p.trim_start_matches("./") == path))
            }
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
        // A skill you called comes as a load made for you.
        for (name, given) in std::mem::take(&mut self.skills.called) {
            let arguments = json!({ "name": name, "arguments": given, "file": null, "start_line": null }).to_string();
            let call = ToolCall { call_id: format!("mention_{}", self.window.full.len()), name: ToolName::Skill.as_str().into(), arguments };
            let output = self.skill_output(&call.arguments);
            self.attached(call, output, ui)?;
        }
        // What you pasted comes as a read made for you, as what you name
        // with @ does: an image is always some call's.
        for image in std::mem::take(&mut self.pasted) {
            let call = self.mention_call("read", json!({ "path": "(pasted)", "start_line": null, "line_count": null }));
            let call_id = call.call_id.clone();
            self.attached(call, format!("[{}]", image.about), ui)?;
            self.record(Item::Image { call_id, image })?;
        }
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
        self.record(Item::ToolResult { call_id: call_id.clone(), output })?;
        self.record_images(&call_id)
    }

    /// `image`, which you pasted, goes with your next message.
    pub fn paste(&mut self, image: rein_llm::Image) {
        self.pasted.push(image);
    }

    /// What loading a skill as `arguments` ask gives, or that it's as it
    /// was when it was loaded before: it's in the conversation already. A
    /// skill's instructions loaded count as a use.
    fn skill_output(&mut self, arguments: &str) -> String {
        let output = crate::skills::load(&self.skills.found, arguments);
        let before = self.history.iter().any(|item| matches!(item, Item::ToolResult { output: earlier, .. } if *earlier == output));
        if let Ok(asked) = serde_json::from_str::<Value>(arguments)
            && !output.starts_with(crate::tools::ERROR)
            && asked["file"].is_null()
            && asked["start_line"].is_null()
            && let Some(name) = asked["name"].as_str()
        {
            self.skills.loaded.push(name.to_owned());
        }
        match serde_json::from_str::<Value>(arguments) {
            Ok(asked) if before && !output.starts_with(crate::tools::ERROR) => {
                format!(
                    "{} the skill {} is as it was when it was loaded earlier in this conversation, so that copy is current.]",
                    compact::UNCHANGED,
                    asked["name"].as_str().unwrap_or_default()
                )
            }
            _ => output,
        }
    }

    /// Records the images the call `call_id` gave, right after its result.
    fn record_images(&mut self, call_id: &str) -> Result<()> {
        for image in self.tools.take_images(call_id) {
            self.record(Item::Image { call_id: call_id.to_owned(), image })?;
        }
        Ok(())
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
        self.compacting(hooks::Trigger::Manual, focus, summarize, ui).await
    }

    /// Compacts as [`Agent::compact`] does, as `trigger` asked, with your
    /// hooks before and after: one may say no to a compaction you asked for
    /// (not to one the conversation needs); and what they add once it's
    /// done, as a session starting again from the summary, the model is
    /// told.
    async fn compacting(&mut self, trigger: hooks::Trigger, focus: Option<&str>, summarize: bool, ui: &mut impl Ui) -> Result<Compacted> {
        let heard = self.hook(hooks::Event::PreCompact { trigger, focus }, ui).await;
        if let Some(by) = heard.no {
            if trigger == hooks::Trigger::Manual {
                ui.note(&format!("{} said no to compacting: {}", by.hook, first_line(&by.said)));
                let now = self.estimated();
                return Ok(Compacted { before: now, after: now, retired: 0, summarized: false });
            }
            ui.note(&format!("{} said no to compacting, but the conversation doesn't fit: compacting anyway", by.hook));
        }
        let report = self.compact_now(focus, summarize, ui).await?;
        let summary = self.history.iter().find(|item| compact::is_summary(item)).and_then(|item| match item {
            Item::User(text) => Some(text.clone()),
            _ => None,
        });
        self.hook(hooks::Event::PostCompact { trigger, summary: summary.as_deref().unwrap_or_default() }, ui).await;
        let heard = self.hook(hooks::Event::SessionStart(hooks::Source::Compact), ui).await;
        self.hooks_add(&heard.context, ui)?;
        Ok(report)
    }

    /// About how many tokens the next request will carry: the provider's
    /// count for the last one, plus an estimate for what was added since.
    /// Without a count, all of it is estimated, with the tools the request
    /// carries: not those held back until they're found. Counted, a new
    /// conversation with Linear's, Chrome's and Playwright's servers was
    /// compacted before its first request, at 49k of a 30k limit.
    pub fn estimated(&self) -> u64 {
        let added = |items: &[Item]| items.iter().map(|item| compact::estimate(compact::size(item))).sum::<u64>();
        match self.window.measured {
            Some((tokens, sent)) if sent <= self.history.len() => tokens + added(&self.history[sent..]),
            _ => {
                let tools: usize = self.tools_sent(&self.tools_found()).iter().map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len()).sum();
                let instructions = self.system().map_or(0, |system| system.len());
                compact::estimate(tools + instructions) + added(&self.history)
            }
        }
    }

    /// The tools held back that have been found so far.
    fn tools_found(&self) -> std::collections::HashSet<&str> {
        self.history
            .iter()
            .filter_map(|item| match item {
                Item::Found { names, .. } => Some(names.iter().map(String::as_str)),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// The tools a request carries: those held back only once `found`, and
    /// only where the format holds them back.
    fn tools_sent(&self, found: &std::collections::HashSet<&str>) -> Vec<&Tool> {
        let held = self.model.holds_back(&self.request(&[], None));
        let sent = |tool: &&Tool| match tool.loading {
            Loading::UpFront => true,
            Loading::Found => !held || found.contains(tool.name.as_str()),
            Loading::Search => held,
        };
        self.specs().iter().filter(sent).collect()
    }

    /// What's taking room in the conversation, as [`Agent::estimated`] counts it.
    pub fn room(&self) -> Room {
        let found = self.tools_found();
        let shown = self.tools_sent(&found);
        let tools: usize = shown.iter().map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len()).sum();
        let made = compact::made_of(&self.history);
        Room {
            tool_output: compact::estimate(made.tool_output),
            reins_words: compact::estimate(made.reins_words),
            your_messages: compact::estimate(made.your_messages),
            project: compact::estimate(made.project),
            tools: compact::estimate(tools),
            tools_shown: shown.len(),
            tools_found: found.len(),
            instructions: compact::estimate(self.system().map_or(0, |system| system.len())),
            biggest: compact::biggest(&self.history, 4).into_iter().map(|one| (one.what, compact::estimate(one.size), one.stale)).collect(),
            retiring: self.retiring(),
        }
    }

    /// What retiring what's out of date would free.
    fn retiring(&self) -> u64 {
        compact::stale(&self.history)
            .iter()
            .map(|(id, text)| {
                let old = self.history.iter().find_map(|item| match item {
                    Item::ToolResult { call_id, output } if call_id == id => Some(output.len()),
                    _ => None,
                });
                compact::estimate(old.unwrap_or(0).saturating_sub(text.len()))
            })
            .sum()
    }

    async fn compact_now(&mut self, focus: Option<&str>, summarize: bool, ui: &mut impl Ui) -> Result<Compacted> {
        ui.compacting();
        let before = self.estimated();
        let retired = compact::stale(&self.history);
        let freed = self.retiring();
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
            let system = self.system();
            match self.model.respond(&self.request(&input, system.as_deref()), &mut |streamed| stream_to(ui, streamed)).await {
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
        let mut record = compact::record(&self.window.full);
        // What still runs, so it isn't started again: a second dev server
        // after compaction, in Claude Code.
        let running: Vec<String> =
            self.tools.jobs().list().into_iter().filter(|listed| listed.ended.is_none()).map(|listed| format!("- {}: `{}`", listed.id, listed.command)).collect();
        if !running.is_empty() {
            record.push_str(&format!("Still running in the background, so don't start them again (read or stop them with background):\n{}\n", running.join("\n")));
        }
        let message = compact::summary_message(&summary.join("\n"), &record);

        let pinned = usize::from(self.history.first().is_some_and(context::is_context));
        // Keep less word for word when the limit is small (as in tests).
        let budget = compact::KEEP_RECENT.min(self.window.limit / 5);
        let start = compact::recent_start(&self.history, pinned, budget);
        let mut kept = self.history[start..].to_vec();
        if let Some(again) = compact::found_again(&self.history[..start], &kept, format!("found_again_{}", self.window.full.len())) {
            kept.splice(0..0, again);
        }
        let skills = compact::skills_again(&self.history[..start], &kept, &format!("skill_again_{}", self.window.full.len()));
        kept.splice(0..0, skills);
        // What changed of the skills, if the summary took the note away.
        if let Some(note) = &self.skills.noted
            && !kept.iter().any(|item| matches!(item, Item::User(text) if text == note))
        {
            kept.insert(0, Item::User(note.clone()));
        }
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
        tools.open_undo(&header.id);
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
            finds_tools: rein_llm::FindsTools::Unknown,
            harness: rein_llm::Harness::Rein,
            searcher: None,
            sees: rein_llm::Sees::Unknown,
            pasted: Vec::new(),
            skills: SkillsHere::default(),
            step_limit: None,
            instructions: header.instructions,
            servers_say: Vec::new(),
            session_id: Some(header.id),
            history: conversation.history,
            session: Some(conversation.session),
            context,
            window: Window { limit, measured: None, full: conversation.full },
            hooks: std::sync::Arc::default(),
            hooks_said: Vec::new(),
            hook_stopped: None,
            checking: crate::check::Checking::default(),
            changed_in_step: false,
        }
    }

    /// Moves to another conversation. The settings stay; what was only
    /// true of the last one goes with it: what you said "always" to, and
    /// its turns to undo, as this one's come back if it kept any.
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
        self.tools.open_undo(&self.session_id.clone().unwrap_or_default());
        // A new conversation, a new list: servers that started late, or
        // list other tools now, are in it, and so are the skills there are.
        self.listed = false;
        self.skills.listed = None;
        self.skills.noted = None;
        self.skills.called.clear();
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
            finds_tools,
            harness,
            searcher,
            sees,
            pasted,
            skills,
            step_limit,
            specs,
            listed,
            instructions,
            servers_say,
            session_id,
            history,
            session,
            context,
            window,
            hooks,
            hooks_said,
            hook_stopped,
            checking,
            changed_in_step,
        } = self;
        Agent {
            model,
            tools,
            permissions,
            model_name,
            reasoning_effort,
            efforts,
            max_output,
            finds_tools,
            harness,
            searcher,
            sees,
            pasted,
            skills,
            step_limit,
            specs,
            listed,
            instructions,
            servers_say,
            session_id,
            history,
            session,
            context,
            window,
            hooks,
            hooks_said,
            hook_stopped,
            checking,
            changed_in_step,
        }
    }

    /// The check that runs after a step changes the project's files, or
    /// none: see [`crate::check`].
    pub fn set_check(&mut self, check: Option<crate::check::Check>) {
        self.checking.set(check);
    }

    /// The project's check, and whether it's paused.
    pub fn checking(&self) -> &crate::check::Checking {
        &self.checking
    }

    /// The hooks that run from now on: yours, Claude Code's and its
    /// plugins', as you chose them.
    pub fn set_hooks(&mut self, hooks: std::sync::Arc<Hooks>) {
        self.hooks = hooks;
    }

    /// The hooks there are.
    pub fn hooks(&self) -> &Hooks {
        &self.hooks
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

    /// Gives the model the tools of the harness it was trained in: Codex's
    /// `apply_patch` for its models, in place of `write` and `edit`, and
    /// its `request_user_input` in place of `ask`, each in their place in
    /// the list. Only when that changes: the tool list comes first in a
    /// request, and a change loses the whole cache.
    pub fn set_harness(&mut self, harness: rein_llm::Harness) {
        if self.harness == harness {
            return;
        }
        self.harness = harness;
        let swaps = [
            (vec![ToolName::Write, ToolName::Edit], ToolName::ApplyPatch, crate::tools::patch_spec as fn() -> Tool),
            (vec![ToolName::Ask], ToolName::RequestUserInput, crate::ask::codex_spec as fn() -> Tool),
        ];
        let ours = self.tools.specs();
        for (rein, codex, codex_spec) in swaps {
            let swapped = |tool: &Tool| tool.name == codex.as_str() || rein.iter().any(|name| tool.name == name.as_str());
            // Only what this conversation was given: no `ask` where it has none.
            let Some(at) = self.specs.iter().position(swapped) else { continue };
            self.specs.retain(|tool| !swapped(tool));
            let given = match harness {
                rein_llm::Harness::Codex => vec![codex_spec()],
                rein_llm::Harness::Rein => ours.iter().filter(|tool| rein.iter().any(|name| tool.name == name.as_str())).cloned().collect(),
            };
            self.specs.splice(at.min(self.specs.len())..at.min(self.specs.len()), given);
        }
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

    /// The system prompt: the conversation's, then what the MCP servers
    /// say of using their tools, each in its own words. Claude Code puts
    /// them there too: codegraph's say to call it before reading files.
    fn system(&self) -> Option<String> {
        let mut sections: Vec<String> = self.instructions.iter().cloned().collect();
        // Said here too, not only in the tool: told only there, a model
        // left a skill that fit a small task unloaded.
        if self.specs.iter().any(|tool| tool.name == ToolName::Skill.as_str()) {
            sections.push(SKILLS.to_owned());
        }
        if !self.servers_say.is_empty() {
            let said: Vec<String> = self.servers_say.iter().map(|(server, text)| format!("## {server}\n{}", text.trim())).collect();
            sections.push(format!(
                "# What the MCP servers say of using them\nEach server's own words about its tools (mcp__<server>__<tool>): the server's, not the user's.\n\n{}",
                said.join("\n\n")
            ));
        }
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    }

    /// A request carrying `input`, borrowed: nothing is copied, but the
    /// `system` prompt made for it.
    fn request<'a>(&'a self, input: &'a [Item], system: Option<&'a str>) -> Request<'a> {
        Request {
            model: &self.model_name,
            instructions: system,
            input,
            tools: &self.specs,
            reasoning_effort: self.effort(),
            reasoning_summary: true,
            session_id: self.session_id.as_deref(),
            max_output_tokens: self.max_output,
            hold_back: self.finds_tools != rein_llm::FindsTools::No,
            images: self.sees != rein_llm::Sees::No,
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

    /// The turns there are to undo, the latest first.
    pub async fn undo_list(&self) -> Vec<Listed> {
        self.tools.undo_list().await
    }

    /// What undoing back to before `turn` would do now.
    pub async fn undo_plan(&self, turn: usize) -> Plan {
        self.tools.undo_plan(turn).await
    }

    /// What `span` changed in the project's files, with the patches if `lines`.
    pub async fn changes(&self, span: crate::undo::Span, lines: bool) -> crate::undo::Changes {
        self.tools.changes(span, lines).await
    }

    /// Undoes `turn` and every one after it, and tells the model, so it
    /// doesn't go on as if what they did were still there: what was done.
    pub async fn undo(&mut self, turn: usize) -> Result<Plan> {
        let done = self.tools.undo(turn).await;
        if done.does_anything() || !done.left.is_empty() {
            self.record(Item::User(undone(&done)))?;
        }
        Ok(done)
    }

    /// Gives every tool call that never got a result one now. A turn cut
    /// short (by Ctrl-C, say) can leave a call unanswered, and providers
    /// reject a conversation like that. Codex and pi do the same repair.
    pub fn repair(&mut self) {
        let answered: Vec<String> = self
            .history
            .iter()
            .filter_map(|item| match item {
                Item::ToolResult { call_id, .. } | Item::Found { call_id, .. } => Some(call_id.clone()),
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
        Streamed::Meters(meters) => ui.meters(meters),
    }
}

/// How long to wait before each retry of a failed request.
const RETRY_WAITS: [u64; 3] = [1, 2, 4];

impl Model for Llm {
    fn holds_back(&self, request: &Request<'_>) -> bool {
        self.client.holds_back(request)
    }

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
                Event::Meters(meters) => on(Streamed::Meters(&meters)),
                Event::Item(item) => items.push(item),
                Event::Completed { usage } => return Ok((items, usage)),
            }
        }
        // The stream only ends after `Completed` or an error, both handled above.
        Err(Error::Interrupted)
    }
}

/// How rein's note of an undo starts.
const UNDONE: &str = "[rein: the user undid what ";

/// `text` is rein's note of an undo, not something the user said.
pub(crate) fn is_undo_note(text: &str) -> bool {
    text.starts_with(UNDONE)
}

/// What the model is told when the user undoes its turns: what's back,
/// and what was left, so it doesn't go on as if its work were still there.
fn undone(done: &Plan) -> String {
    let list = |paths: &[String]| paths.join(", ");
    let turns = if done.turns == 1 { "your last turn".to_owned() } else { format!("your last {} turns", done.turns) };
    let mut told = format!("{UNDONE}{turns} did to the project's files, back to before \"{}\".", done.said);
    if !done.put_back.is_empty() {
        told.push_str(&format!(" Back as they were: {}.", list(&done.put_back)));
    }
    if !done.take_away.is_empty() {
        told.push_str(&format!(" Gone again, as they were made then: {}.", list(&done.take_away)));
    }
    if !done.left.is_empty() {
        told.push_str(&format!(" Changed since, so left as they are: {}.", list(&done.left)));
    }
    told.push_str(" What commands did outside the project, or to anything but its files, wasn't undone.]");
    told
}

/// Whether `call` may change the project's files: a write or an edit, or
/// a command that asks for more than to look.
fn may_change_files(call: &ToolCall) -> bool {
    match ToolName::of(call) {
        Some(tool) if tool.changes_files() => true,
        Some(ToolName::Bash) => BashCall::of(call).is_some_and(|bash| bash.access() != crate::sandbox::Access::Look),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use serde_json::json;

    use super::*;

    #[test]
    fn a_note_of_an_undo_isnt_what_you_typed() {
        let note = undone(&Plan { said: "fix it".into(), turns: 1, ..Plan::default() });
        assert_eq!(compact::typed(&Item::User(note)), None);
    }
    use crate::permissions::Mode;

    /// What a test model kept of a request it was sent: requests borrow,
    /// so it keeps its own copy.
    #[derive(Clone)]
    struct Seen {
        input: Vec<Item>,
        tools: Vec<Tool>,
        session_id: Option<String>,
        hold_back: bool,
        instructions: Option<String>,
        images: bool,
    }

    impl From<&Request<'_>> for Seen {
        fn from(request: &Request<'_>) -> Self {
            Seen {
                input: request.input.to_vec(),
                tools: request.tools.to_vec(),
                session_id: request.session_id.map(str::to_owned),
                hold_back: request.hold_back,
                instructions: request.instructions.map(str::to_owned),
                images: request.images,
            }
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
            access: rein_mcp::Walled::Set(crate::sandbox::Access::Outside),
        };
        let server = rein_mcp::Server {
            name: "fake".into(),
            origin: rein_mcp::Origin::Yours,
            reach: rein_mcp::Reach::Local(local),
            allow: vec!["echo".into()],
            offered: Default::default(),
        };
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
        let system = requests[0].instructions.as_deref().unwrap_or_default();
        assert!(system.ends_with("the server's, not the user's.\n\n## fake\nEcho before peeking.[2J"), "what the server says of using it: {system}");
        assert!(requests.iter().all(|seen| seen.instructions == requests[0].instructions), "and the same all conversation");
        assert!(requests[0].tools.iter().filter(|tool| rein_mcp::is_mcp(&tool.name)).all(|tool| !tool.strict));
    }

    /// A server whose tools are held back is named in the search, which
    /// rein runs itself: what it finds is recorded as found, and called as
    /// any of the server's tools.
    #[tokio::test]
    async fn a_held_back_tool_is_found_with_the_search_and_then_called() {
        let call = |id: &str, name: &str, arguments: Value| Item::ToolCall(ToolCall { call_id: id.into(), name: name.into(), arguments: arguments.to_string() });
        let turns = vec![
            vec![call("s1", "tool_search", json!({ "query": "select:echo", "limit": null }))],
            vec![call("c1", "mcp__fake__echo", json!({ "text": "hello" }))],
            vec![Item::Assistant("Done.".into())],
        ];
        let mut agent = agent("mcp-held-back", turns);
        let root = agent.tools.root().to_owned();
        std::fs::write(root.join("server.sh"), rein_mcp::FAKE_SERVER).unwrap();
        let local =
            rein_mcp::Local { command: "sh".into(), args: vec!["server.sh".into()], env: Default::default(), access: rein_mcp::Walled::Set(crate::sandbox::Access::Outside) };
        let server = rein_mcp::Server {
            name: "fake".into(),
            origin: rein_mcp::Origin::Yours,
            reach: rein_mcp::Reach::Local(local),
            allow: vec!["echo".into()],
            offered: rein_mcp::Offered::OnSearch,
        };
        let launch = rein_mcp::Launch { sandbox: Default::default(), root: root.clone(), caches: root.join("caches"), keys: Default::default(), web: None, tokens: None };
        agent.tools = Tools::new(&root).with_mcp(std::sync::Arc::new(rein_mcp::Servers::start(vec![server], launch)));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("use the server".into(), &mut person).await.unwrap();
        assert_eq!(person.log, ["ran tool_search", "result found fake: echo", "ran mcp__fake__echo", "result hello", "text Done."]);
        {
            let requests = agent.model.requests.borrow();
            let first = &requests[0];
            assert!(first.hold_back);
            assert!(first.tools.iter().filter(|tool| rein_mcp::is_mcp(&tool.name)).all(|tool| tool.loading == Loading::Found), "all held back");
            let search = first.tools.last().unwrap();
            assert_eq!((search.name.as_str(), search.loading), ("tool_search", Loading::Search), "last, after what's always the same");
            assert!(search.description.contains("- fake: echo, "), "{}", search.description);
            assert_eq!(requests[1].input.last(), Some(&Item::Found { call_id: "s1".into(), names: vec!["mcp__fake__echo".into()] }));
            assert!(requests.iter().all(|seen| seen.tools == first.tools), "the same tools all conversation");
        }

        // A model whose list says it can't is shown them all; the format leaves the search out.
        agent.finds_tools = rein_llm::FindsTools::No;
        agent.model.turns.borrow_mut().push_back(vec![Item::Assistant("Fine.".into())]);
        agent.send("again".into(), &mut person).await.unwrap();
        assert!(!agent.model.requests.borrow().last().unwrap().hold_back);
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
        let local =
            rein_mcp::Local { command: "sh".into(), args: vec!["server.sh".into()], env: Default::default(), access: rein_mcp::Walled::Set(crate::sandbox::Access::Outside) };
        let server =
            rein_mcp::Server { name: "fake".into(), origin: rein_mcp::Origin::Yours, reach: rein_mcp::Reach::Local(local), allow: Vec::new(), offered: Default::default() };
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
            servers_say: Vec::new(),
            reasoning_effort: None,
            efforts: rein_llm::Efforts::Unknown,
            max_output: None,
            finds_tools: rein_llm::FindsTools::Unknown,
            harness: rein_llm::Harness::Rein,
            searcher: None,
            sees: rein_llm::Sees::Unknown,
            pasted: Vec::new(),
            skills: SkillsHere::default(),
            step_limit: None,
            session_id: Some("session-1".into()),
            history: Vec::new(),
            session: None,
            context: None,
            window: Window::default(),
            hooks: std::sync::Arc::default(),
            hooks_said: Vec::new(),
            hook_stopped: None,
            checking: crate::check::Checking::default(),
            changed_in_step: false,
        }
    }

    #[test]
    fn a_model_gets_the_tools_its_own_harness_gives_it_in_their_place() {
        let mut agent = agent("harness", Vec::new());
        let names = |agent: &Agent<Scripted>| agent.specs.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
        let rein = names(&agent);
        agent.set_harness(rein_llm::Harness::Codex);
        let codex = names(&agent);
        let at = rein.iter().position(|name| name == "write").unwrap();
        assert_eq!(codex[at], "apply_patch", "where write and edit were: {codex:?}");
        assert!(!codex.contains(&"edit".to_owned()) && !codex.contains(&"write".to_owned()));
        assert_eq!(codex.len(), rein.len() - 1);
        assert!(agent.specs[at].grammar.is_some(), "in Codex's grammar");
        let asks = rein.iter().position(|name| name == "ask").expect("rein's models are asked through ask");
        let asks_now = codex.iter().position(|name| name == "request_user_input").expect("Codex's through its own");
        // Two tools before it became one if write and edit came first.
        assert_eq!(asks_now, if at < asks { asks - 1 } else { asks }, "in ask's place: {codex:?}");
        assert!(!codex.contains(&"ask".to_owned()));
        agent.set_harness(rein_llm::Harness::Rein);
        assert_eq!(names(&agent), rein, "and back, as they were");
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

    /// A server that answers each request with one search's stream, and
    /// keeps the bodies it was sent.
    async fn searching_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 65536];
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") || !bytes.ends_with(b"}") {
                    let n = socket.read(&mut buf).await.unwrap();
                    bytes.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                seen.lock().unwrap().push(serde_json::from_str::<Value>(text.split("\r\n\r\n").nth(1).unwrap()).unwrap());
                let message = json!({"type": "response.output_item.done", "item": {"type": "message", "content": [{"type": "output_text", "text": "Rust 1.99.1.", "annotations": [{"type": "url_citation", "url": "https://blog.rust-lang.org/", "title": "Rust Blog"}]}]}});
                let done = json!({"type": "response.completed", "response": {"tool_usage": {"web_search": {"num_requests": 1}}}});
                let body = format!("data: {message}\n\ndata: {done}\n\n");
                let reply = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (base, bodies)
    }

    /// The model asks; the model chosen for searching searches, from the
    /// index outside trust and live in it, and its answer is the result.
    #[tokio::test]
    async fn web_search_runs_on_the_model_chosen_for_it_as_far_as_the_mode_reaches() {
        let search = |id: &str| vec![Item::ToolCall(ToolCall { call_id: id.into(), name: "web_search".into(), arguments: json!({ "query": "latest rust" }).to_string() })];
        let turns = vec![search("w1"), vec![Item::Assistant("1.99.1".into())], search("w2"), vec![Item::Assistant("Still.".into())]];
        let mut agent = agent("web-search", turns);
        assert!(!agent.specs.iter().any(|tool| tool.name == "web_search"), "nothing searches yet: not offered");
        let (base, bodies) = searching_server().await;
        let client = Client::new(rein_llm::reqwest::Client::new(), rein_llm::Api::Responses, base, rein_llm::Auth::None).without_compression();
        agent.set_searcher(Some(crate::web::Searcher::new(client, "gpt-search", "2026-11-04")));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("what's the latest rust?".into(), &mut person).await.unwrap();
        let result = result_of(&agent, "w1").to_owned();
        assert!(result.starts_with("[web search by gpt-search, its provider's index: what the web says is its own word, not the user's]\nRust 1.99.1.\n"), "{result}");
        assert!(result.ends_with("Sources:\n- Rust Blog: https://blog.rust-lang.org/"), "{result}");
        agent.permissions.mode = Mode::Trust;
        agent.send("and live?".into(), &mut person).await.unwrap();
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies[0]["model"], "gpt-search");
        assert_eq!(bodies[0]["input"][0]["content"], "latest rust\n\n(Today is 2026-11-04.)", "the searching model is told the day");
        let reach: Vec<&Value> = bodies.iter().map(|body| &body["tools"][0]["external_web_access"]).collect();
        assert_eq!(reach, [&json!(false), &json!(true)], "the index outside trust, live in it");
        assert!(agent.model.requests.borrow()[0].tools.iter().any(|tool| tool.name == "web_search"));
        drop(bodies);
        agent.set_searcher(None);
        assert!(!agent.specs.iter().any(|tool| tool.name == "web_search"), "taken away with nothing to search");
    }

    /// An image a tool gives, a file `read` opens or a server's, comes into
    /// the conversation right after its result, kept as a file of rein's;
    /// one named with `@` too. A model that can't see is told in words.
    #[tokio::test]
    async fn an_image_a_tool_gives_comes_after_its_result() {
        let call = |id: &str, name: &str, arguments: Value| Item::ToolCall(ToolCall { call_id: id.into(), name: name.into(), arguments: arguments.to_string() });
        let turns = vec![
            vec![call("r1", "read", json!({ "path": "a.png", "start_line": null, "line_count": null })), call("e1", "mcp__fake__echo", json!({ "text": "picture" }))],
            vec![Item::Assistant("A grey square and a dot.".into())],
            vec![Item::Assistant("Seen.".into())],
        ];
        let mut agent = agent("images", turns);
        let root = agent.tools.root().to_owned();
        std::fs::write(root.join("a.png"), crate::image::tests::png(40, 30, png::Compression::Fast)).unwrap();
        std::fs::write(root.join("server.sh"), rein_mcp::FAKE_SERVER).unwrap();
        let local =
            rein_mcp::Local { command: "sh".into(), args: vec!["server.sh".into()], env: Default::default(), access: rein_mcp::Walled::Set(crate::sandbox::Access::Outside) };
        let server = rein_mcp::Server {
            name: "fake".into(),
            origin: rein_mcp::Origin::Yours,
            reach: rein_mcp::Reach::Local(local),
            allow: vec!["echo".into()],
            offered: rein_mcp::Offered::UpFront,
        };
        let launch = rein_mcp::Launch { sandbox: Default::default(), root: root.clone(), caches: root.join("caches"), keys: Default::default(), web: None, tokens: None };
        let kept = root.join(".images");
        agent.tools = Tools::new(&root).with_mcp(std::sync::Arc::new(rein_mcp::Servers::start(vec![server], launch))).with_images(kept.clone());
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("look at them".into(), &mut person).await.unwrap();
        let after = |id: &str| -> Option<(String, Option<&Item>)> {
            let at = agent.history.iter().position(|item| matches!(item, Item::ToolResult { call_id, .. } if call_id == id))?;
            Some((result_of(&agent, id).to_owned(), agent.history.get(at + 1)))
        };
        let (read, next) = after("r1").unwrap();
        assert_eq!(read, "[image a.png, 40×30]");
        assert!(matches!(next, Some(Item::Image { call_id, image }) if call_id == "r1" && image.width == 40 && image.file.starts_with(&kept)), "{next:?}");
        let (echo, next) = after("e1").unwrap();
        assert_eq!(echo, "a picture\n[an image fake's echo gave, 1×1]");
        assert!(matches!(next, Some(Item::Image { call_id, .. }) if call_id == "e1"));
        {
            let requests = agent.model.requests.borrow();
            assert!(requests[1].images, "shown to a model that may see");
            assert_eq!(requests[1].input.iter().filter(|item| matches!(item, Item::Image { .. })).count(), 2);
        }
        // Named with @, it's read for the message, and comes the same way.
        agent.sees = rein_llm::Sees::No;
        agent.send("and @a.png?".into(), &mut person).await.unwrap();
        let mentioned = agent.history.iter().any(|item| matches!(item, Item::Image { call_id, .. } if call_id.starts_with("mention_")));
        assert!(mentioned, "{:?}", agent.history);
        assert!(!agent.model.requests.borrow().last().unwrap().images, "a model that can't see: said in words");
    }

    /// The model loads a skill when a task fits, and gets it once: again,
    /// it's said to be in the conversation already. One you call goes with
    /// your message, as a load made for you.
    #[tokio::test]
    async fn a_skill_is_loaded_by_the_model_or_with_your_message() {
        let load = |id: &str| {
            vec![Item::ToolCall(ToolCall { call_id: id.into(), name: "skill".into(), arguments: json!({ "name": "graphs", "file": null, "start_line": null }).to_string() })]
        };
        let mut agent = agent("skills", vec![load("k1"), load("k2"), vec![Item::Assistant("Drawn.".into())], vec![Item::Assistant("Again.".into())]]);
        assert!(agent.system().is_none_or(|system| !system.contains("# Skills")), "no skills: nothing said of them");
        let home = agent.tools.root().join("home");
        std::fs::create_dir_all(home.join(".agents/skills/graphs")).unwrap();
        std::fs::write(home.join(".agents/skills/graphs/SKILL.md"), "---\nname: graphs\ndescription: Draw graphs.\n---\nUse boxes and arrows.\n").unwrap();
        agent.set_skills(crate::skills::find(&crate::skills::folders(&home, Some(&home), false)), crate::skills::LEAST_ROOM);
        assert!(agent.specs.iter().any(|tool| tool.name == "skill" && tool.description.contains("- graphs: Draw graphs.")));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("draw the modules".into(), &mut person).await.unwrap();
        assert!(result_of(&agent, "k1").contains("Use boxes and arrows."), "{}", result_of(&agent, "k1"));
        assert!(
            agent.model.requests.borrow()[0].instructions.as_deref().is_some_and(|system| system.contains("# Skills\nThe skill tool lists skills")),
            "told in its instructions too"
        );
        assert!(result_of(&agent, "k2").starts_with(compact::UNCHANGED), "not twice: {}", result_of(&agent, "k2"));
        agent.call_skill("graphs", None);
        agent.send("/graphs the tests".into(), &mut person).await.unwrap();
        let at = agent.history.iter().position(|item| matches!(item, Item::User(text) if text == "/graphs the tests")).unwrap();
        assert!(matches!(&agent.history[at + 1], Item::ToolCall(call) if call.name == "skill" && call.call_id.starts_with("mention_")), "{:?}", agent.history[at + 1]);
        assert!(
            matches!(&agent.history[at + 2], Item::ToolResult { output, .. } if output.starts_with(compact::UNCHANGED)),
            "loaded already this conversation: {:?}",
            agent.history[at + 2]
        );
        assert_eq!(agent.take_loaded(), ["graphs", "graphs", "graphs"], "each a use, to count");
        assert!(agent.take_loaded().is_empty());
    }

    /// A skill added, changed or turned off mid-conversation is told at its
    /// end, with your next message: the tool's list, before everything,
    /// stays as it was, and so does what's cached. A summary keeps it.
    #[tokio::test]
    async fn skills_that_change_are_told_at_the_end_not_in_the_list() {
        let answer = |text: &str| vec![Item::Assistant(text.into())];
        let mut agent = agent("skills-change", vec![answer("one"), answer("two"), answer("three"), answer("Summary."), answer("four")]);
        let home = agent.tools.root().join("home");
        let skills = home.join(".agents/skills");
        crate::skills::tests::skill(&skills, "graphs", "name: graphs\ndescription: Draw graphs.", "Boxes.");
        let find = || crate::skills::find(&crate::skills::folders(&home, Some(&home), false));
        agent.set_skills(find(), crate::skills::LEAST_ROOM);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("first".into(), &mut person).await.unwrap();
        let listed = agent.specs.clone();
        crate::skills::tests::skill(&skills, "deploy", "name: deploy\ndescription: Ship it.", "Ship.");
        agent.set_skills(find(), crate::skills::LEAST_ROOM);
        agent.send("second".into(), &mut person).await.unwrap();
        agent.set_skills(find(), crate::skills::LEAST_ROOM);
        agent.send("third".into(), &mut person).await.unwrap();
        {
            let requests = agent.model.requests.borrow();
            assert_eq!(requests[1].tools, requests[0].tools, "the list stays");
            assert_eq!(agent.specs, listed);
            let notes: Vec<&String> = requests[2]
                .input
                .iter()
                .filter_map(|item| match item {
                    Item::User(text) if text.starts_with("[The skills") => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(notes.len(), 1, "told once, not again while nothing more changed: {notes:?}");
            assert!(notes[0].contains("New, to load as the listed ones are:\n- deploy: Ship it."), "{}", notes[0]);
            let second = requests[1].input.iter().position(|item| matches!(item, Item::User(text) if text == "second")).unwrap();
            assert!(matches!(&requests[1].input[second - 1], Item::User(text) if text == notes[0]), "just before your message");
        }
        // Nothing kept word for word: the note comes after the summary.
        let limit = agent.limit();
        agent.set_limit(5);
        agent.compact(None, true, &mut person).await.unwrap();
        agent.set_limit(limit);
        assert!(!agent.history.iter().any(|item| matches!(item, Item::User(text) if text == "third")), "summarized");
        assert!(agent.history.iter().any(|item| matches!(item, Item::User(text) if text.contains("- deploy: Ship it."))), "a summary keeps it");
        let summary = agent
            .history
            .iter()
            .find_map(|item| match item {
                Item::User(text) if compact::is_summary(item) => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        assert!(!summary.contains("Ship it."), "not as something you said: {summary}");
        std::fs::remove_dir_all(skills.join("deploy")).unwrap();
        agent.set_skills(find(), crate::skills::LEAST_ROOM);
        agent.send("fourth".into(), &mut person).await.unwrap();
        assert!(agent.history.iter().any(|item| matches!(item, Item::User(text) if text == crate::skills::AS_LISTED)), "as listed again");
    }

    fn in_background(id: &str, command: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "bash".into(), arguments: json!({ "command": command, "background": true }).to_string() })
    }

    #[tokio::test]
    async fn a_background_command_that_ends_mid_turn_is_told_with_the_next_step_once() {
        // It ends when the second step says so: a Linux shell can finish before the next request, so it can't be left to timing.
        let command = "until [ -e go ]; do sleep 0.01; done; echo built; exit 4";
        let turns = vec![vec![in_background("bg", command)], vec![bash("wait", "touch go; sleep 0.6")], vec![Item::Assistant("Seen.".into())]];
        let mut agent = agent("bg-mid-turn", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("build in the background".into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        let notes = |input: &[Item]| input.iter().filter(|item| matches!(item, Item::User(text) if crate::background::is_note(text))).count();
        assert_eq!(notes(&requests[1].input), 0, "it hadn't ended yet");
        let last = &requests[2].input;
        assert_eq!(notes(last), 1);
        // After the step it ended during, at the end of the conversation.
        let Some(Item::User(note)) = last.last() else { panic!("{last:?}") };
        assert!(note.contains(&format!("b1 ({command}) ended: [exit code 4]")) && note.contains("\nbuilt"), "{note}");
        assert!(matches!(&last[last.len() - 2], Item::ToolResult { call_id, .. } if call_id == "wait"));
        assert!(!compact::record(&agent.window.full).contains("ended: [exit code 4]"), "not as something you said");
    }

    #[tokio::test]
    async fn a_command_you_stopped_is_told_just_before_your_next_message() {
        let turns = vec![vec![in_background("bg", "sleep 49.31")], vec![Item::Assistant("Running.".into())], vec![Item::Assistant("Noted.".into())]];
        let mut agent = agent("bg-yours", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("start it".into(), &mut person).await.unwrap();
        agent.tools.jobs().stop_yours("b1").await.unwrap();
        agent.send("I stopped it".into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        let input = &requests[2].input;
        let yours = input.iter().position(|item| matches!(item, Item::User(text) if text == "I stopped it")).unwrap();
        assert!(matches!(&input[yours - 1], Item::User(text) if text.contains("b1 (sleep 49.31) was stopped by the user.")), "{:?}", input[yours - 1]);
    }

    fn edits(id: &str, path: &str, old: &str, new: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "edit".into(), arguments: json!({ "path": path, "old_text": old, "new_text": new, "replace_all": false }).to_string() })
    }

    /// A check that reads as tsc writes: an error for each line of a.ts
    /// that says BROKEN, and a line in check.log each time it runs.
    fn tsc_like(root: &std::path::Path) -> crate::check::Check {
        let script =
            "echo ran >> check.log; grep -n BROKEN a.ts | while IFS=: read n rest; do echo \"a.ts($n,1): error TS2304: Cannot find name '$rest'.\"; done; ! grep -q BROKEN a.ts";
        std::fs::write(root.join("check.sh"), script).unwrap();
        crate::check::Check { command: "sh check.sh".into(), reads: crate::check::Reads::Tsc, found_from: None }
    }

    fn check_notes(input: &[Item]) -> Vec<String> {
        input
            .iter()
            .filter_map(|item| match item {
                Item::User(text) if crate::check::is_note(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn an_edit_that_breaks_the_build_is_told_at_its_step_and_its_fix_too() {
        let turns = vec![vec![edits("e1", "a.ts", "ok", "BROKEN here")], vec![edits("e2", "a.ts", "BROKEN here", "fine")], vec![Item::Assistant("Fixed.".into())]];
        let mut agent = agent("check-breaks", turns);
        let root = root_of("check-breaks");
        std::fs::write(root.join("a.ts"), "OLDBROKEN stays\nok\n").unwrap();
        agent.set_check(Some(tsc_like(&root)));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("change it".into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        assert!(check_notes(&requests[0].input).is_empty());
        // The old error was there before: only the edit's is told.
        assert_eq!(
            check_notes(&requests[1].input),
            ["[rein checked the project with `sh` after these changes: 1 new error.\na.ts:2:1 error[TS2304]: Cannot find name 'BROKEN here'.]"]
        );
        let fixed = check_notes(&requests[2].input);
        assert_eq!(fixed.len(), 2);
        assert!(fixed[1].ends_with("the errors your changes brought in are gone.]"), "{fixed:?}");
        // Before the first change, and after each of the two.
        assert_eq!(std::fs::read_to_string(root.join("check.log")).unwrap().lines().count(), 3);
        assert!(!compact::record(&agent.window.full).contains("rein checked"), "not as something you said");
    }

    #[tokio::test]
    async fn a_turn_that_ends_with_what_it_broke_is_told_once() {
        let turns = vec![vec![edits("e1", "a.ts", "ok", "BROKEN")], vec![Item::Assistant("Done.".into())], vec![Item::Assistant("It stays broken on purpose.".into())]];
        let mut agent = agent("check-end", turns);
        let root = root_of("check-end");
        std::fs::write(root.join("a.ts"), "ok\n").unwrap();
        agent.set_check(Some(tsc_like(&root)));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("break it".into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        assert_eq!(requests.len(), 3, "told once as it said it was done, then it was");
        let end = check_notes(&requests[2].input);
        assert!(end.last().unwrap().contains("1 new error your changes this turn brought in is still there. Fix it, or say why it's left:\na.ts:1:1"), "{end:?}");
    }

    #[tokio::test]
    async fn a_key_in_what_the_check_says_is_hidden_from_the_model() {
        let turns = vec![vec![edits("e1", "a.ts", "ok", "BROKEN FAKE-check-key-123")], vec![Item::Assistant("Seen.".into())], vec![Item::Assistant("Left.".into())]];
        let mut agent = agent("check-key", turns);
        let root = root_of("check-key");
        std::fs::write(root.join("a.ts"), "ok\n").unwrap();
        agent.set_check(Some(tsc_like(&root)));
        agent.tools.hide_too(["FAKE-check-key-123".to_owned()]);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("change it".into(), &mut person).await.unwrap();
        let told = check_notes(&agent.model.requests.borrow()[1].input);
        assert!(told[0].contains("Cannot find name 'BROKEN") && !told[0].contains("FAKE-check-key-123"), "{told:?}");
    }

    #[tokio::test]
    async fn a_turn_that_only_looks_isnt_checked() {
        let turns = vec![vec![bash("look", "ls"), read_call("r", "a.ts", json!(null), json!(null))], vec![Item::Assistant("Seen.".into())]];
        let mut agent = agent("check-looks", turns);
        let root = root_of("check-looks");
        std::fs::write(root.join("a.ts"), "BROKEN\n").unwrap();
        agent.set_check(Some(tsc_like(&root)));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("look".into(), &mut person).await.unwrap();
        assert!(!root.join("check.log").exists(), "it never ran");
    }

    #[tokio::test]
    async fn the_compaction_record_says_what_still_runs_and_the_end_stops_it() {
        let turns = vec![vec![in_background("bg", "sleep 49.21")], vec![Item::Assistant("Running.".into())], vec![Item::Assistant("Summary.".into())]];
        let mut agent = agent("bg-compact", turns);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("start it".into(), &mut person).await.unwrap();
        agent.set_limit(5);
        agent.compact(None, true, &mut person).await.unwrap();
        let summary = agent.history.iter().find_map(|item| if compact::is_summary(item) { Some(format!("{item:?}")) } else { None }).unwrap();
        assert!(summary.contains("Still running in the background, so don't start them again") && summary.contains("- b1: `sleep 49.21`"), "{summary}");
        agent.session_ended(hooks::Ended::Exit, &mut person).await;
        assert!(agent.tools.jobs().list().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(200));
        let ps = std::process::Command::new("ps").args(["-axo", "stat=,command="]).output().unwrap();
        let left = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .any(|line| line.trim_start().split_once(' ').is_some_and(|(stat, rest)| !stat.starts_with('Z') && rest.trim() == "sleep 49.21"));
        assert!(!left, "the sleep was left running");
    }

    /// An image you paste goes with your message, as a read made for you.
    #[tokio::test]
    async fn a_pasted_image_goes_with_the_message() {
        let mut agent = agent("pasted", vec![vec![Item::Assistant("A dot.".into())]]);
        let image =
            rein_llm::Image { kind: rein_llm::ImageKind::Png, data: None, file: "/k/a.png".into(), width: 1, height: 1, about: "the image you pasted as [Image #1], 1×1".into() };
        agent.paste(image.clone());
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("what's [Image #1]?".into(), &mut person).await.unwrap();
        let input = &agent.model.requests.borrow()[0].input;
        let at = input.iter().position(|item| matches!(item, Item::User(text) if text == "what's [Image #1]?")).unwrap();
        assert!(matches!(&input[at + 1], Item::ToolCall(call) if call.name == "read" && call.arguments.contains("(pasted)")));
        assert!(matches!(&input[at + 2], Item::ToolResult { output, .. } if output == "[the image you pasted as [Image #1], 1×1]"));
        assert!(matches!(&input[at + 3], Item::Image { image: shown, .. } if *shown == image));
    }

    #[tokio::test]
    async fn a_search_rein_has_to_say_something_of_is_said_in_words() {
        let search = |id: &str| vec![Item::ToolCall(ToolCall { call_id: id.into(), name: "tool_search".into(), arguments: json!({ "query": "echo", "limit": null }).to_string() })];
        let mut agent = agent("search-noted", vec![search("s1"), search("s2"), search("s3"), vec![Item::Assistant("Stopping.".into())]]);
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("find echo".into(), &mut person).await.unwrap();
        // The same search over and over: the third is told so, which only words can carry.
        assert!(matches!(&agent.history[2], Item::Found { call_id, .. } if call_id == "s1"));
        assert!(matches!(&agent.history[4], Item::Found { call_id, .. } if call_id == "s2"));
        assert!(matches!(&agent.history[6], Item::ToolResult { call_id, output } if call_id == "s3" && output.ends_with(REPEATING)), "{:?}", agent.history[6]);
    }

    #[tokio::test]
    async fn a_tool_found_before_a_compaction_is_found_again_after_it() {
        let call = |id: &str, name: &str, arguments: Value| Item::ToolCall(ToolCall { call_id: id.into(), name: name.into(), arguments: arguments.to_string() });
        let mut agent = agent("compact-found", vec![vec![Item::Assistant("Goal: echo.".into())]]);
        let echo = call("c1", "mcp__fake__echo", json!({ "text": "hi" }));
        agent.history = vec![
            Item::User("echo hi".into()),
            call("s1", "tool_search", json!({ "query": "echo", "limit": null })),
            Item::Found { call_id: "s1".into(), names: vec!["mcp__fake__echo".into()] },
            Item::Assistant("x".repeat(8_000)),
            echo.clone(),
            Item::ToolResult { call_id: "c1".into(), output: "hi".into() },
        ];
        agent.window.limit = 2_000;
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.compact(None, true, &mut person).await.unwrap();
        // The summary, the search found again, and what was kept, which calls it.
        assert!(matches!(&agent.history[0], Item::User(text) if text.starts_with("<summary>")));
        assert!(matches!(&agent.history[1], Item::ToolCall(search) if search.name == "tool_search" && search.arguments.contains("select:mcp__fake__echo")));
        assert!(matches!(&agent.history[2], Item::Found { names, .. } if names == &["mcp__fake__echo".to_owned()]));
        assert_eq!(agent.history[3..], [echo, Item::ToolResult { call_id: "c1".into(), output: "hi".into() }]);
        // A search answered by what it found has its result: nothing to repair.
        let before = agent.history.clone();
        agent.repair();
        assert_eq!(agent.history, before);
    }

    /// Before the provider has counted, the estimate counts the tools a
    /// request carries: one held back only once the model has found it.
    #[test]
    fn a_held_back_tool_counts_only_once_it_is_found() {
        let mut agent = agent("estimate", vec![]);
        let bare = agent.estimated();
        let tool = |name: &str, loading| Tool { grammar: None, loading, strict: false, name: name.into(), description: "x".repeat(40_000), parameters: json!({}) };
        agent.specs.push(tool("mcp__big__one", Loading::Found));
        agent.specs.push(tool(rein_llm::TOOL_SEARCH, Loading::Search));
        let held = agent.estimated();
        assert!(held < bare + 11_000, "held back, only the search goes: {bare} → {held}");
        agent.history.push(Item::Found { call_id: "s1".into(), names: vec!["mcp__big__one".into()] });
        assert!(agent.estimated() >= held + 10_000, "found, it goes from then on");
        // A model that can't find tools is shown them all, but the search.
        agent.history.pop();
        agent.finds_tools = rein_llm::FindsTools::No;
        let all = agent.estimated();
        assert!(all >= bare + 10_000 && all < bare + 11_000, "{bare} → {all}");
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
        agent.window.limit = 2_500;
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
        let request = Request {
            model: "m",
            instructions: None,
            input: &input,
            tools: &[],
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
            hold_back: false,
            images: false,
        };
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
        agent.window.limit = 2_500;
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

    /// `agent` with `hooks`, as rein's own file sets them, in trust mode,
    /// where they run as your commands do.
    fn with_hooks(agent: &mut Agent<Scripted>, name: &str, hooks: Value) {
        let dir = root_of(name).join(".hooks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hooks.json"), json!({ "hooks": hooks }).to_string()).unwrap();
        agent.set_hooks(std::sync::Arc::new(Hooks::load(&dir, None, &root_of(name), &hooks::Chosen::default(), &[])));
        agent.permissions.mode = crate::permissions::Mode::Trust;
    }

    fn hook(line: &str) -> Value {
        json!({ "type": "command", "command": line })
    }

    /// A hook that gives `to` as a command's line in place of the model's.
    fn rewriting(to: &str) -> Value {
        hook(&format!(r#"cat >/dev/null; echo '{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","updatedInput":{{"command":"{to}"}}}}}}'"#))
    }

    #[tokio::test]
    async fn a_hooks_rewrite_runs_and_reins_rules_still_see_the_call_as_it_was() {
        let turns = vec![vec![bash("b1", "printf hi"), bash("b2", "rm -rf x")], vec![Item::Assistant("Done.".into())]];
        let mut agent = agent("hook-rewrite", turns);
        with_hooks(&mut agent, "hook-rewrite", json!({ "PreToolUse": [{ "matcher": "Bash", "hooks": [rewriting("echo rewritten")] }] }));
        agent.permissions.set_yours("ask rm").unwrap();
        let mut person = Person { answer: Approval::No, log: Vec::new() };
        agent.send("go".into(), &mut person).await.unwrap();
        assert_eq!(result_of(&agent, "b1"), "rewritten\n[exit code 0]", "what ran is the hook's");
        assert!(person.log.iter().any(|line| line.starts_with("asked") && line.contains("echo rewritten")), "rm, as the model asked, asks: {:?}", person.log);
        assert_eq!(result_of(&agent, "b2"), DECLINED_COMMAND);
        let history = &agent.history;
        assert!(history.iter().any(|item| matches!(item, Item::ToolCall(call) if call.arguments.contains("printf hi"))), "the model's call stays as it made it");
    }

    #[tokio::test]
    async fn a_hook_that_says_no_keeps_the_call_from_running_and_tells_the_model_why() {
        let turns = vec![vec![bash("b1", "touch made.txt")], vec![Item::Assistant("Ok.".into())]];
        let mut agent = agent("hook-no", turns);
        with_hooks(&mut agent, "hook-no", json!({ "PreToolUse": [{ "hooks": [hook("echo 'not on main' >&2; exit 2")] }] }));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("go".into(), &mut person).await.unwrap();
        assert_eq!(result_of(&agent, "b1"), "error: one of the user's hooks, echo, said no: not on main");
        assert!(!root_of("hook-no").join("made.txt").exists());
    }

    #[tokio::test]
    async fn a_stop_hook_keeps_the_turn_going_until_it_says_the_work_is_done() {
        let turns = vec![vec![Item::Assistant("Done.".into())], vec![Item::Assistant("Tests pass now.".into())]];
        let mut agent = agent("hook-stop", turns);
        let line = r#"cat >/dev/null; if [ -e checked ]; then exit 0; fi; touch checked; echo '{"decision":"block","reason":"the tests fail"}'"#;
        with_hooks(&mut agent, "hook-stop", json!({ "Stop": [{ "hooks": [hook(line)] }] }));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        assert_eq!(agent.send("fix it".into(), &mut person).await.unwrap(), Outcome::Done);
        let requests = agent.model.requests.borrow();
        assert_eq!(requests.len(), 2, "kept going once");
        let told = requests[1].input.iter().any(|item| matches!(item, Item::User(text) if hooks::is_note(text) && text.contains("the tests fail") && text.contains("isn't done")));
        assert!(told, "{:?}", requests[1].input);
    }

    #[tokio::test]
    async fn what_a_hook_adds_to_your_message_goes_after_it_and_isnt_taken_for_your_words() {
        let mut agent = agent("hook-context", vec![vec![Item::Assistant("Seen.".into())]]);
        with_hooks(&mut agent, "hook-context", json!({ "UserPromptSubmit": [{ "hooks": [hook("cat >/dev/null; echo 'a.rs calls b.rs'")] }] }));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("how does a.rs work?".into(), &mut person).await.unwrap();
        let input = &agent.model.requests.borrow()[0].input;
        assert_eq!(input[0], Item::User("how does a.rs work?".into()));
        assert!(matches!(&input[1], Item::User(text) if hooks::is_note(text) && text.ends_with("a.rs calls b.rs]")), "{:?}", input[1]);
        assert!(!compact::record(&agent.window.full).contains("a.rs calls b.rs"), "compaction's record of your words leaves it out");
    }

    #[tokio::test]
    async fn a_hook_that_says_no_to_your_message_keeps_it_from_being_sent() {
        let mut agent = agent("hook-refused", vec![vec![Item::Assistant("Never.".into())]]);
        with_hooks(&mut agent, "hook-refused", json!({ "UserPromptSubmit": [{ "hooks": [hook("echo 'that has a key in it' >&2; exit 2")] }] }));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        let outcome = agent.send("my key is sk-123".into(), &mut person).await.unwrap();
        assert_eq!(outcome, Outcome::Refused(By { hook: "echo".into(), said: "that has a key in it".into() }));
        assert!(agent.model.requests.borrow().is_empty() && agent.history.is_empty(), "not sent, nor kept");
    }

    #[tokio::test]
    async fn what_a_hook_says_as_the_session_starts_goes_with_your_first_message() {
        let mut agent = agent("hook-start", vec![vec![Item::Assistant("Ok.".into())]]);
        with_hooks(&mut agent, "hook-start", json!({ "SessionStart": [{ "matcher": "startup", "hooks": [hook("echo 'on branch main'")] }] }));
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.session_started(hooks::Source::Resume, &mut person).await;
        agent.session_started(hooks::Source::Startup, &mut person).await;
        agent.send("hi".into(), &mut person).await.unwrap();
        let requests = agent.model.requests.borrow();
        let notes: Vec<&Item> = requests[0].input.iter().filter(|item| matches!(item, Item::User(text) if hooks::is_note(text))).collect();
        assert_eq!(notes.len(), 1, "only on startup, as its matcher says");
        assert!(matches!(notes[0], Item::User(text) if text.contains("on branch main")));
    }

    #[tokio::test]
    async fn a_hook_on_a_question_answers_it_but_never_one_an_ask_rule_asks() {
        let turns = vec![vec![bash("b1", "touch a.txt"), bash("b2", "rm -f a.txt")], vec![Item::Assistant("Ok.".into())]];
        let mut agent = agent("hook-permission", turns);
        // One hook has rein ask about every command; another answers yes.
        let ask = r#"cat >/dev/null; echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"check it"}}'"#;
        let yes = r#"cat >/dev/null; echo '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'"#;
        with_hooks(&mut agent, "hook-permission", json!({ "PreToolUse": [{ "hooks": [hook(ask)] }], "PermissionRequest": [{ "hooks": [hook(yes)] }] }));
        agent.permissions.set_yours("ask rm").unwrap();
        let mut person = Person { answer: Approval::No, log: Vec::new() };
        agent.send("go".into(), &mut person).await.unwrap();
        let asked: Vec<&String> = person.log.iter().filter(|line| line.starts_with("asked")).collect();
        assert_eq!(asked.len(), 1, "only rm, which your rule asks about: {:?}", person.log);
        assert!(asked[0].contains("rm -f a.txt"));
        assert_eq!(result_of(&agent, "b1"), "[exit code 0]");
    }

    #[tokio::test]
    async fn what_a_hook_after_a_tool_adds_goes_with_its_result() {
        let turns = vec![vec![bash("b1", "printf done")], vec![Item::Assistant("Ok.".into())]];
        let mut agent = agent("hook-after", turns);
        let adds = r#"cat >/dev/null; echo '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"formatted 2 files"}}'"#;
        with_hooks(
            &mut agent,
            "hook-after",
            json!({ "PostToolUse": [{ "matcher": "Bash", "hooks": [hook(adds)] }], "PostToolUseFailure": [{ "hooks": [hook("echo failed > failed.txt")] }] }),
        );
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        agent.send("go".into(), &mut person).await.unwrap();
        assert_eq!(result_of(&agent, "b1"), "done[exit code 0]\n[The user's hook cat, in its own words, not the user's:\nformatted 2 files]");
        assert!(!root_of("hook-after").join("failed.txt").exists(), "it didn't fail");
    }

    #[tokio::test]
    async fn a_hook_may_say_no_to_a_compaction_you_ask_for() {
        let mut agent = agent("hook-compact", vec![vec![Item::Assistant("A summary.".into())]]);
        with_hooks(&mut agent, "hook-compact", json!({ "PreCompact": [{ "matcher": "manual", "hooks": [hook("echo 'not mid-review' >&2; exit 2")] }] }));
        agent.history = vec![Item::User("first".into()), Item::Assistant("answer".into())];
        let mut person = Person { answer: Approval::Yes, log: Vec::new() };
        let report = agent.compact(None, true, &mut person).await.unwrap();
        assert!(!report.summarized && agent.model.requests.borrow().is_empty(), "nothing asked of the model");
        assert_eq!(agent.history.len(), 2, "the conversation as it was");
    }
}
