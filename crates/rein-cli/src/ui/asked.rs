//! What you're asked before a step runs, in the same words wherever you
//! are: how far a yes goes is part of the question, and a window that said
//! less would have you agree to more than it showed.

use rein_agent::permissions::{Rule, Sandboxing, Whose};
use rein_agent::sandbox::Access;
use rein_agent::tools::{BashCall, ToolName};
use rein_llm::ToolCall;

/// The answer that says no and hands the conversation back to you.
pub(crate) const INSTEAD: &str = "No, and tell rein what to do instead";

/// What you're asked of a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    pub question: String,
    /// The yes that stops asking, which says how far it goes.
    pub always: &'static str,
    /// Why it's asked: why it needs to, as the model put it, and the rule
    /// that asks, which is yours to change.
    pub reason: Option<String>,
    /// What that yes covers.
    covers: &'static str,
}

impl Asked {
    pub(crate) fn of(call: &ToolCall, rule: Option<&Rule>, sandboxing: Sandboxing) -> Asked {
        let bash = BashCall::of(call);
        // An MCP server's tool, by its server's name and its own.
        let server_tool = rein_mcp::parts(&call.name).filter(|_| ToolName::of(call).is_none());
        let (question, always, covers) = match (ToolName::of(call), server_tool) {
            (Some(ToolName::Bash), _) => (command_question(bash.as_ref(), sandboxing).to_owned(), "Yes, and don't ask again for this command", "this command"),
            (Some(ToolName::Edit), _) => ("Allow this edit?".to_owned(), "Yes, and don't ask again for file changes", "file changes"),
            (Some(ToolName::Write), _) => ("Allow writing this file?".to_owned(), "Yes, and don't ask again for file changes", "file changes"),
            (Some(ToolName::Spawn), _) => ("Start a sub-agent in a new tab?".to_owned(), "Yes, and don't ask again in this conversation", "sub-agents"),
            (None, Some((server, tool))) => (format!("Let {server}'s {tool} run?"), "Yes, and don't ask again for this tool", "this tool"),
            _ => ("Allow this?".to_owned(), "Yes, and don't ask again", "this"),
        };
        let why = bash.as_ref().and_then(BashCall::reason).map(|reason| format!("Why: {reason}"));
        let asks = rule.map(|rule| match rule.whose() {
            Whose::Yours => format!("Your rule `{rule}` asks first · /permissions changes your rules"),
            Whose::Project => format!("This project's rule `{rule}` asks first"),
        });
        let reason = match (why, asks) {
            (Some(why), Some(asks)) => Some(format!("{why} · {asks}")),
            (why, asks) => why.or(asks),
        };
        Asked { question, always, reason, covers }
    }

    /// Said once the answer is [`always`](Asked::always).
    pub(crate) fn wont_ask_again(&self) -> String {
        format!("won't ask again for {} in this conversation", self.covers)
    }
}

/// How a command is asked about: the sandbox's walls say what it's asking
/// to go past. Without them, access is only words.
fn command_question(bash: Option<&BashCall>, sandboxing: Sandboxing) -> &'static str {
    match bash.map(BashCall::access) {
        _ if sandboxing != Sandboxing::On => "Allow this command?",
        Some(Access::Write) => "Allow this command to change files?",
        Some(Access::Network) => "Allow this command to use the network?",
        Some(Access::Outside) => "Allow this command to run outside the sandbox?",
        Some(Access::Look) | None => "Allow this command?",
    }
}

#[cfg(test)]
mod tests {
    use rein_agent::permissions::Mode;
    use rein_agent::permissions::Permissions;
    use rein_llm::serde_json::json;

    use super::*;

    fn call(name: &str, arguments: rein_llm::serde_json::Value) -> ToolCall {
        ToolCall { call_id: "c".to_owned(), name: name.to_owned(), arguments: arguments.to_string() }
    }

    fn bash(access: &str, reason: Option<&str>) -> ToolCall {
        call("bash", json!({ "command": "curl example.com", "access": access, "reason": reason }))
    }

    #[test]
    fn a_yes_says_how_far_it_goes_and_what_it_comes_to_says_the_same() {
        let cases = [
            (call("edit", json!({ "path": "a.rs" })), "Allow this edit?", "Yes, and don't ask again for file changes", "won't ask again for file changes in this conversation"),
            (
                call("write", json!({ "path": "a.rs" })),
                "Allow writing this file?",
                "Yes, and don't ask again for file changes",
                "won't ask again for file changes in this conversation",
            ),
            (bash("look", None), "Allow this command?", "Yes, and don't ask again for this command", "won't ask again for this command in this conversation"),
            (
                call("mcp__github__create_issue", json!({})),
                "Let github's create_issue run?",
                "Yes, and don't ask again for this tool",
                "won't ask again for this tool in this conversation",
            ),
        ];
        for (call, question, always, said) in cases {
            let asked = Asked::of(&call, None, Sandboxing::Off);
            assert_eq!((asked.question.as_str(), asked.always, asked.wont_ask_again().as_str()), (question, always, said));
        }
    }

    #[test]
    fn what_a_command_asks_to_go_past_is_named_only_where_there_are_walls() {
        let asked = |access, sandboxing| Asked::of(&bash(access, None), None, sandboxing).question;
        assert_eq!(asked("network", Sandboxing::On), "Allow this command to use the network?");
        assert_eq!(asked("outside", Sandboxing::On), "Allow this command to run outside the sandbox?");
        assert_eq!(asked("write", Sandboxing::On), "Allow this command to change files?");
        assert_eq!(asked("network", Sandboxing::Off), "Allow this command?", "off, access is only words");
    }

    #[test]
    fn the_reason_is_the_models_and_the_rule_thats_asking() {
        let permissions = Permissions::new(Mode::Ask, "ask curl").unwrap();
        let reasoned = bash("network", Some("it fetches the page"));
        let rule = permissions.asked_by(&reasoned);
        assert!(rule.is_some());
        assert_eq!(Asked::of(&reasoned, None, Sandboxing::On).reason.as_deref(), Some("Why: it fetches the page"));
        assert_eq!(Asked::of(&reasoned, rule, Sandboxing::On).reason.as_deref(), Some("Why: it fetches the page · This project's rule `ask curl` asks first"));
        assert_eq!(Asked::of(&bash("network", None), None, Sandboxing::On).reason, None);
    }
}
