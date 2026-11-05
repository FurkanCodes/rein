//! Hooks: your own commands, run when something happens in a
//! conversation, as Claude Code runs them: before a tool runs, and after;
//! when you send a message; when a turn ends; as a session starts and
//! ends. A hook can say no, change a tool's input before rein decides on
//! it, or add what the model is to know. rtk rewrites commands so their
//! output comes back short, and codegraph adds the code your message is
//! about.
//!
//! What rein does better: the hooks on a tool run one after another, each
//! given the input as the one before left it, so two that rewrite a call
//! don't race (Claude Code runs them at once, and the last to finish wins);
//! a rewritten call is decided on as the model's own is, and rein's ask and
//! deny rules on the call as it was too; what a hook adds is said where
//! you see it, as its words, not yours.
//!
//! Your hooks run as you'd run them yourself, outside the sandbox, in every
//! mode, as Claude Code's do: they're yours, from your own settings, and
//! the walls are for what the model runs. Walled in, a hook couldn't open
//! its own script in `~/.claude/hooks`, Python's exit code for that (2)
//! read as a no, and every message was turned away in read-only mode.
//! Nothing the model runs can plant one: your settings, `~/.claude`, rein's
//! own folder and a project's `.claude` are out of its commands' reach in
//! every walled mode. A project's hooks, someone else's, don't run.

mod answer;
mod config;
mod named;
mod run;

pub use config::{Chosen, Command, From, Hook, LeftOut, State};

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use rein_llm::ToolCall;
use serde_json::{Value, json};

use crate::permissions::Mode;
use answer::Said;

/// The events a hook runs on: Claude Code's, those rein has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventName {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    Stop,
    StopFailure,
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
}

impl EventName {
    pub const ALL: [EventName; 14] = [
        EventName::SessionStart,
        EventName::SessionEnd,
        EventName::UserPromptSubmit,
        EventName::PreToolUse,
        EventName::PermissionRequest,
        EventName::PostToolUse,
        EventName::PostToolUseFailure,
        EventName::Notification,
        EventName::Stop,
        EventName::StopFailure,
        EventName::PreCompact,
        EventName::PostCompact,
        EventName::SubagentStart,
        EventName::SubagentStop,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            EventName::SessionStart => "SessionStart",
            EventName::SessionEnd => "SessionEnd",
            EventName::UserPromptSubmit => "UserPromptSubmit",
            EventName::PreToolUse => "PreToolUse",
            EventName::PermissionRequest => "PermissionRequest",
            EventName::PostToolUse => "PostToolUse",
            EventName::PostToolUseFailure => "PostToolUseFailure",
            EventName::Notification => "Notification",
            EventName::Stop => "Stop",
            EventName::StopFailure => "StopFailure",
            EventName::PreCompact => "PreCompact",
            EventName::PostCompact => "PostCompact",
            EventName::SubagentStart => "SubagentStart",
            EventName::SubagentStop => "SubagentStop",
        }
    }

    /// The event called `name`, as a settings file names it.
    pub fn of(name: &str) -> Option<EventName> {
        EventName::ALL.into_iter().find(|event| event.as_str() == name)
    }

    /// Whether plain output is context for the model, as Claude Code has
    /// it: on these two, where what a hook says is for the conversation.
    fn takes_plain_output(self) -> bool {
        matches!(self, EventName::SessionStart | EventName::UserPromptSubmit)
    }

    /// Whether it's about a tool's call: only then does a hook's `if` count.
    fn of_a_tool(self) -> bool {
        matches!(self, EventName::PreToolUse | EventName::PermissionRequest | EventName::PostToolUse | EventName::PostToolUseFailure)
    }

    /// How long a hook on it may take when it doesn't say, as Claude
    /// Code's do: a message waits half a minute at most, and leaving less.
    fn timeout(self) -> Duration {
        match self {
            EventName::UserPromptSubmit => Duration::from_secs(30),
            EventName::SessionEnd => Duration::from_millis(1500),
            _ => Duration::from_secs(600),
        }
    }
}

/// What started a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Startup,
    Resume,
    Clear,
    Compact,
}

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Clear,
    Resume,
    Exit,
}

/// What rein needs you for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// It's asking something.
    Permission,
    /// It's done, and waits for you.
    Idle,
}

/// What started a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Manual,
    Auto,
}

/// Something that happened, which hooks may run on.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    SessionStart(Source),
    SessionEnd(Ended),
    UserPromptSubmit(&'a str),
    PreToolUse(&'a ToolCall),
    PermissionRequest(&'a ToolCall),
    /// A tool ran, `failed` or not, giving `output` in `took`.
    PostToolUse {
        call: &'a ToolCall,
        output: &'a str,
        took: Duration,
        failed: bool,
    },
    Notification(Notice, &'a str),
    /// The model is done: `again` when a hook kept it going already.
    Stop {
        again: bool,
        last: Option<&'a str>,
    },
    /// The turn ended on an error, of `kind`.
    StopFailure {
        kind: &'a str,
        error: &'a str,
    },
    PreCompact {
        trigger: Trigger,
        focus: Option<&'a str>,
    },
    PostCompact {
        trigger: Trigger,
        summary: &'a str,
    },
    SubagentStart {
        id: &'a str,
        role: &'a str,
    },
    SubagentStop {
        id: &'a str,
        role: &'a str,
        last: Option<&'a str>,
    },
}

impl Event<'_> {
    pub fn name(&self) -> EventName {
        match self {
            Event::SessionStart(_) => EventName::SessionStart,
            Event::SessionEnd(_) => EventName::SessionEnd,
            Event::UserPromptSubmit(_) => EventName::UserPromptSubmit,
            Event::PreToolUse(_) => EventName::PreToolUse,
            Event::PermissionRequest(_) => EventName::PermissionRequest,
            Event::PostToolUse { failed: false, .. } => EventName::PostToolUse,
            Event::PostToolUse { failed: true, .. } => EventName::PostToolUseFailure,
            Event::Notification(..) => EventName::Notification,
            Event::Stop { .. } => EventName::Stop,
            Event::StopFailure { .. } => EventName::StopFailure,
            Event::PreCompact { .. } => EventName::PreCompact,
            Event::PostCompact { .. } => EventName::PostCompact,
            Event::SubagentStart { .. } => EventName::SubagentStart,
            Event::SubagentStop { .. } => EventName::SubagentStop,
        }
    }

    fn call(&self) -> Option<&ToolCall> {
        match self {
            Event::PreToolUse(call) | Event::PermissionRequest(call) | Event::PostToolUse { call, .. } => Some(call),
            _ => None,
        }
    }

    /// What a matcher matches, as Claude Code's do: a tool's name, what
    /// started a session, and so on. `None` where every hook runs.
    fn target(&self) -> Option<String> {
        let word = match self {
            Event::PreToolUse(call) | Event::PermissionRequest(call) | Event::PostToolUse { call, .. } => return Some(named::name(call)),
            Event::SessionStart(source) => match source {
                Source::Startup => "startup",
                Source::Resume => "resume",
                Source::Clear => "clear",
                Source::Compact => "compact",
            },
            Event::SessionEnd(ended) => match ended {
                Ended::Clear => "clear",
                Ended::Resume => "resume",
                Ended::Exit => "prompt_input_exit",
            },
            Event::Notification(notice, _) => match notice {
                Notice::Permission => "permission_prompt",
                Notice::Idle => "idle_prompt",
            },
            Event::PreCompact { trigger, .. } | Event::PostCompact { trigger, .. } => match trigger {
                Trigger::Manual => "manual",
                Trigger::Auto => "auto",
            },
            Event::StopFailure { kind, .. } => kind,
            Event::SubagentStart { role, .. } | Event::SubagentStop { role, .. } => role,
            Event::UserPromptSubmit(_) | Event::Stop { .. } => return None,
        };
        Some(word.to_owned())
    }
}

/// Where hooks run, and what they're told of it.
pub struct Here<'a> {
    pub root: &'a Path,
    pub session_id: Option<&'a str>,
    /// The conversation's file: rein's own, not Claude Code's.
    pub transcript: Option<&'a Path>,
    pub mode: Mode,
}

/// What one hook said, by its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct By {
    pub hook: String,
    pub said: String,
}

impl By {
    fn new(hook: &Hook, said: impl Into<String>) -> By {
        By { hook: hook.name(), said: said.into() }
    }
}

/// What the hooks on an event said, together.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Heard {
    /// Those that ran, by name.
    pub ran: Vec<String>,
    /// No: the first that said so, and why.
    pub no: Option<By>,
    /// Ask the user first.
    pub ask: Option<By>,
    /// Yes without asking.
    pub yes: Option<String>,
    /// The tool's call as it runs now, and which changed it.
    pub call: Option<(ToolCall, Vec<String>)>,
    /// What the model is to know.
    pub context: Vec<By>,
    /// What the user is shown.
    pub messages: Vec<By>,
    /// Stop altogether.
    pub stop: Option<By>,
    /// The tool's result, as one would have it.
    pub output: Option<By>,
    /// Those that didn't work, and why.
    pub failed: Vec<By>,
}

/// How a hook last ran, for `/hooks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Last {
    pub at: SystemTime,
    pub took: Duration,
    /// What came of it, in a few words.
    pub came: String,
}

/// The most of what one hook adds that the model is given, as Claude
/// Code gives: more is cut, and said so.
pub const MOST_SAID: usize = 10_000;

/// How a note of what a hook said starts, in the conversation: it's the
/// hook's words, not the user's.
const SAID: &str = "[The user's hook ";

/// `text` is a note of what a hook said, not something you said.
pub fn is_note(text: &str) -> bool {
    text.starts_with(SAID)
}

/// A note of what `by` said, for the model: its words, marked as its own.
pub fn note(by: &By) -> String {
    format!("{SAID}{}, in its own words, not the user's:\n{}]", by.hook, by.said)
}

/// A note that `by` says the work isn't done, and why: the turn goes on.
pub fn not_done(by: &By) -> String {
    format!("{SAID}{} says the work isn't done yet, in its own words, not the user's:\n{}]", by.hook, by.said)
}

/// The hooks there are, and how each last ran.
#[derive(Default)]
pub struct Hooks {
    hooks: Vec<Hook>,
    left_out: Vec<LeftOut>,
    last: Mutex<HashMap<String, Last>>,
    /// What was said once already: a hook that doesn't work.
    told: Mutex<HashSet<String>>,
    /// Those running in the background, stopped when rein lets them go.
    background: Mutex<tokio::task::JoinSet<()>>,
}

impl Hooks {
    /// The hooks of rein's own file in `rein`, Claude Code's settings and
    /// plugins in `home`, and the project at `root`'s, as you `chosen`.
    pub fn load(rein: &Path, home: Option<&Path>, root: &Path, chosen: &Chosen) -> Hooks {
        let found = config::find(rein, home, root, chosen);
        Hooks { hooks: found.hooks, left_out: found.left_out, ..Hooks::default() }
    }

    /// Every hook found, those that don't run too.
    pub fn all(&self) -> &[Hook] {
        &self.hooks
    }

    /// The hooks that couldn't be read, and why.
    pub fn left_out(&self) -> &[LeftOut] {
        &self.left_out
    }

    /// How `hook` last ran, if it has.
    pub fn last(&self, hook: &Hook) -> Option<Last> {
        lock(&self.last).get(&hook.id()).cloned()
    }

    /// Whether any hook runs on `event`: if not, nothing need be made for one.
    pub fn any(&self, event: EventName) -> bool {
        self.hooks.iter().any(|hook| hook.event == event && hook.runs())
    }

    /// Whether a hook runs on `event`, in the project at `root`.
    pub fn runs_on(&self, event: &Event<'_>, root: &Path) -> bool {
        self.any(event.name()) && !self.matching(event, root).is_empty()
    }

    /// Whether `by` hasn't been said before: a hook that doesn't work is
    /// said once, not at every call.
    pub fn first_time(&self, by: &By) -> bool {
        lock(&self.told).insert(format!("{}\n{}", by.hook, by.said))
    }

    /// The hooks that run on `event`, in order: on it, for what it's about,
    /// and, about a tool, fitting their `if`.
    fn matching(&self, event: &Event<'_>, root: &Path) -> Vec<&Hook> {
        let (name, target) = (event.name(), event.target());
        self.hooks
            .iter()
            .filter(|hook| hook.event == name && hook.runs())
            .filter(|hook| target.as_deref().is_none_or(|target| hook.matches.matches(target)))
            .filter(|hook| match &hook.only_if {
                Some(rule) => event.call().is_some_and(|call| name.of_a_tool() && fits(rule, call, root)),
                None => true,
            })
            .collect()
    }

    /// Runs the hooks on `event`, `here`: what they said, together. Those
    /// on a tool run in turn, each given the call as the one before left
    /// it, and the first that says no stops the rest; the others run at
    /// once.
    pub async fn on(&self, event: Event<'_>, here: &Here<'_>) -> Heard {
        let name = event.name();
        let mut heard = Heard::default();
        let hooks = self.matching(&event, here.root);
        if hooks.is_empty() {
            return heard;
        }
        if name == EventName::PreToolUse {
            let Event::PreToolUse(call) = event else { return heard };
            let mut current = call.clone();
            for hook in hooks {
                let said = self.run_one(hook, &input(&Event::PreToolUse(&current), here), here).await;
                heard.ran.push(hook.name());
                if let Some(updated) = &said.input {
                    match named::rewritten(&current, updated, here.root) {
                        Ok(rewritten) if rewritten.arguments != current.arguments => {
                            current = rewritten;
                            let (_, by) = heard.call.get_or_insert_with(|| (current.clone(), Vec::new()));
                            by.push(hook.name());
                        }
                        Ok(_) => {}
                        Err(why) => heard.failed.push(By::new(hook, why)),
                    }
                }
                let stops = said.no.is_some() || said.stop.is_some();
                gather(&mut heard, hook, said);
                if stops {
                    break;
                }
            }
            if let Some((call, _)) = &mut heard.call {
                call.clone_from(&current);
            }
            return heard;
        }
        let given = input(&event, here);
        let runs = hooks.iter().map(|hook| self.run_one(hook, &given, here)).collect();
        for (hook, said) in hooks.iter().zip(crate::agent::all(runs).await) {
            heard.ran.push(hook.name());
            gather(&mut heard, hook, said);
        }
        heard
    }

    /// Runs `hook` with `given`, or in the background, if it says so.
    async fn run_one(&self, hook: &Hook, given: &Value, here: &Here<'_>) -> Said {
        let bytes = given.to_string().into_bytes();
        let timeout = hook.timeout.unwrap_or_else(|| hook.event.timeout());
        if hook.background {
            let (hook, root) = (hook.clone(), here.root.to_owned());
            lock(&self.background).spawn(async move {
                run::run(&hook.command, &hook.from, &bytes, &root, timeout).await;
            });
            return Said::default();
        }
        let ran = run::run(&hook.command, &hook.from, &bytes, here.root, timeout).await;
        let said = answer::read(hook.event, &ran);
        let came = came(&said);
        lock(&self.last).insert(hook.id(), Last { at: SystemTime::now(), took: ran.took, came });
        said
    }
}

/// What a hook said, in a few words, for `/hooks`.
fn came(said: &Said) -> String {
    if let Some(why) = &said.failed {
        return format!("didn't work: {why}");
    }
    if let Some(why) = &said.no {
        return format!("said no: {}", first_line(why));
    }
    let mut came = Vec::new();
    if said.input.is_some() {
        came.push("changed the call".to_owned());
    }
    if let Some(context) = &said.context {
        came.push(format!("added {} characters", context.chars().count()));
    }
    if said.ask.is_some() {
        came.push("asked for a yes".into());
    }
    if came.is_empty() { "fine".into() } else { came.join(", ") }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// Adds what `hook` said to what's `heard`.
fn gather(heard: &mut Heard, hook: &Hook, said: Said) {
    if let Some(why) = said.failed {
        heard.failed.push(By::new(hook, why));
    }
    if let Some(why) = said.no {
        heard.no.get_or_insert_with(|| By::new(hook, why));
    }
    if let Some(why) = said.ask {
        heard.ask.get_or_insert_with(|| By::new(hook, why));
    }
    if said.yes {
        heard.yes.get_or_insert_with(|| hook.name());
    }
    if let Some(context) = said.context {
        heard.context.push(By::new(hook, cut(&context)));
    }
    if let Some(message) = said.message {
        heard.messages.push(By::new(hook, cut(&message)));
    }
    if let Some(why) = said.stop {
        heard.stop.get_or_insert_with(|| By::new(hook, why));
    }
    if let Some(output) = said.output {
        heard.output = Some(By::new(hook, output));
    }
}

/// `text` cut to [`MOST_SAID`] characters, saying so.
fn cut(text: &str) -> String {
    let count = text.chars().count();
    if count <= MOST_SAID {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MOST_SAID).collect();
    format!("{kept}\n[{} more characters cut: a hook gives the model at most {MOST_SAID}]", count - MOST_SAID)
}

/// The name a hook knows `call`'s tool by: Claude Code's for rein's own.
pub fn tool_name(call: &ToolCall) -> String {
    named::name(call)
}

/// The mode in a word, as Claude Code's hooks are told it.
fn permission_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Trust => "bypassPermissions",
        Mode::Ask => "default",
        Mode::Edits => "acceptEdits",
        Mode::Auto => "dontAsk",
        Mode::ReadOnly => "plan",
    }
}

/// What a hook on `event` is given, as Claude Code gives it.
fn input(event: &Event<'_>, here: &Here<'_>) -> Value {
    let mut given = json!({
        "session_id": here.session_id.unwrap_or_default(),
        "transcript_path": here.transcript.map(|path| path.display().to_string()).unwrap_or_default(),
        "cwd": here.root.display().to_string(),
        "hook_event_name": event.name().as_str(),
        "permission_mode": permission_mode(here.mode),
    });
    let fields = match event {
        Event::SessionStart(_) => json!({ "source": event.target() }),
        Event::SessionEnd(_) => json!({ "reason": event.target() }),
        Event::UserPromptSubmit(prompt) => json!({ "prompt": prompt }),
        Event::PreToolUse(call) => json!({ "tool_name": named::name(call), "tool_input": named::input(call, here.root), "tool_use_id": call.call_id }),
        Event::PermissionRequest(call) => json!({ "tool_name": named::name(call), "tool_input": named::input(call, here.root) }),
        Event::PostToolUse { call, output, took, failed } => {
            let mut fields = json!({ "tool_name": named::name(call), "tool_input": named::input(call, here.root), "tool_use_id": call.call_id, "duration_ms": took.as_millis() });
            if *failed {
                fields["error"] = json!(output);
            } else {
                fields["tool_response"] = named::response(call, output, here.root);
            }
            fields
        }
        Event::Notification(_, message) => json!({ "message": message, "title": "rein", "notification_type": event.target() }),
        Event::Stop { again, last } => json!({ "stop_hook_active": again, "last_assistant_message": last }),
        Event::StopFailure { kind, error } => json!({ "error": kind, "error_details": error }),
        Event::PreCompact { focus, .. } => json!({ "trigger": event.target(), "custom_instructions": focus.unwrap_or_default() }),
        Event::PostCompact { summary, .. } => json!({ "trigger": event.target(), "compact_summary": summary }),
        Event::SubagentStart { id, role } => json!({ "agent_id": id, "agent_type": role }),
        Event::SubagentStop { id, role, last } => json!({ "agent_id": id, "agent_type": role, "stop_hook_active": false, "last_assistant_message": last }),
    };
    if let (Value::Object(given), Value::Object(fields)) = (&mut given, fields) {
        given.extend(fields);
    }
    given
}

/// Whether `call` fits `rule`, a permission rule as a hook's `if` gives
/// it: `Bash(git *)`, `Edit(src/**)`, `Read`. One rein can't read lets the
/// hook run, as Claude Code does.
fn fits(rule: &str, call: &ToolCall, root: &Path) -> bool {
    let rule = rule.trim();
    let (tool, pattern) = match rule.split_once('(') {
        Some((tool, rest)) => match rest.strip_suffix(')') {
            Some(pattern) => (tool.trim(), Some(pattern.trim())),
            None => return true,
        },
        None => (rule, None),
    };
    if named::name(call) != tool {
        return false;
    }
    let Some(pattern) = pattern.filter(|pattern| !pattern.is_empty()) else { return true };
    let given = named::input(call, root);
    if let Some(command) = given.get("command").and_then(Value::as_str) {
        // Each part of a line, as Claude Code checks each.
        let parts = command.split(['&', '|', ';']).map(str::trim).filter(|part| !part.is_empty());
        return std::iter::once(command).chain(parts).any(|part| glob(pattern, part));
    }
    if let Some(path) = given.get("file_path").and_then(Value::as_str) {
        let relative = Path::new(path).strip_prefix(root).map_or_else(|_| path.to_owned(), |rest| rest.display().to_string());
        return glob(pattern, path) || glob(pattern, &relative);
    }
    true
}

/// Whether `text` fits `pattern`, where `*` is anything and a closing
/// `:*` is any end, as Claude Code's rules have it.
fn glob(pattern: &str, text: &str) -> bool {
    let pattern = pattern.strip_suffix(":*").map_or_else(|| pattern.to_owned(), |start| format!("{start}*"));
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    // best[j]: whether pattern[..i] fits text[..j].
    let mut best = vec![false; text.len() + 1];
    best[0] = true;
    for &p in &pattern {
        let mut next = vec![false; text.len() + 1];
        if p == '*' {
            let mut any = false;
            for j in 0..=text.len() {
                any |= best[j];
                next[j] = any;
            }
        } else {
            for j in 1..=text.len() {
                next[j] = best[j - 1] && text[j - 1] == p;
            }
        }
        best = next;
    }
    best[text.len()]
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
