//! What rein shows a client instead of a screen: the agent's [`Ui`], as
//! `session/update`s, and its questions as `session/request_permission`.

use std::collections::HashSet;

use rein_agent::agent::{Approval, Compacted, Heard, Ui};
use rein_agent::permissions::Rule;
use rein_agent::tools::ToolName;
use rein_llm::{ToolCall, Usage};
use serde_json::{Value, json};

use super::{Link, wire};

const ALLOW: &str = "allow";
const ALWAYS: &str = "always";
const REJECT: &str = "reject";

/// The client, as the agent sees it.
pub(super) struct Served {
    session: String,
    link: Link,
    /// The context window, for what's used of it.
    limit: u64,
    /// Calls the client knows of: one is announced once, and updated after.
    announced: HashSet<String>,
}

impl Served {
    pub(super) fn new(session: String, link: Link, limit: u64) -> Served {
        Served { session, link, limit, announced: HashSet::new() }
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
        let content = [json!({ "type": "content", "content": { "type": "text", "text": said } })];
        self.update(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": call.call_id, "status": status, "content": content }));
    }

    fn used(&self, tokens: u64) {
        self.update(&json!({ "sessionUpdate": "usage_update", "used": tokens, "size": self.limit }));
    }
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
        self.chunk("agent_message_chunk", delta);
    }

    fn thought(&mut self, delta: &str) {
        self.chunk("agent_thought_chunk", delta);
    }

    fn note(&mut self, note: &str) {
        self.chunk("agent_thought_chunk", note);
    }

    async fn approve(&mut self, call: &ToolCall, _rule: Option<&Rule>) -> Approval {
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

    fn tool_started(&mut self, call: &ToolCall) {
        self.announce(call, "in_progress");
    }

    fn tool_blocked(&mut self, call: &ToolCall, reason: &str) {
        self.finish(call, "failed", reason);
    }

    fn tool_finished(&mut self, call: &ToolCall, output: &str) {
        self.finish(call, if rein_agent::agent::failed(output) { "failed" } else { "completed" }, output);
    }

    fn usage(&mut self, usage: &Usage) {
        self.used(usage.input_tokens);
    }

    fn compacted(&mut self, report: &Compacted) {
        self.used(report.after);
    }

    fn steering(&mut self) -> Vec<Heard> {
        std::mem::take(&mut *self.link.heard.borrow_mut())
    }

    fn waiting(&mut self) -> Vec<Heard> {
        self.steering()
    }

    fn steered(&mut self, heard: &Heard) {
        if let Heard::You(text) = heard {
            self.chunk("user_message_chunk", text);
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
