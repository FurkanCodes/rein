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
use crate::peers::Node;
use crate::peers::talk::{self, Child};
use crate::terminal::Terminal;
use crate::ui::steps;

const ALLOW: &str = "allow";
const ALWAYS: &str = "always";
const REJECT: &str = "reject";

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

impl Served {
    pub(super) fn new(session: String, link: Link, terminal: Terminal) -> Served {
        Served { session, link, announced: HashSet::new(), terminal, answering: false, compacting: false, blocked: None }
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

    /// Tells the client of `call`, or that it's now `status`.
    fn announce(&mut self, call: &ToolCall, status: &str) {
        if self.announced.insert(call.call_id.clone()) {
            self.update(&tool_call(call, status));
        } else {
            self.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": call.call_id, "status": status }));
        }
    }

    fn finish(&mut self, call: &ToolCall, status: &str, said: &str) {
        if !self.announced.remove(&call.call_id) {
            self.update(&tool_call(call, status));
        }
        self.update(&result_of(&call.call_id, status, said));
    }

    fn used(&self, tokens: u64) {
        self.update(&json!({ "sessionUpdate": "usage_update", "used": tokens, "size": self.terminal.context_limit }));
    }

    /// Says the conversation so far, as `session/load` asks: what you typed,
    /// what other reins said, the model's answers and its steps. What rein
    /// added for the model isn't anything you said, and isn't shown.
    pub(super) fn replay(&self, items: &[Item]) {
        for item in items {
            match item {
                Item::User(_) => {
                    if let Some(said) = compact::typed(item) {
                        match rein_agent::peers::unheard(said) {
                            Some((from, text)) => self.link.send(wire::heard(&self.session, from, text)),
                            None => self.chunk("user_message_chunk", said),
                        }
                    }
                }
                Item::Assistant(text) => self.chunk("agent_message_chunk", text),
                // Failed, until its result says otherwise: a turn cut off
                // may have left it with none.
                Item::ToolCall(call) => self.update(&tool_call(call, "failed")),
                Item::ToolResult { call_id, output } => self.update(&result_of(call_id, finished(output), output)),
                Item::Found { .. } | Item::Image { .. } | Item::Opaque(_) => {}
            }
        }
    }

    /// Asks whether `call` may go ahead.
    async fn asked(&mut self, call: &ToolCall) -> Approval {
        self.announce(call, "pending");
        let options = json!([
            { "optionId": ALLOW, "name": "Allow", "kind": "allow_once" },
            { "optionId": ALWAYS, "name": "Allow for this conversation", "kind": "allow_always" },
            { "optionId": REJECT, "name": "Reject", "kind": "reject_once" },
        ]);
        let (id, answer) = self.link.ask();
        self.link.send(wire::ask_permission(id, &self.session, &call.call_id, &options));
        // Never answered, as the client left: not a yes.
        answer.await.map_or(Approval::Stop, |result| approval(result.as_ref()))
    }

    /// What the model asked for with `spawn`: you're asked, then the client
    /// starts a `rein serve` for it, which claims the task. What the model
    /// is told of how that went.
    async fn spawn(&mut self, call: &ToolCall) -> String {
        let args: SpawnArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => return format!("{ERROR}bad arguments: {e}"),
        };
        let Some(node) = &self.terminal.peers else { return format!("{ERROR}this rein isn't among the others, so it can't start sub-agents") };
        let child = match Child::wanted(&args, &self.terminal.spec, node).await {
            Ok(child) => child,
            Err(e) => return format!("{ERROR}{e}"),
        };
        if !self.terminal.spawn_always {
            match self.asked(call).await {
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
        Some(ToolName::Write | ToolName::Edit) => "edit",
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

    async fn approve(&mut self, call: &ToolCall, _rule: Option<&Rule>) -> Approval {
        self.asked(call).await
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
        self.announce(call, "in_progress");
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
        self.finish(call, finished(output), output);
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
            Heard::You(text) => self.chunk("user_message_chunk", text),
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
        assert_eq!(approval(Some(&picked("yes please"))), Approval::No);
        assert_eq!(approval(Some(&json!({ "outcome": { "outcome": "cancelled" } }))), Approval::Stop);
        assert_eq!(approval(Some(&json!({ "outcome": { "outcome": "maybe", "optionId": ALLOW } }))), Approval::No);
        assert_eq!(approval(Some(&json!({}))), Approval::No);
        assert_eq!(approval(None), Approval::No, "an error isn't a yes");
    }
}
