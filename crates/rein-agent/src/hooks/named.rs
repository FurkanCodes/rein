//! rein's tools as a hook knows them: under Claude Code's names, with
//! Claude Code's arguments, so a hook written for it (`"matcher": "Bash"`,
//! `tool_input.command`) works on rein's; and what a hook gives back,
//! read into rein's arguments again. A tool Claude Code has no like of
//! goes by rein's name, and an MCP server's by its own.

use std::path::{Path, PathBuf};

use rein_llm::ToolCall;
use serde_json::{Map, Value, json};

use crate::tools::ToolName;

/// The name a hook knows `call`'s tool by.
pub(crate) fn name(call: &ToolCall) -> String {
    match ToolName::of(call) {
        Some(ToolName::Bash) => "Bash".into(),
        Some(ToolName::Read) => "Read".into(),
        Some(ToolName::Write) => "Write".into(),
        Some(ToolName::Edit) => "Edit".into(),
        Some(ToolName::Grep) => "Grep".into(),
        Some(ToolName::Glob) => "Glob".into(),
        Some(ToolName::WebSearch) => "WebSearch".into(),
        Some(ToolName::Skill) => "Skill".into(),
        Some(ToolName::Plan) => "TodoWrite".into(),
        Some(other) => other.as_str().into(),
        None => call.name.clone(),
    }
}

/// `call`'s arguments as Claude Code's tool takes them, its paths whole.
pub(crate) fn input(call: &ToolCall, root: &Path) -> Value {
    let args: Map<String, Value> = serde_json::from_str(&call.arguments).unwrap_or_default();
    let get = |key: &str| args.get(key).cloned().unwrap_or(Value::Null);
    let path = |key: &str| args.get(key).and_then(Value::as_str).map_or(Value::Null, |path| Value::String(whole(root, path).display().to_string()));
    let input = match ToolName::of(call) {
        Some(ToolName::Bash) => {
            let timeout = args.get("timeout_seconds").and_then(Value::as_u64).map_or(Value::Null, |seconds| json!(seconds.saturating_mul(1000)));
            json!({ "command": get("command"), "description": get("reason"), "timeout": timeout, "run_in_background": get("background") })
        }
        Some(ToolName::Read) => json!({ "file_path": path("path"), "offset": get("start_line"), "limit": get("line_count") }),
        Some(ToolName::Write) => json!({ "file_path": path("path"), "content": get("content") }),
        Some(ToolName::Edit) => json!({ "file_path": path("path"), "old_string": get("old_text"), "new_string": get("new_text"), "replace_all": get("replace_all") }),
        Some(ToolName::Grep) => {
            let mode = match args.get("output").and_then(Value::as_str) {
                Some("files") => json!("files_with_matches"),
                Some(_) => json!("content"),
                None => Value::Null,
            };
            json!({ "pattern": get("pattern"), "path": path("path"), "glob": get("glob"), "-i": get("ignore_case"), "output_mode": mode, "-C": get("context") })
        }
        Some(ToolName::Glob) => json!({ "pattern": get("pattern"), "path": path("path") }),
        Some(ToolName::WebSearch) => json!({ "query": get("query") }),
        Some(ToolName::Skill) => json!({ "skill": get("name"), "args": get("arguments") }),
        Some(ToolName::Plan) => {
            let steps = args.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
            let todos: Vec<Value> = steps
                .iter()
                .map(|step| {
                    let status = match step["status"].as_str() {
                        Some("done") => "completed",
                        Some("in_progress") => "in_progress",
                        _ => "pending",
                    };
                    json!({ "content": step["step"], "status": status, "activeForm": step["step"] })
                })
                .collect();
            json!({ "todos": todos })
        }
        _ => Value::Object(args.clone()),
    };
    without_nulls(input)
}

/// `call` with what a hook gave as its `updated` input, read back into
/// rein's arguments; what it doesn't say stays as it was. Claude Code
/// replaces the whole input, so a hook gives every field back.
pub(crate) fn rewritten(call: &ToolCall, updated: &Value, root: &Path) -> Result<ToolCall, String> {
    let Some(updated) = updated.as_object() else { return Err("what it gave as the tool's input isn't an object".into()) };
    let mut args: Map<String, Value> = serde_json::from_str(&call.arguments).unwrap_or_default();
    let mut take = |from: &str, to: &str| {
        if let Some(value) = updated.get(from) {
            args.insert(to.into(), value.clone());
        }
    };
    let path = |from: &str| updated.get(from).and_then(Value::as_str).map(|path| Value::String(relative(root, path)));
    match ToolName::of(call) {
        Some(ToolName::Bash) => {
            take("command", "command");
            take("description", "reason");
            take("run_in_background", "background");
            if let Some(ms) = updated.get("timeout").and_then(Value::as_u64) {
                args.insert("timeout_seconds".into(), json!(ms.div_ceil(1000).max(1)));
            }
        }
        Some(ToolName::Read) => {
            take("offset", "start_line");
            take("limit", "line_count");
        }
        Some(ToolName::Write) => take("content", "content"),
        Some(ToolName::Edit) => {
            take("old_string", "old_text");
            take("new_string", "new_text");
            take("replace_all", "replace_all");
        }
        Some(ToolName::Grep) => {
            take("pattern", "pattern");
            take("glob", "glob");
            take("-i", "ignore_case");
            take("-C", "context");
            if let Some(mode) = updated.get("output_mode").and_then(Value::as_str) {
                args.insert("output".into(), json!(if mode == "files_with_matches" { "files" } else { "lines" }));
            }
        }
        Some(ToolName::Glob) => take("pattern", "pattern"),
        Some(ToolName::WebSearch) => take("query", "query"),
        Some(ToolName::Skill) => {
            take("skill", "name");
            take("args", "arguments");
        }
        Some(ToolName::Plan) => return Err("rein's plan is the model's own: a hook can't change it".into()),
        _ => args = updated.clone(),
    }
    // A path, whichever tool takes one, back as rein has it.
    let key = if ToolName::of(call).is_some_and(|tool| tool.changes_files() || tool == ToolName::Read) { "file_path" } else { "path" };
    if matches!(ToolName::of(call), Some(ToolName::Read | ToolName::Write | ToolName::Edit | ToolName::Grep | ToolName::Glob))
        && let Some(path) = path(key)
    {
        args.insert("path".into(), path);
    }
    Ok(ToolCall { arguments: Value::Object(args).to_string(), ..call.clone() })
}

/// What `call` gave, `output`, as Claude Code's tool would give it, for a
/// hook after it: a command's output and exit code, a file's path.
pub(crate) fn response(call: &ToolCall, output: &str, root: &Path) -> Value {
    let args: Map<String, Value> = serde_json::from_str(&call.arguments).unwrap_or_default();
    match ToolName::of(call) {
        Some(ToolName::Bash) => {
            let (text, code) = match output.rsplit_once("[exit code ") {
                Some((text, rest)) => (text, rest.split(']').next().and_then(|code| code.parse::<i64>().ok())),
                None => (output, None),
            };
            json!({ "stdout": text, "stderr": "", "interrupted": false, "exitCode": code })
        }
        Some(ToolName::Write | ToolName::Edit | ToolName::Read) => {
            let path = args.get("path").and_then(Value::as_str).map(|path| whole(root, path).display().to_string());
            json!({ "filePath": path, "success": !output.starts_with(crate::tools::ERROR), "output": output })
        }
        _ => json!({ "output": output }),
    }
}

/// `path`, as rein's tools take it, whole.
fn whole(root: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() { path.to_owned() } else { root.join(path) }
}

/// `path`, as a hook gives it, as rein's tools take it: inside the project,
/// from its root.
fn relative(root: &Path, path: &str) -> String {
    Path::new(path).strip_prefix(root).map_or_else(|_| path.to_owned(), |rest| rest.display().to_string())
}

fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(_, value)| !value.is_null()).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall { call_id: "c".into(), name: name.into(), arguments: args.to_string() }
    }

    #[test]
    fn reins_tools_go_by_claude_codes_names_and_arguments() {
        let root = Path::new("/p");
        let bash = call("bash", json!({ "command": "cargo test", "timeout_seconds": 600, "access": "write", "reason": null }));
        assert_eq!((name(&bash), input(&bash, root)), ("Bash".into(), json!({ "command": "cargo test", "timeout": 600_000 })));
        let edit = call("edit", json!({ "path": "src/a.rs", "old_text": "a", "new_text": "b", "replace_all": false }));
        assert_eq!(input(&edit, root), json!({ "file_path": "/p/src/a.rs", "old_string": "a", "new_string": "b", "replace_all": false }));
        let plan = call("plan", json!({ "steps": [{ "step": "test it", "status": "done" }] }));
        assert_eq!((name(&plan), input(&plan, root)["todos"][0]["status"].clone()), ("TodoWrite".into(), json!("completed")));
        let mcp = call("mcp__linear__create_issue", json!({ "title": "x" }));
        assert_eq!((name(&mcp), input(&mcp, root)), ("mcp__linear__create_issue".into(), json!({ "title": "x" })));
        assert_eq!(name(&call("recall", json!({}))), "recall", "rein's own goes by rein's name");
    }

    #[test]
    fn a_hooks_rewrite_comes_back_as_reins_arguments() {
        let root = Path::new("/p");
        let bash = call("bash", json!({ "command": "cargo test", "timeout_seconds": null, "access": "write", "reason": null }));
        let rtk = json!({ "command": "rtk cargo test", "timeout": 1500 });
        let back: Value = serde_json::from_str(&rewritten(&bash, &rtk, root).unwrap().arguments).unwrap();
        assert_eq!(back, json!({ "command": "rtk cargo test", "timeout_seconds": 2, "access": "write", "reason": null }), "what rein has and Claude Code hasn't stays");
        let edit = call("edit", json!({ "path": "src/a.rs", "old_text": "a", "new_text": "b", "replace_all": false }));
        let moved = json!({ "file_path": "/p/src/b.rs", "old_string": "a", "new_string": "c", "replace_all": true });
        let back: Value = serde_json::from_str(&rewritten(&edit, &moved, root).unwrap().arguments).unwrap();
        assert_eq!(back, json!({ "path": "src/b.rs", "old_text": "a", "new_text": "c", "replace_all": true }));
        assert!(rewritten(&bash, &json!("rm -rf /"), root).is_err());
        assert!(rewritten(&call("plan", json!({ "steps": [] })), &json!({ "todos": [] }), root).is_err());
    }

    #[test]
    fn a_result_goes_to_a_hook_as_claude_codes_tool_gives_it() {
        let root = Path::new("/p");
        let bash = call("bash", json!({ "command": "false" }));
        assert_eq!(response(&bash, "oops\n[exit code 1]", root), json!({ "stdout": "oops\n", "stderr": "", "interrupted": false, "exitCode": 1 }));
        let write = call("write", json!({ "path": "a.txt", "content": "x" }));
        assert_eq!(response(&write, "created a.txt (1 line)", root)["filePath"], "/p/a.txt");
    }
}
