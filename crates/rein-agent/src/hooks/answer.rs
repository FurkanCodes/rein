//! What a hook said, read as Claude Code reads it: exit code 2 says no,
//! with why on its error output; anything else that isn't 0 is a hook
//! that didn't work, which doesn't stop what it's for. Its output is read
//! as JSON when it's one object and nothing else, and otherwise, where an
//! event takes it, as context for the model.

use serde_json::Value;

use super::EventName;
use super::run::Ran;

/// What one hook said, read.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Said {
    /// No, and why: exit code 2, `"decision": "block"`, or a tool's
    /// `"permissionDecision": "deny"`.
    pub(crate) no: Option<String>,
    /// Ask the user first, and why: a tool's `"permissionDecision": "ask"`.
    pub(crate) ask: Option<String>,
    /// Yes without asking: a tool's `"permissionDecision": "allow"`.
    pub(crate) yes: bool,
    /// The tool's input, as the hook would have it: `updatedInput`.
    pub(crate) input: Option<Value>,
    /// What the model is to know: `additionalContext`, or plain output
    /// where the event takes it.
    pub(crate) context: Option<String>,
    /// What the user is shown: `systemMessage`.
    pub(crate) message: Option<String>,
    /// Stop altogether, and why: `"continue": false`.
    pub(crate) stop: Option<String>,
    /// The tool's result, as the hook would have it: `updatedToolOutput`.
    pub(crate) output: Option<String>,
    /// It didn't work, and why.
    pub(crate) failed: Option<String>,
}

/// What `ran`, a hook on `event`, said.
pub(crate) fn read(event: EventName, ran: &Ran) -> Said {
    if let Some(why) = &ran.failed {
        return Said { failed: Some(why.clone()), ..Said::default() };
    }
    if ran.code == Some(2) {
        let why = ran.stderr.trim();
        return Said { no: Some(if why.is_empty() { "it said no, without saying why".to_owned() } else { why.to_owned() }), ..Said::default() };
    }
    let trimmed = ran.stdout.trim();
    let json = trimmed.starts_with('{').then(|| serde_json::from_str::<serde_json::Map<String, Value>>(trimmed).ok()).flatten();
    let mut said = match json {
        Some(json) => from_json(&json),
        // Plain output is context only where the event takes it.
        None if event.takes_plain_output() && !trimmed.is_empty() => Said { context: Some(ran.stdout.trim_end().to_owned()), ..Said::default() },
        None => Said::default(),
    };
    if ran.code != Some(0) {
        let first = ran.stderr.lines().find(|line| !line.trim().is_empty()).map(str::trim);
        said.failed = Some(match (first, ran.code) {
            (Some(line), _) => line.to_owned(),
            (None, Some(code)) => format!("it ended with exit code {code}"),
            (None, None) => "a signal stopped it".to_owned(),
        });
    }
    said
}

fn from_json(json: &serde_json::Map<String, Value>) -> Said {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let mut said = Said { message: text(json.get("systemMessage")).filter(|message| !message.trim().is_empty()), ..Said::default() };
    if json.get("continue") == Some(&Value::Bool(false)) {
        said.stop = Some(text(json.get("stopReason")).unwrap_or_default());
    }
    let reason = text(json.get("reason"));
    match json.get("decision").and_then(Value::as_str) {
        Some("block") => said.no = Some(reason.clone().unwrap_or_else(|| "it said no, without saying why".into())),
        Some("approve") => said.yes = true,
        _ => {}
    }
    let Some(specific) = json.get("hookSpecificOutput").and_then(Value::as_object) else { return said };
    let why = text(specific.get("permissionDecisionReason")).or(reason);
    match specific.get("permissionDecision").and_then(Value::as_str) {
        Some("allow") => said.yes = true,
        Some("deny") => said.no = Some(why.clone().unwrap_or_else(|| "it said no, without saying why".into())),
        Some("ask") => said.ask = Some(why.clone().unwrap_or_default()),
        _ => {}
    }
    // As a permission request's hook decides.
    if let Some(decision) = specific.get("decision").and_then(Value::as_object) {
        match decision.get("behavior").and_then(Value::as_str) {
            Some("allow") => said.yes = true,
            Some("deny") => said.no = Some(text(decision.get("message")).unwrap_or_else(|| "it said no, without saying why".into())),
            _ => {}
        }
        if let Some(input) = decision.get("updatedInput").filter(|input| input.is_object()) {
            said.input = Some(input.clone());
        }
    }
    if let Some(input) = specific.get("updatedInput").filter(|input| input.is_object()) {
        said.input = Some(input.clone());
    }
    said.context = text(specific.get("additionalContext")).filter(|context| !context.trim().is_empty());
    said.output = specific.get("updatedToolOutput").or_else(|| specific.get("updatedMCPToolOutput")).and_then(output_text);
    said
}

/// A tool's result as a hook gives it: text, or the text in it.
fn output_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => ["output", "stdout", "text"].iter().find_map(|key| map.get(*key).and_then(Value::as_str)).map(str::to_owned),
        // An MCP tool's content, its text parts.
        Value::Array(parts) => {
            let texts: Vec<&str> = parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ran(code: i32, stdout: &str, stderr: &str) -> Ran {
        Ran { code: Some(code), stdout: stdout.into(), stderr: stderr.into(), ..Ran::default() }
    }

    #[test]
    fn exit_code_two_says_no_and_others_are_a_hook_that_didnt_work() {
        let no = read(EventName::PreToolUse, &ran(2, r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#, "not on main\n"));
        assert_eq!(no, Said { no: Some("not on main".into()), ..Said::default() }, "its JSON can't say otherwise");
        let broken = read(EventName::PreToolUse, &ran(1, "", "\njq: command not found\nmore\n"));
        assert_eq!(broken, Said { failed: Some("jq: command not found".into()), ..Said::default() }, "it doesn't stop the tool");
        let late = Ran { failed: Some("it took longer than 30s and was stopped".into()), ..Ran::default() };
        assert_eq!(read(EventName::Stop, &late).failed.as_deref(), Some("it took longer than 30s and was stopped"));
    }

    #[test]
    fn json_says_what_a_hook_wants_as_claude_code_reads_it() {
        let rtk = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecisionReason":"RTK auto-rewrite","updatedInput":{"command":"rtk cargo test"}}}"#;
        let said = read(EventName::PreToolUse, &ran(0, rtk, ""));
        assert_eq!(said.input, Some(json!({ "command": "rtk cargo test" })));
        assert!(!said.yes && said.no.is_none() && said.ask.is_none(), "a rewrite alone decides nothing");
        let deny = read(EventName::PreToolUse, &ran(0, r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"use pnpm"}}"#, ""));
        assert_eq!(deny.no.as_deref(), Some("use pnpm"));
        let stop = read(EventName::Stop, &ran(0, r#"{"decision":"block","reason":"tests fail","systemMessage":"still red","continue":false,"stopReason":"enough"}"#, ""));
        assert_eq!((stop.no.as_deref(), stop.message.as_deref(), stop.stop.as_deref()), (Some("tests fail"), Some("still red"), Some("enough")));
        let asked = read(EventName::PermissionRequest, &ran(0, r#"{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"never on Fridays"}}}"#, ""));
        assert_eq!(asked.no.as_deref(), Some("never on Fridays"));
        let mcp = read(EventName::PostToolUse, &ran(0, r#"{"hookSpecificOutput":{"updatedMCPToolOutput":[{"type":"text","text":"short"}]}}"#, ""));
        assert_eq!(mcp.output.as_deref(), Some("short"));
    }

    #[test]
    fn plain_output_is_context_only_where_the_event_takes_it() {
        let codegraph = "<codegraph_context>...</codegraph_context>\n";
        assert_eq!(read(EventName::UserPromptSubmit, &ran(0, codegraph, "")).context.as_deref(), Some("<codegraph_context>...</codegraph_context>"));
        assert_eq!(read(EventName::SessionStart, &ran(0, "on main\n", "")).context.as_deref(), Some("on main"));
        assert_eq!(read(EventName::PostToolUse, &ran(0, "formatted\n", "")), Said::default(), "after a tool, plain output is the hook's own");
        let profile = "Welcome!\n{\"hookSpecificOutput\":{\"additionalContext\":\"x\"}}";
        assert_eq!(read(EventName::UserPromptSubmit, &ran(0, profile, "")).context.as_deref(), Some(profile), "JSON only when it's all there is");
    }
}
