//! Keeping the conversation inside the model's context window.
//!
//! Every request carries the whole conversation, so it only grows. When it
//! gets too big, rein makes room in two steps, cheapest first:
//!
//! 1. Retire stale tool output: an old read of a file that was read again
//!    or changed later, and command output from before your last two
//!    messages. No model call, nothing invented, and a stale copy of a file
//!    can't mislead the model any more.
//! 2. If that isn't enough, the model writes a summary, rein adds its own
//!    record of what happened (your messages, files changed, commands run),
//!    and the conversation becomes: the first message, the summary, and its
//!    latest part word for word.
//!
//! Nothing is lost for good: the whole conversation stays in the session
//! file, and the `recall` tool searches it.
//!
//! Compacting loses the prompt cache once: the next request starts
//! differently. The request for the summary itself doesn't: it's the
//! conversation untouched, with the request added at the end.
//!
//! Claude's thinking is signed together with everything before it, so it
//! only holds while that stays as it was. Where compaction changes the
//! conversation, the thinking after that point goes (see [`unbind`]); the
//! rest, and every word and tool call, stays. Requests then never carry
//! thinking the provider would turn away.

use std::collections::{BTreeSet, HashMap};

use rein_llm::{Item, Tool};

use crate::agent::{DECLINED_AND_STOPPED, DECLINED_CHANGE, DECLINED_COMMAND, INTERRUPTED};
use crate::tools::{ERROR, ToolName};
use serde_json::{Value, json};

/// Compact once a request would carry more than this many tokens. Well
/// below today's windows on purpose: a model works better, faster and
/// cheaper with less to read.
pub const DEFAULT_LIMIT: u64 = 150_000;

/// After a summary, about this many tokens of the latest conversation are
/// kept word for word.
pub const KEEP_RECENT: u64 = 20_000;

/// What retired tool output is replaced with starts with this.
pub const RETIRED: &str = "[removed to save space:";

/// How a read starts when the file hadn't changed since an earlier one: it
/// points at that copy, so retiring the copy would leave it pointing at
/// nothing.
pub const UNCHANGED: &str = "[unchanged:";

/// What a summary message starts with.
const SUMMARY: &str = "<summary>";

/// What the model is asked to write when the conversation is compacted.
const PROMPT: &str = "\
Pause the work: this conversation is about to be compacted to save space. Write a \
summary that lets you carry on without the conversation so far. Don't call any tools; \
answer with the summary only.

Use these headings:
Goal: what the user wants overall, in their words where it matters.
Constraints and preferences: what they asked for or ruled out.
Progress: what is done, what is in progress, what is blocked.
Key decisions: and why they were made.
Next steps: what to do next, in order.
Critical details: exact file paths, function names, commands, error messages and values \
needed to continue.

Be concise. rein adds its own record of the user's messages, the files changed and the \
commands run, so don't repeat those in full.";

/// Roughly how many tokens `chars` characters of text take: about four
/// each. Only used for what was added since the provider last counted.
pub fn estimate(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

/// How much text an item holds.
pub fn size(item: &Item) -> usize {
    match item {
        Item::User(text) | Item::Assistant(text) => text.len(),
        Item::ToolCall(call) => call.name.len() + call.arguments.len(),
        Item::ToolResult { output, .. } => output.len(),
        Item::Opaque(opaque) => opaque.value.to_string().len(),
    }
}

pub fn is_summary(item: &Item) -> bool {
    matches!(item, Item::User(text) if text.starts_with(SUMMARY))
}

/// A message the user actually typed: not rein's first message, not a summary.
fn typed(item: &Item) -> Option<&str> {
    match item {
        Item::User(text) if !crate::context::is_context(item) && !is_summary(item) => Some(text),
        _ => None,
    }
}

/// What a tool call was about, from its arguments.
struct Call {
    tool: Option<ToolName>,
    path: Option<String>,
    command: Option<String>,
    /// What a search looked for.
    pattern: Option<String>,
}

fn calls(history: &[Item]) -> HashMap<&str, (usize, Call)> {
    history
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match item {
            Item::ToolCall(call) => {
                let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
                let path = args["path"].as_str().map(|p| p.trim_start_matches("./").to_owned());
                let command = args["command"].as_str().map(str::to_owned);
                let pattern = args["pattern"].as_str().map(str::to_owned);
                Some((call.call_id.as_str(), (i, Call { tool: ToolName::of(call), path, command, pattern })))
            }
            _ => None,
        })
        .collect()
}

/// The exit code a bash result ends with, if it does.
fn exit_code(output: &str) -> Option<&str> {
    output.trim_end().lines().last()?.strip_prefix("[exit code ")?.strip_suffix(']')
}

/// The tool results worth retiring, each with what replaces it: a read of
/// a file that a later call read, wrote or edited again; and the output of
/// commands from before the user's last two messages. Short outputs stay,
/// as there's nothing to gain.
pub fn stale(history: &[Item]) -> Vec<(String, String)> {
    let calls = calls(history);
    let notes: std::collections::HashSet<&str> = history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult { call_id, output } if output.starts_with(UNCHANGED) => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let typed: Vec<usize> = history.iter().enumerate().filter(|(_, item)| typed(item).is_some()).map(|(i, _)| i).collect();
    // Commands before the second-to-last message are old news.
    let recent = if typed.len() >= 2 { typed[typed.len() - 2] } else { 0 };
    let mut retired = Vec::new();
    for (i, item) in history.iter().enumerate() {
        let Item::ToolResult { call_id, output } = item else { continue };
        if output.len() < 400 || output.starts_with(RETIRED) {
            continue;
        }
        let Some((_, call)) = calls.get(call_id.as_str()) else { continue };
        match (call.tool, &call.path, &call.command) {
            (Some(ToolName::Read), Some(path), _) => {
                // Read again or changed: a search in it doesn't count.
                let touched_later = calls.iter().any(|(id, (j, later))| {
                    *j > i && later.path.as_ref() == Some(path) && matches!(later.tool, Some(ToolName::Read | ToolName::Write | ToolName::Edit)) && !notes.contains(id)
                });
                if touched_later {
                    retired.push((
                        call_id.clone(),
                        format!("{RETIRED} an older read of {path}. It was read or changed again later; read it again if you need it.]"),
                    ));
                }
            }
            (Some(ToolName::Bash), _, Some(command)) if i < recent => {
                let lines = output.lines().count();
                let code = exit_code(output).map(|c| format!(", exit code {c}")).unwrap_or_default();
                retired.push((
                    call_id.clone(),
                    format!("{RETIRED} the output of `{command}` ({lines} lines{code}). Run it again, or use recall to search it.]"),
                ));
            }
            (Some(ToolName::Grep | ToolName::Glob), _, _) if i < recent => {
                let pattern = call.pattern.as_deref().unwrap_or_default();
                let found = output.lines().next().unwrap_or_default().trim_end_matches('.');
                retired.push((call_id.clone(), format!("{RETIRED} an older search for `{pattern}`: {found}. Search again if you need it.]")));
            }
            _ => {}
        }
    }
    retired
}

/// Swaps in the replacements from [`stale`]. Thinking after the first one
/// was signed with what's been replaced, so it goes.
pub fn retire(history: &mut Vec<Item>, retired: &[(String, String)]) {
    let by_id: HashMap<&str, &str> = retired.iter().map(|(id, text)| (id.as_str(), text.as_str())).collect();
    let mut first = None;
    for (i, item) in history.iter_mut().enumerate() {
        if let Item::ToolResult { call_id, output } = item
            && let Some(text) = by_id.get(call_id.as_str())
        {
            *output = (*text).to_owned();
            first.get_or_insert(i);
        }
    }
    if let Some(first) = first {
        let mut i = 0;
        history.retain(|item| {
            i += 1;
            i <= first || !bound(item)
        });
    }
}

/// Drops the thinking that only holds while the conversation before it
/// stays as it was: Claude's. After a summary, nothing before is as it was.
pub fn unbind(items: &mut Vec<Item>) {
    items.retain(|item| !bound(item));
}

fn bound(item: &Item) -> bool {
    matches!(item, Item::Opaque(opaque) if opaque.bound())
}

/// rein's own record of the conversation, from everything that happened
/// (`full`, which compaction never shortens): what the user typed, the
/// files changed and read, and the commands run. Facts the model doesn't
/// have to remember, so it can't get them wrong.
pub fn record(full: &[Item]) -> String {
    let calls = calls(full);
    let results: HashMap<&str, &str> = full
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult { call_id, output } => Some((call_id.as_str(), output.as_str())),
            _ => None,
        })
        .collect();

    let messages: Vec<&str> = full.iter().filter_map(typed).collect();
    // Long conversations keep every message, but shortened.
    let room = if messages.iter().map(|m| m.len()).sum::<usize>() > 12_000 { 300 } else { usize::MAX };
    let mut text = String::from("Messages from the user, oldest first:\n");
    for message in &messages {
        text.push_str(&format!("- {}\n", shorten(message, room)));
    }

    let mut changed = BTreeSet::new();
    let mut read = BTreeSet::new();
    let mut commands = Vec::new();
    let mut ordered: Vec<(&usize, &str, &Call)> = calls.iter().map(|(id, (i, call))| (i, *id, call)).collect();
    ordered.sort_by_key(|(i, _, _)| **i);
    for (_, id, call) in ordered {
        let result = results.get(id).copied().unwrap_or("");
        let declined = [DECLINED_COMMAND, DECLINED_CHANGE, DECLINED_AND_STOPPED].contains(&result);
        let worked = !result.starts_with(ERROR) && !declined;
        match (call.tool, &call.path, &call.command) {
            (Some(ToolName::Write | ToolName::Edit), Some(path), _) if worked => {
                changed.insert(path.clone());
            }
            (Some(ToolName::Read), Some(path), _) => {
                read.insert(path.clone());
            }
            (Some(ToolName::Bash), _, Some(command)) => {
                let outcome = match exit_code(result) {
                    Some(code) => format!("exit code {code}"),
                    None if declined => "declined".into(),
                    None if result == INTERRUPTED => "interrupted".into(),
                    None => "no exit code".into(),
                };
                commands.push(format!("- `{}`: {outcome}", shorten(command, 200)));
            }
            _ => {}
        }
    }
    let list = |set: &BTreeSet<String>| if set.is_empty() { "none".to_owned() } else { set.iter().cloned().collect::<Vec<_>>().join(", ") };
    text.push_str(&format!("Files changed: {}\nFiles read: {}\n", list(&changed), list(&read)));
    if commands.is_empty() {
        text.push_str("Commands run: none\n");
    } else {
        // The latest ones matter most.
        let skip = commands.len().saturating_sub(40);
        text.push_str(&format!("Commands run{}:\n", if skip > 0 { format!(" (the last 40 of {})", commands.len()) } else { String::new() }));
        text.push_str(&commands[skip..].join("\n"));
        text.push('\n');
    }
    text
}

/// The message that stands in for the compacted conversation.
pub fn summary_message(summary: &str, record: &str) -> String {
    format!(
        "{SUMMARY}\nThe conversation before this point was compacted to save space. This is a summary of it, \
         followed by rein's record of what happened. Earlier messages and tool output can still be searched \
         with the recall tool.\n\n{}\n</summary>\n\n<record written by rein>\n{}</record>",
        summary.trim(),
        record
    )
}

/// The request that asks for a summary: the user's message after the
/// whole conversation, so the request starts exactly like the last one and
/// is mostly served from the prompt cache.
pub fn prompt(focus: Option<&str>) -> String {
    match focus {
        Some(focus) if !focus.trim().is_empty() => format!("{PROMPT}\n\nThe user asked the summary to focus on: {}", focus.trim()),
        _ => PROMPT.to_owned(),
    }
}

/// Where the word-for-word part starts: as far back from the end as
/// `budget` tokens reach, but never at `from` or earlier, and never so that
/// a tool result is kept without its call.
pub fn recent_start(history: &[Item], from: usize, budget: u64) -> usize {
    let mut start = history.len();
    let mut used = 0;
    while start > from {
        let next = estimate(size(&history[start - 1]));
        if used + next > budget {
            break;
        }
        used += next;
        start -= 1;
    }
    let calls = calls(history);
    // Move later until every kept result has its call.
    while start < history.len() {
        let orphan = history[start..].iter().any(|item| match item {
            Item::ToolResult { call_id, .. } => calls.get(call_id.as_str()).is_none_or(|(i, _)| *i < start),
            _ => false,
        });
        if !orphan {
            break;
        }
        start += 1;
    }
    start
}

/// The tool for searching the conversation, offered once it's been compacted.
pub fn recall_spec() -> Tool {
    Tool {
        name: ToolName::Recall.as_str().into(),
        description: "Search the whole conversation, including the parts compacted away, for messages and tool \
                      output containing all the given words. Use it when the summary doesn't have a detail you need."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": { "query": { "type": "string", "description": "Words to look for; all must appear, in any case." } },
            "required": ["query"],
            "additionalProperties": false
        }),
    }
}

/// Runs `recall`: the items in `full` that contain every word of the
/// query, oldest first, each cut down to the part around the first match.
pub fn recall(full: &[Item], arguments: &str) -> String {
    let query = serde_json::from_str::<Value>(arguments).ok().and_then(|v| v["query"].as_str().map(str::to_owned)).unwrap_or_default();
    // Models like to quote their search terms; the quotes aren't in the text.
    let words: Vec<String> = query
        .split_whitespace()
        .map(|word| word.trim_matches(|c| matches!(c, '"' | '\'' | '`')).to_lowercase())
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        return "error: give some words to search for".into();
    }
    let mut found = Vec::new();
    for (i, item) in full.iter().enumerate() {
        if crate::context::is_context(item) || is_summary(item) {
            continue;
        }
        let (kind, text) = match item {
            Item::User(text) => ("user", text.clone()),
            Item::Assistant(text) => ("assistant", text.clone()),
            Item::ToolCall(call) => ("tool call", format!("{} {}", call.name, call.arguments)),
            Item::ToolResult { output, .. } => ("tool result", output.clone()),
            Item::Opaque(_) => continue,
        };
        let lower = text.to_lowercase();
        if words.iter().all(|w| lower.contains(w.as_str())) {
            let at = lower.find(words[0].as_str()).unwrap_or(0);
            found.push(format!("#{i} {kind}:\n{}", around(&text, at, 700)));
        }
    }
    if found.is_empty() {
        return format!("Nothing in the conversation contains all of: {}. Try fewer or other words.", words.join(" "));
    }
    let total = found.len();
    // The latest matches are usually the ones that matter.
    let shown = &found[total.saturating_sub(10)..];
    let mut out = if total > shown.len() { format!("{total} matches; the last {}:\n\n", shown.len()) } else { String::new() };
    out.push_str(&shown.join("\n\n"));
    out
}

/// About `width` characters of `text` around byte `at`.
fn around(text: &str, at: usize, width: usize) -> String {
    let mut start = at.saturating_sub(width / 3);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + width).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&text[start..end]);
    if end < text.len() {
        out.push('…');
    }
    out
}

/// The first `max` characters of `text`, on one line.
fn shorten(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use rein_llm::ToolCall;

    use super::*;

    fn call(id: &str, name: &str, args: Value) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: name.into(), arguments: args.to_string() })
    }

    fn result(id: &str, output: &str) -> Item {
        Item::ToolResult { call_id: id.into(), output: output.into() }
    }

    fn big(word: &str) -> String {
        format!("{word}\n").repeat(100)
    }

    #[test]
    fn retires_reads_made_stale_and_old_command_output() {
        let history = vec![
            Item::User("<environment>…</environment>".into()),
            Item::User("look at a.py".into()),
            call("r1", "read", json!({"path": "a.py"})),
            result("r1", &big("old a")),
            call("b1", "bash", json!({"command": "cargo test"})),
            result("b1", &(big("test output") + "[exit code 1]")),
            call("e1", "edit", json!({"path": "./a.py"})),
            result("e1", "edited a.py"),
            call("g1", "grep", json!({"pattern": "def main", "path": "b.py"})),
            result("g1", &format!("12 matches in 3 files\n{}", big("b.py:1:def main"))),
            Item::User("now b.py".into()),
            call("r2", "read", json!({"path": "b.py"})),
            result("r2", &big("b")),
            call("g2", "grep", json!({"pattern": "import", "path": "b.py"})),
            result("g2", &big("1:import os")),
            Item::User("and run it".into()),
            call("b2", "bash", json!({"command": "python b.py"})),
            result("b2", &big("fresh output")),
        ];
        let retired = stale(&history);
        let ids: Vec<&str> = retired.iter().map(|(id, _)| id.as_str()).collect();
        // a.py was edited later; cargo test and the first search ran before
        // the last two messages. b.py's only read stays, though it was
        // searched after: a search isn't a read. So do the latest command
        // and search.
        assert_eq!(ids, ["r1", "b1", "g1"]);
        assert!(retired[0].1.contains("an older read of a.py"));
        assert!(retired[1].1.contains("`cargo test` (101 lines, exit code 1)"));
        assert!(retired[2].1.contains("an older search for `def main`: 12 matches in 3 files."), "{}", retired[2].1);

        let mut history = history;
        retire(&mut history, &retired);
        assert!(stale(&history).is_empty(), "retiring twice changes nothing");
    }

    #[test]
    fn thinking_signed_before_a_retired_output_goes_and_the_rest_stays() {
        let thinking = |sig: &str| Item::Opaque(rein_llm::Opaque::new(rein_llm::Api::Anthropic, json!({"type": "thinking", "thinking": "", "signature": sig})));
        let reasoning = Item::Opaque(rein_llm::Opaque::new(rein_llm::Api::Responses, json!({"type": "reasoning", "encrypted_content": "x"})));
        let mut history = vec![
            Item::User("look at a.py".into()),
            thinking("before"),
            call("r1", "read", json!({"path": "a.py"})),
            result("r1", &big("old a")),
            thinking("after"),
            reasoning.clone(),
            call("e1", "edit", json!({"path": "a.py"})),
            result("e1", "edited a.py"),
            thinking("later"),
            Item::Assistant("done".into()),
        ];
        let retired = stale(&history);
        assert_eq!(retired.len(), 1);
        retire(&mut history, &retired);
        let signatures: Vec<&str> = history
            .iter()
            .filter_map(|item| match item {
                Item::Opaque(o) if o.bound() => o.value["signature"].as_str(),
                _ => None,
            })
            .collect();
        // The thinking before the change still matches what came before it.
        assert_eq!(signatures, ["before"]);
        assert!(history.contains(&reasoning), "OpenAI's reasoning isn't signed with the conversation");
        assert_eq!(history.len(), 8, "every word and call stays");

        let mut kept = history.clone();
        unbind(&mut kept);
        assert!(kept.iter().all(|item| !bound(item)));
    }

    #[test]
    fn keeps_the_copy_a_later_note_points_to() {
        let history = vec![
            Item::User("look at @a.py".into()),
            call("r1", "read", json!({"path": "a.py"})),
            result("r1", &big("a")),
            Item::User("again @a.py".into()),
            call("r2", "read", json!({"path": "a.py"})),
            result("r2", &format!("{UNCHANGED} a.py is as it was when it was last read in this conversation, so that copy is current.]")),
        ];
        assert!(stale(&history).is_empty(), "the note isn't a fresh copy, so the first read stays");
    }

    #[test]
    fn the_record_lists_what_happened() {
        let full = vec![
            Item::User("<environment>…</environment>".into()),
            Item::User("fix the tests".into()),
            call("r1", "read", json!({"path": "a.py"})),
            result("r1", "…"),
            call("e1", "edit", json!({"path": "a.py"})),
            result("e1", "edited a.py"),
            call("w1", "write", json!({"path": "b.py"})),
            result("w1", "error: can't write"),
            call("b1", "bash", json!({"command": "pytest"})),
            result("b1", "ok\n[exit code 0]"),
            call("b2", "bash", json!({"command": "rm -rf /"})),
            result("b2", "The user declined to run this command."),
            Item::User(summary_message("earlier", "record")),
        ];
        let record = record(&full);
        assert_eq!(
            record,
            "Messages from the user, oldest first:\n- fix the tests\nFiles changed: a.py\nFiles read: a.py\n\
             Commands run:\n- `pytest`: exit code 0\n- `rm -rf /`: declined\n"
        );
    }

    #[test]
    fn keeps_the_latest_part_without_splitting_a_call_from_its_result() {
        let history = vec![
            Item::User("<environment>…</environment>".into()),
            Item::User("go".into()),
            call("c1", "bash", json!({"command": "a"})),
            call("c2", "bash", json!({"command": "b"})),
            result("c1", &"x".repeat(400)),
            result("c2", &"y".repeat(40)),
            Item::Assistant("done".into()),
        ];
        // The budget reaches back to c2's call, but c1's result would then be
        // kept without its call, so the cut moves past both results.
        assert_eq!(recent_start(&history, 1, 120), 6);
        // With room for everything it stops at `from`.
        assert_eq!(recent_start(&history, 1, 10_000), 1);
    }

    #[test]
    fn recall_finds_what_was_compacted_away() {
        let full = vec![
            Item::User("<environment>secret</environment>".into()),
            call("b1", "bash", json!({"command": "cargo test"})),
            result("b1", "test parse ... FAILED: expected Token::Comma, found Token::Semi\n[exit code 101]"),
            Item::Assistant("The parser test fails.".into()),
        ];
        let found = recall(&full, r#"{"query": "\"token::semi\" `FAILED`"}"#);
        assert!(found.starts_with("#2 tool result:\ntest parse ... FAILED"), "{found}");
        assert!(recall(&full, r#"{"query": "environment"}"#).starts_with("Nothing"), "rein's first message isn't searched");
        assert!(recall(&full, r#"{"query": " "}"#).starts_with("error:"));
    }

    #[test]
    fn estimates_about_four_characters_a_token() {
        assert_eq!(estimate(0), 0);
        assert_eq!(estimate(9), 3);
    }
}
