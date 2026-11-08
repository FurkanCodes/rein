//! What rein shows a client instead of a screen: the agent's [`Ui`], as
//! `session/update`s, and its questions as `session/request_permission`.

use std::collections::HashSet;

use rein_agent::agent::{Approval, Compacted, Heard, Ui};
use rein_agent::compact;
use rein_agent::peers::SpawnArgs;
use rein_agent::permissions::Rule;
use rein_agent::tools::{ERROR, ToolName};
use rein_agent::undo::Watched;
use rein_llm::{Item, ToolCall, Usage};
use serde_json::{Value, json};

use super::{Link, wire};
use crate::input::{Menu, Pending, Question, StandIn};
use crate::peers::Node;
use crate::peers::talk::{self, Child};
use crate::terminal::Terminal;
use crate::ui::asked::{Asked, INSTEAD};
use crate::ui::screen::{self, Taken};
use crate::ui::steps;
use crate::ui::style::{self, GUTTER};

const ALLOW: &str = "allow";
const ALWAYS: &str = "always";
const REJECT: &str = "reject";
const STOP: &str = "stop";

/// The client, as the agent sees it.
pub(super) struct Served {
    session: String,
    link: Link,
    /// Calls the client knows of: one is announced once, and updated after.
    announced: HashSet<String>,
    /// What a rein at a terminal has too, which draws nothing here: what
    /// setting up reported to, the other reins, and the tasks it owes.
    pub(super) terminal: Terminal,
    /// The model has said something since its last step: more is the same
    /// answer.
    answering: bool,
    /// What the model writes now is the conversation's summary, not an answer.
    compacting: bool,
    /// The call just turned down: all of it's been said, and what it came
    /// to comes round again as any step's does.
    blocked: Option<String>,
    /// The folder it works in.
    root: std::path::PathBuf,
    /// What the client was last told, so that it's told only what's new.
    options_said: Option<Value>,
    state_said: Option<String>,
    commands_said: Option<Vec<Value>>,
    /// How many checks have run, which each go as a step of their own.
    checks: usize,
}

/// A place kept for a sub-agent that hasn't claimed its task, given up
/// when this is dropped. In a `Drop`: a turn stopped while a client takes
/// its time is this future dropped, and the place would be kept for good.
struct Awaited<'a> {
    node: &'a Node,
    id: &'a str,
}

impl Drop for Awaited<'_> {
    fn drop(&mut self) {
        self.node.forget_child(self.id);
    }
}

/// What a command says, as a client wants it: it's laid out for a terminal,
/// under a gutter.
fn plainly(text: &str) -> String {
    let pad = " ".repeat(GUTTER);
    style::strip(text).split_inclusive('\n').map(|line| line.strip_prefix(pad.as_str()).unwrap_or(line)).collect()
}

/// Asks the client what the box would ask you.
pub(super) struct Asker {
    pub(super) session: String,
    pub(super) link: Link,
}

impl Asker {
    /// Sends what `asking` writes under a number, and waits for what comes
    /// back. A client that can't show it has said so as it started, and it's
    /// as if you'd changed your mind.
    fn ask(&self, asking: impl FnOnce(u64) -> String) -> Pending<Option<Value>> {
        if !self.link.can_ask.get() {
            say!("This asks something this client can't show. Say it on the line, like /model gpt-5.");
            return Box::pin(std::future::ready(None));
        }
        let (id, answer) = self.link.ask();
        self.link.send(asking(id));
        Box::pin(async move { answer.await.ok().flatten() })
    }
}

impl StandIn for Asker {
    fn can_ask(&self) -> bool {
        self.link.can_ask.get()
    }

    fn choose(&self, menu: &Menu) -> Pending<Option<usize>> {
        let answer = self.ask(|id| wire::choose(id, &self.session, menu));
        Box::pin(async move { usize::try_from(answer.await?["picked"].as_u64()?).ok() })
    }

    fn read(&self, prompt: &str, secret: bool) -> Pending<Option<String>> {
        let answer = self.ask(|id| wire::read(id, &self.session, prompt, secret));
        Box::pin(async move { answer.await?["text"].as_str().map(str::to_owned) })
    }

    fn ask(&self, question: &Question) -> Pending<Option<String>> {
        let answer = self.ask(|id| wire::yes(id, &self.session, question));
        Box::pin(async move { answer.await?["key"].as_str().map(str::to_owned) })
    }
}

impl Served {
    pub(super) fn new(session: String, link: Link, terminal: Terminal) -> Served {
        let root = std::env::current_dir().unwrap_or_default();
        Served {
            session,
            link,
            announced: HashSet::new(),
            terminal,
            answering: false,
            compacting: false,
            blocked: None,
            root,
            options_said: None,
            state_said: None,
            commands_said: None,
            checks: 0,
        }
    }

    /// Says what's changed of what can be set, if anything has.
    pub(super) fn options(&mut self, options: Value) {
        if self.options_said.as_ref() != Some(&options) {
            self.link.send(wire::options(&self.session, &options));
            self.options_said = Some(options);
        }
    }

    /// Says what's changed of the branch, your limits and what's running
    /// in the background, if anything has.
    pub(super) fn state(&mut self) {
        let line =
            wire::state(&self.session, crate::peers::git_branch(&self.root).as_deref(), &self.terminal.meters.cells(std::time::SystemTime::now()), self.link.in_the_background());
        if self.state_said.as_ref() != Some(&line) {
            self.link.send(line.clone());
            self.state_said = Some(line);
        }
    }

    /// Your limits as they stand: what it has to say of them goes to the
    /// client, with or without a turn.
    pub(super) fn meters(&mut self, meters: &[rein_llm::Meter]) {
        {
            let _noted = self.take_noted();
            self.terminal.take_meters(meters.to_vec());
        }
        self.state();
    }

    /// While this is held, what commands say goes to the client.
    pub(super) fn take_said(&self) -> Taken {
        let (out, session) = (self.link.out.clone(), self.session.clone());
        screen::take_said(Box::new(move |text| {
            // Gone only once the client has stopped reading.
            let _ = out.send(wire::said(&session, &plainly(text)));
        }))
    }

    /// While this is held, what commands say goes nowhere: a client's own
    /// control shows what it came to.
    pub(super) fn leave_unsaid(&self) -> Taken {
        screen::take_said(Box::new(|_| {}))
    }

    /// While this is held, what commands say goes to the client as notes.
    fn take_noted(&self) -> Taken {
        let (out, session) = (self.link.out.clone(), self.session.clone());
        screen::take_said(Box::new(move |text| {
            // Gone only once the client has stopped reading.
            let _ = out.send(wire::noted(&session, plainly(text).trim_end()));
        }))
    }

    /// The line that ends a turn that did something, if this one did.
    pub(super) fn turn_ended(&mut self, undo: Option<usize>) {
        self.terminal.end_turn();
        if let Some(summary) = self.terminal.turn.summary() {
            self.link.send(wire::turn_line(&self.session, &summary, undo));
        }
    }

    /// Says what the plan is, as ACP has it, if `call` is the model's plan.
    fn plan(&self, call: &ToolCall) {
        if let Some(plan) = plan_of(call) {
            self.update(&plan);
        }
    }

    /// Shows a client that shows its own what you ran, as rein has it: it
    /// may have had a key in it.
    pub(super) fn ran(&self, line: &str) {
        if self.link.can_ask.get() {
            self.link.send(wire::yours(&self.session, &crate::commands::shown(line)));
        }
    }

    /// Says what can be run here, if that's changed: rein's commands, and
    /// the skills you can call the same way, which come and go as files do.
    pub(super) fn commands(&mut self, skills: &[crate::ui::slash::Offer]) {
        let hint = |takes: &str| (!takes.is_empty()).then(|| json!({ "hint": takes }));
        let ours = crate::commands::COMMANDS.iter().map(|(name, takes, does)| json!({ "name": name.trim_start_matches('/'), "description": does, "input": hint(takes) }));
        let theirs = skills.iter().map(|skill| wire::skill(skill.name.trim_start_matches('/'), &skill.about));
        let commands: Vec<Value> = ours.chain(theirs).collect();
        if self.commands_said.as_ref() != Some(&commands) {
            self.link.send(wire::commands(&self.session, &commands));
            self.commands_said = Some(commands);
        }
    }

    /// A turn starts.
    pub(super) fn starting(&mut self) {
        self.terminal.start_turn(None);
        self.answering = false;
        self.compacting = false;
    }

    fn update(&self, update: &Value) {
        self.link.send(wire::update(&self.session, update));
    }

    fn chunk(&self, kind: &str, text: &str) {
        self.update(&json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }));
    }

    /// Tells the client of `call`, with what it would change, or that it's
    /// now `status`.
    fn announce(&mut self, call: &ToolCall, status: &str, change: Option<&steps::Whole>) {
        if self.announced.insert(call.call_id.clone()) {
            let mut step = tool_call(call, status);
            if let Some(change) = change {
                step["content"] = json!([wire::diff(change)]);
            }
            self.update(&step);
        } else {
            self.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": call.call_id, "status": status }));
        }
    }

    fn finish(&mut self, call: &ToolCall, status: &str, said: &str) {
        if !self.announced.remove(&call.call_id) {
            self.update(&tool_call(call, status));
        }
        self.update(&summed(result_of(&call.call_id, status, said), call, said));
    }

    fn used(&self, tokens: u64) {
        self.update(&json!({ "sessionUpdate": "usage_update", "used": tokens, "size": self.terminal.context_limit }));
    }

    /// Says the conversation so far, as `session/load` asks: what you typed,
    /// what other reins said, the model's answers and its steps. What rein
    /// added for the model isn't anything you said, and isn't shown.
    pub(super) fn replay(&self, items: &[Item]) {
        let mut calls = std::collections::HashMap::new();
        for item in items {
            match item {
                Item::User(_) => {
                    if let Some(said) = compact::typed(item) {
                        match rein_agent::peers::unheard(said) {
                            Some((from, text)) => self.link.send(wire::heard(&self.session, from, text)),
                            None => self.link.send(wire::yours(&self.session, said)),
                        }
                    }
                }
                Item::Assistant(text) => self.chunk("agent_message_chunk", text),
                // Failed, until its result says otherwise: a turn cut off
                // may have left it with none.
                Item::ToolCall(call) => {
                    self.update(&tool_call(call, "failed"));
                    self.plan(call);
                    calls.insert(call.call_id.as_str(), call);
                }
                Item::ToolResult { call_id, output } => {
                    let result = result_of(call_id, finished(output), output);
                    self.update(&match calls.get(call_id.as_str()) {
                        Some(call) => summed(result, call, output),
                        None => result,
                    });
                }
                Item::Found { .. } | Item::Image { .. } | Item::Opaque(_) => {}
            }
        }
    }

    /// Asks whether `call` may go ahead, in the terminal's words. Only what's
    /// asked about shows its change: a whole file on every step, and a look
    /// at it for each one a saved conversation has, would be for nothing.
    async fn asked(&mut self, call: &ToolCall, asked: &Asked) -> Approval {
        self.announce(call, "pending", steps::whole_change(call, &self.root).as_ref());
        let mut options = vec![
            json!({ "optionId": ALLOW, "name": "Yes", "kind": "allow_once" }),
            json!({ "optionId": ALWAYS, "name": asked.always, "kind": "allow_always" }),
            json!({ "optionId": REJECT, "name": "No", "kind": "reject_once" }),
        ];
        // What comes of a sub-agent is words for the model, which stop nothing.
        if ToolName::of(call) != Some(ToolName::Spawn) {
            options.push(json!({ "optionId": STOP, "name": INSTEAD, "kind": "reject_once", "_meta": { wire::INSTEAD: true } }));
        }
        let (id, answer) = self.link.ask();
        self.link.send(wire::ask_permission(id, &self.session, &call.call_id, &Value::Array(options), asked));
        // Never answered, as the client left: not a yes.
        answer.await.map_or(Approval::Stop, |result| approval(result.as_ref()))
    }

    /// What the model asked for with `spawn`: you're asked, in a mode that
    /// asks, then the client starts a `rein serve` for it, which claims the
    /// task. What the model
    /// is told of how that went.
    async fn spawn(&mut self, call: &ToolCall) -> String {
        match serde_json::from_str(&call.arguments) {
            Ok(args) => self.start_sub_agent(args, Some(call)).await,
            Err(e) => format!("{ERROR}bad arguments: {e}"),
        }
    }

    /// `/spawn`: you asked for it, so nothing asks. What came of it.
    pub(super) async fn spawn_yours(&mut self, args: SpawnArgs) -> String {
        self.start_sub_agent(args, None).await
    }

    /// Has the client start a sub-agent, asking you first about what `call`
    /// asked for, if the mode asks
    /// ([`rein_agent::permissions::Mode::asks_to_start_sub_agents`]).
    async fn start_sub_agent(&mut self, args: SpawnArgs, call: Option<&ToolCall>) -> String {
        let Some(node) = &self.terminal.peers else { return format!("{ERROR}this rein isn't among the others, so it can't start sub-agents") };
        let child = match Child::wanted(&args, &self.terminal.spec, node).await {
            Ok(child) => child,
            Err(e) => return format!("{ERROR}{e}"),
        };
        if let Some(call) = call.filter(|_| self.terminal.mode.asks_to_start_sub_agents() && !self.terminal.spawn_always) {
            let asked = Asked::of(call, None, self.terminal.sandboxing);
            match self.asked(call, &asked).await {
                Approval::Yes => {}
                Approval::Always => self.terminal.spawn_always = true,
                _ => return "The user declined to start this sub-agent.".to_owned(),
            }
        }
        let Some(node) = &self.terminal.peers else { return format!("{ERROR}this rein left the others") };
        let id = talk::task_id();
        if let Err(e) = node.expect_child(&id, &child.name, &args.task) {
            return format!("{ERROR}{e}");
        }
        let _awaited = Awaited { node, id: &id };
        let flags = child.flags(&id, args.edits, self.terminal.mode, self.terminal.sandboxing);
        let serve: Vec<String> = std::iter::once("serve".to_owned()).chain(flags).collect();
        let command = talk::rein_program();
        let env = talk::carried();
        let pane = wire::Pane { name: &child.name, model: &child.model, command: &command, args: &serve, cwd: &node.info().root, env: &env };
        let (asked, answer) = self.link.ask();
        self.link.send(wire::open_pane(asked, &self.session, &pane));
        // A client answers once the sub-agent's `session/new` has, so it has
        // its task by now. If it hasn't, nothing will come of it, and the
        // model is told so now: its result would never come.
        if let Ok(Some(_)) = answer.await
            && node.claimed(&id)
        {
            return format!("Started sub-agent {} ({}) in a pane of its own (task {id}). {}", child.name, child.model, talk::AWAITED);
        }
        format!("{ERROR}the client didn't open a pane for sub-agent {}", child.name)
    }
}

/// The word for how a step went, from what it gave.
fn finished(output: &str) -> &'static str {
    if rein_agent::agent::failed(output) { "failed" } else { "completed" }
}

/// ACP's plan, from the model's own: each step, and how far it's got.
fn plan_of(call: &ToolCall) -> Option<Value> {
    if ToolName::of(call) != Some(ToolName::Plan) {
        return None;
    }
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let entries: Vec<Value> = args["steps"]
        .as_array()?
        .iter()
        .map(|step| {
            let status = match step["status"].as_str() {
                Some("done") => "completed",
                Some("in_progress") => "in_progress",
                _ => "pending",
            };
            json!({ "content": step["step"], "priority": "medium", "status": status })
        })
        .collect();
    Some(json!({ "sessionUpdate": "plan", "entries": entries }))
}

/// What a step came to, short, as the terminal says it on the step's line:
/// `exit 101`, how big a change is, how many lines a read brought in.
const SUMMARY: &str = "io.github.furkancodes/summary";
/// What a check after edits came to, in fields, for a panel to say.
const CHECKED: &str = "io.github.furkancodes/checked";

/// `result` with what `call` came to, in short, in its `_meta`.
fn summed(mut result: Value, call: &ToolCall, output: &str) -> Value {
    let outcome = steps::outcome(call, output, style::Style::plain());
    let mut summary = json!({ "status": outcome.status, "said": outcome.said.trim_start_matches(" · ") });
    if outcome.ok == Some(false) {
        summary["failed"] = json!(true);
    }
    match steps::change_sizes(call) {
        Some(steps::Sizes::Edited { added, removed }) => {
            summary["added"] = json!(added);
            summary["removed"] = json!(removed);
        }
        Some(steps::Sizes::Written(lines)) => summary["lines"] = json!(lines),
        None => {}
    }
    if let Some(lines) = steps::lines_read(call, output) {
        summary["lines"] = json!(lines);
    }
    result["_meta"] = json!({ SUMMARY: summary });
    result
}

/// The update saying what the call `id` came to.
fn result_of(id: &str, status: &str, said: &str) -> Value {
    let content = [json!({ "type": "content", "content": { "type": "text", "text": said } })];
    json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": status, "content": content })
}

fn tool_call(call: &ToolCall, status: &str) -> Value {
    let (verb, what) = crate::ui::steps::describe(call);
    let input = serde_json::from_str(&call.arguments).unwrap_or_else(|_| Value::String(call.arguments.clone()));
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call.call_id,
        "title": format!("{verb} {what}"),
        "name": call.name,
        "kind": kind(call),
        "status": status,
        "rawInput": input,
    })
}

/// ACP's word for what a call does, which clients pick an icon by.
fn kind(call: &ToolCall) -> &'static str {
    match ToolName::of(call) {
        Some(ToolName::Read) => "read",
        Some(ToolName::Write | ToolName::Edit | ToolName::ApplyPatch) => "edit",
        Some(ToolName::Grep | ToolName::Glob | ToolName::Recall | ToolName::ToolSearch) => "search",
        Some(ToolName::Bash | ToolName::Background) => "execute",
        Some(ToolName::WebSearch) => "fetch",
        Some(ToolName::Plan) => "think",
        _ => "other",
    }
}

/// What a client answered, which is only a yes if it says so: a client
/// that says something else, or an error, hasn't said yes.
fn approval(result: Option<&Value>) -> Approval {
    let outcome = result.map(|result| &result["outcome"]);
    match outcome.and_then(|outcome| outcome["outcome"].as_str()) {
        Some("selected") => match outcome.and_then(|outcome| outcome["optionId"].as_str()) {
            Some(ALLOW) => Approval::Yes,
            Some(ALWAYS) => Approval::Always,
            Some(STOP) => Approval::Stop,
            _ => Approval::No,
        },
        Some("cancelled") => Approval::Stop,
        _ => Approval::No,
    }
}

impl Ui for Served {
    fn text(&mut self, delta: &str) {
        if self.compacting {
            self.chunk("agent_thought_chunk", delta);
            return;
        }
        if !std::mem::replace(&mut self.answering, true) {
            crate::peers::farewell::writing();
        }
        crate::peers::farewell::wrote(delta);
        self.chunk("agent_message_chunk", delta);
    }

    fn thought(&mut self, delta: &str) {
        self.chunk("agent_thought_chunk", delta);
    }

    fn note(&mut self, note: &str) {
        self.chunk("agent_thought_chunk", note);
    }

    fn meters(&mut self, meters: &[rein_llm::Meter]) {
        Served::meters(self, meters);
    }

    fn hooked(&mut self, _call: &ToolCall, what: &str) {
        self.link.send(wire::noted(&self.session, what));
    }

    fn background_ended(&mut self, ended: &rein_agent::background::Ended) {
        let command = rein_agent::background::plain(ended.command.lines().next().unwrap_or_default());
        self.link.send(wire::noted(&self.session, &format!("↳ {} {command} ended: {}", ended.id, ended.exit.said())));
        self.state();
    }

    fn checked(&mut self, checked: &rein_agent::check::Checked, printed: &str) {
        self.checks += 1;
        let id = format!("check-{}", self.checks);
        let status = if matches!(checked.verdict, rein_agent::check::Verdict::New(_)) { "failed" } else { "completed" };
        let title = format!("Check {}", crate::terminal::check_said(checked));
        self.update(&json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "other", "status": status }));
        let (verdict, count) = match checked.verdict {
            rein_agent::check::Verdict::Clean => ("clean", 0),
            rein_agent::check::Verdict::New(count) => ("new", count),
            rein_agent::check::Verdict::Fixed => ("fixed", 0),
            rein_agent::check::Verdict::Busy => ("busy", 0),
        };
        let mut result = result_of(&id, status, printed);
        result["_meta"] = json!({ CHECKED: { "name": checked.name, "verdict": verdict, "count": count, "tookMs": checked.took.as_millis(), "before": checked.before } });
        self.update(&result);
    }

    async fn approve(&mut self, call: &ToolCall, rule: Option<&Rule>) -> Approval {
        let asked = Asked::of(call, rule, self.terminal.sandboxing);
        let answer = self.asked(call, &asked).await;
        if answer == Approval::Always {
            self.link.send(wire::noted(&self.session, &asked.wont_ask_again()));
        }
        answer
    }

    async fn peer(&mut self, call: &ToolCall) -> String {
        if ToolName::of(call) == Some(ToolName::Spawn) {
            return self.spawn(call).await;
        }
        match &self.terminal.peers {
            Some(peers) => peers.tool(call).await,
            None => format!("{ERROR}this rein isn't among the others"),
        }
    }

    fn tool_started(&mut self, call: &ToolCall) {
        self.answering = false;
        self.terminal.step_started(call);
        self.announce(call, "in_progress", None);
    }

    fn tool_blocked(&mut self, call: &ToolCall, reason: &str) {
        self.terminal.step_blocked(call, reason);
        self.finish(call, "failed", reason);
        self.blocked = Some(call.call_id.clone());
    }

    fn watched(&mut self, _call: &ToolCall, what: &Watched) {
        if let Watched::Changed(changed) = what {
            self.terminal.step_watched(changed);
        }
    }

    fn tool_finished(&mut self, call: &ToolCall, output: &str) {
        if self.blocked.take().is_some_and(|id| id == call.call_id) {
            return;
        }
        let outcome = steps::outcome(call, output, self.terminal.err);
        self.terminal.step_finished(call, &outcome);
        self.terminal.turn.stepped();
        self.finish(call, finished(output), output);
        self.plan(call);
        // It may have left something running, or stopped it.
        self.state();
    }

    fn usage(&mut self, usage: &Usage) {
        self.terminal.tokens_used(usage);
        self.used(usage.input_tokens);
    }

    fn compacting(&mut self) {
        self.compacting = true;
    }

    fn compacted(&mut self, report: &Compacted) {
        self.compacting = false;
        self.used(report.after);
    }

    /// What the client said, then what came in from other reins.
    fn steering(&mut self) -> Vec<Heard> {
        let mut heard = std::mem::take(&mut *self.link.heard.borrow_mut());
        for one in self.terminal.take_incoming() {
            heard.extend(self.terminal.heard_of(one));
        }
        heard
    }

    fn waiting(&mut self) -> Vec<Heard> {
        let mut heard = std::mem::take(&mut *self.link.heard.borrow_mut());
        heard.append(&mut self.terminal.notes);
        heard
    }

    fn steered(&mut self, heard: &Heard) {
        match heard {
            Heard::You(text) => self.link.send(wire::yours(&self.session, text)),
            Heard::Peer { from, text } | Heard::Reported { from, text } | Heard::Told { from, text } => self.link.send(wire::heard(&self.session, from, text)),
            Heard::Changed { by, path } => self.chunk("agent_thought_chunk", &format!("{by} changed {path}, which this conversation has seen")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_clear_yes_is_a_yes() {
        let picked = |option: &str| json!({ "outcome": { "outcome": "selected", "optionId": option } });
        assert_eq!(approval(Some(&picked(ALLOW))), Approval::Yes);
        assert_eq!(approval(Some(&picked(ALWAYS))), Approval::Always);
        assert_eq!(approval(Some(&picked(REJECT))), Approval::No);
        assert_eq!(approval(Some(&picked(STOP))), Approval::Stop);
        assert_eq!(approval(Some(&picked("yes please"))), Approval::No);
        assert_eq!(approval(Some(&json!({ "outcome": { "outcome": "cancelled" } }))), Approval::Stop);
        assert_eq!(approval(Some(&json!({ "outcome": { "outcome": "maybe", "optionId": ALLOW } }))), Approval::No);
        assert_eq!(approval(Some(&json!({}))), Approval::No);
        assert_eq!(approval(None), Approval::No, "an error isn't a yes");
    }
}
