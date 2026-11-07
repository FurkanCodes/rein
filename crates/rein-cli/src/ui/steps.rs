//! How each step rein takes looks on screen: one line, two columns in from
//! the model's words, with a mark (spinning while it runs, then a green ✓
//! or a red ✗), the tool, what it's about, and a short result. The work is
//! grey, so the model's words stand out from it; a change to a file is
//! what you'd look at, so it isn't. Where a step failed, the lines that say
//! why follow; Ctrl-O shows all of any step's output.
//!
//! ```text
//! rein   ● Fixing the trailing comma.
//!
//!          ✓ Read   src/parser.rs, src/lexer.rs  2 files · 360 lines
//!          ✓ Edit   src/parser.rs  +2 -1
//!               85 -     items.push(last)
//!               85 +     if !last.is_empty() {
//!               86 +         items.push(last)
//!          ✗ Run    cargo test  exit 101
//!            └ test parser::trailing_comma ... FAILED
//!            … 209 more lines
//! ```

use std::path::Path;

use rein_agent::sandbox::Access;
use rein_agent::tools::{self, BashCall, Ended, ToolName};
use rein_llm::ToolCall;
use rein_llm::serde_json::{self, Value};

use super::style::{GUTTER, Style, fit as fit_width, strip, width};
use super::tint::{self, Tint};

/// How far a step's mark is in from the model's words.
pub const STEP: usize = 2;

/// Where a step's details start: under its tool name.
pub const INDENT: usize = GUTTER + STEP + 2;

/// The least of a step's line its target keeps: what a command printed
/// last is cut before it is.
const TARGET_MOST_CUT: usize = 24;

/// Output lines shown under a step; the rest is summed up.
const SHOWN: usize = 2;
/// Diff lines shown per side in a framed diff.
const DIFF_LINES: usize = 12;

/// The tool's name as shown, and what the step is about:
/// `("Run", "cargo test")`. Only the first line of a long command.
pub fn describe(call: &ToolCall) -> (&'static str, String) {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let path = args["path"].as_str().unwrap_or_default().to_owned();
    match ToolName::of(call) {
        Some(ToolName::Read) => {
            let range = match (args["start_line"].as_u64(), args["line_count"].as_u64()) {
                (None, None) => String::new(),
                (start, Some(count)) => {
                    let start = start.unwrap_or(1);
                    format!(", lines {start}-{}", start + count.max(1) - 1)
                }
                (Some(start), None) => format!(", from line {start}"),
            };
            ("Read", path + &range)
        }
        Some(ToolName::Grep) => {
            let mut target = format!("\"{}\"", args["pattern"].as_str().unwrap_or_default());
            if !path.is_empty() && path != "." {
                target += &format!(" in {path}");
            }
            if let Some(glob) = args["glob"].as_str() {
                target += &format!(" ({glob})");
            }
            ("Search", target)
        }
        Some(ToolName::Glob) => {
            let mut target = args["pattern"].as_str().unwrap_or_default().to_owned();
            if !path.is_empty() && path != "." {
                target += &format!(" in {path}");
            }
            ("Find", target)
        }
        Some(ToolName::Write) => ("Write", path),
        Some(ToolName::Edit) => ("Edit", path),
        Some(ToolName::Bash) => {
            let command = args["command"].as_str().unwrap_or_default();
            let first = command.lines().next().unwrap_or_default();
            let more = if command.trim_end().contains('\n') { " …" } else { "" };
            // What went past the sandbox says so, in the transcript too.
            let past = match BashCall::of(call).map(|bash| bash.access()) {
                Some(Access::Network) => " (with network)",
                Some(Access::Outside) => " (outside the sandbox)",
                _ => "",
            };
            let behind = if BashCall::of(call).is_some_and(|bash| bash.background()) { " (in the background)" } else { "" };
            ("Run", format!("{first}{more}{past}{behind}"))
        }
        Some(ToolName::Recall) => ("Recall", args["query"].as_str().unwrap_or_default().to_owned()),
        Some(ToolName::ToolSearch) => ("Find tools", args["query"].as_str().unwrap_or_default().to_owned()),
        Some(ToolName::WebSearch) => ("Search", args["query"].as_str().unwrap_or_default().to_owned()),
        Some(ToolName::Skill) => {
            let file = args["file"].as_str().map(|file| format!(": {file}")).unwrap_or_default();
            ("Skill", format!("{}{file}", args["name"].as_str().unwrap_or_default()))
        }
        // To another rein: who to, then the first line of what.
        Some(tool @ (ToolName::Message | ToolName::Delegate)) => {
            let what = args[if tool == ToolName::Message { "text" } else { "task" }].as_str().unwrap_or_default();
            let first = what.lines().next().unwrap_or_default();
            let more = if what.trim_end().contains('\n') { " …" } else { "" };
            (if tool == ToolName::Message { "Message" } else { "Delegate" }, format!("{}: {first}{more}", args["to"].as_str().unwrap_or_default()))
        }
        Some(ToolName::Peers) => ("Peers", "the other reins".to_owned()),
        // What's saved is said whole, as it's what later conversations get.
        Some(ToolName::Memory) => {
            let name = args["name"].as_str().unwrap_or_default();
            match args["action"].as_str() {
                Some("save") => ("Memory", format!("save {name} ({}): {}", args["type"].as_str().unwrap_or("note"), args["description"].as_str().unwrap_or_default())),
                Some("forget") => ("Memory", format!("forget {name}")),
                _ => ("Memory", format!("read {name}")),
            }
        }
        Some(ToolName::Spawn) => {
            ("Sub-agent", format!("{}: {}", args["role"].as_str().unwrap_or_default(), args["task"].as_str().unwrap_or_default().lines().next().unwrap_or_default()))
        }
        Some(ToolName::Background) => {
            let id = args["id"].as_str().unwrap_or_default();
            match (args["action"].as_str(), args["until"].as_str().filter(|until| !until.is_empty())) {
                (Some("stop"), _) => ("Stop", id.to_owned()),
                (_, Some(until)) => ("Wait", format!("{id} for {until}")),
                _ => ("Check", id.to_owned()),
            }
        }
        Some(ToolName::Plan) => {
            let steps = args["steps"].as_array().map_or(0, Vec::len);
            let done = args["steps"].as_array().into_iter().flatten().filter(|step| step["status"] == "done").count();
            ("Plan", format!("{done} of {steps} done"))
        }
        // An MCP server's: whose, which, and with what.
        None if let Some((server, tool)) = rein_mcp::parts(&call.name) => ("MCP", format!("{server}: {tool} {}", arguments(call)).trim_end().to_owned()),
        _ => ("Tool", format!("{} {}", call.name, call.arguments)),
    }
}

/// A call's arguments on one line, as JSON, with nothing for none.
pub fn arguments(call: &ToolCall) -> String {
    match serde_json::from_str::<Value>(&call.arguments) {
        Ok(Value::Object(args)) if args.is_empty() => String::new(),
        Ok(args) => args.to_string(),
        Err(_) => call.arguments.clone(),
    }
}

/// A command step's whole command.
pub fn command(call: &ToolCall) -> Option<String> {
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    args["command"].as_str().map(str::to_owned)
}

/// What rein is doing while a step runs, for the line over the box:
/// `Running cargo test`.
pub fn doing(call: &ToolCall) -> String {
    let (name, target) = describe(call);
    let verb = match name {
        "Run" => "Running",
        "Read" => "Reading",
        "Edit" => "Editing",
        "Write" => "Writing",
        "Recall" => "Searching the conversation for",
        "Search" => "Searching for",
        "Find" => "Finding",
        "Plan" => return "Updating the plan".to_owned(),
        "Message" => "Writing to",
        "Delegate" => "Handing over to",
        "Sub-agent" => "Starting sub-agent",
        "Peers" => return "Looking at the other reins".to_owned(),
        "Memory" => return format!("Memory: {target}"),
        _ => "Using",
    };
    format!("{verb} {target}")
}

/// A step's line: `lead` (rein's label, or room for it), then two columns
/// in, `mark`, the tool, its target, then `status`. It never wraps: the
/// target is cut to fit `columns`, so a long command takes one line.
pub fn line(call: &ToolCall, lead: &str, mark: &str, status: &str, style: Style, columns: usize) -> String {
    line_saying(call, lead, mark, status, "", style, columns)
}

/// The same, with what the step `said` after its status, like a command's
/// last line: where the target would keep less than [`TARGET_MOST_CUT`],
/// that's cut first, and then left out. The status is never cut.
pub fn line_saying(call: &ToolCall, lead: &str, mark: &str, status: &str, said: &str, style: Style, columns: usize) -> String {
    let (name, target) = describe(call);
    // A change to a file is what you'd look at; the rest of the work is grey.
    let changes = ToolName::of(call).is_some_and(ToolName::changes_files);
    let name = format!("{name:<6}");
    let head = format!("{lead}{}{mark} {} ", " ".repeat(STEP), if changes { style.bold(&name) } else { style.dim(&name) });
    let target = |room: usize| {
        let target = fit_width(&target, room);
        if changes { target } else { style.dim(&target) }
    };
    let tail = if status.is_empty() { String::new() } else { format!("  {status}") };
    let left = columns.saturating_sub(width(&head) + width(&tail) + TARGET_MOST_CUT + 1);
    let said = match left {
        0..8 => String::new(),
        _ if width(said) <= left => said.to_owned(),
        _ => fit_width(said, left),
    };
    let room = columns.saturating_sub(width(&head) + width(&tail) + width(&said) + 1).max(8);
    format!("{head}{}{tail}{said}", target(room))
}

/// Reads in a row, on one line: their paths, as many as fit and then how
/// many more, and what they came to. Ctrl-O shows each.
///
/// ```text
/// rein   ● Read   src/parser.rs, src/lexer.rs +2  4 files · 612 lines
/// ```
pub fn reads_line(paths: &[String], statuses: &[String], lead: &str, mark: &str, style: Style, columns: usize) -> String {
    let lines: Option<u64> = statuses
        .iter()
        .map(|status| {
            let plain = strip(status);
            plain.strip_suffix(" lines").or_else(|| plain.strip_suffix(" line")).and_then(|n| n.parse::<u64>().ok())
        })
        .sum();
    let files = format!("{} files", paths.len());
    let total = match lines {
        Some(1) => format!("{files} · 1 line"),
        Some(n) => format!("{files} · {n} lines"),
        None => files,
    };
    let head = format!("{lead}{}{mark} {} ", " ".repeat(STEP), style.dim(&format!("{:<6}", "Read")));
    let tail = format!("  {}", style.dim(&total));
    let room = columns.saturating_sub(width(&head) + width(&tail) + 1).max(8);
    let mut shown = String::new();
    for (i, path) in paths.iter().enumerate() {
        let joined = if shown.is_empty() { path.clone() } else { format!("{shown}, {path}") };
        let left = paths.len() - i - 1;
        let more = if left > 0 { format!(" +{left}") } else { String::new() };
        if i > 0 && width(&joined) + width(&more) > room {
            shown = format!("{shown} +{}", paths.len() - i);
            break;
        }
        shown = joined;
    }
    format!("{head}{}{tail}", style.dim(&fit_width(&shown, room)))
}

/// A finished step's mark: a green ✓ for one that worked, a red ✗ for one
/// that failed, and a grey ○ for neither, like one declined.
pub fn mark(ok: Option<bool>, style: Style) -> String {
    match ok {
        Some(true) => style.green("✓"),
        Some(false) => style.red("✗"),
        None => style.dim("○"),
    }
}

/// Steps that only looked, in a row, on one line: what they came to,
/// `Read 2 files, searched for 3 patterns`. Ctrl-O shows each.
pub fn looks_line(reads: usize, searches: usize, lead: &str, mark: &str, style: Style, columns: usize) -> String {
    let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = Vec::new();
    if reads > 0 {
        parts.push(format!("read {}", count(reads, "file", "files")));
    }
    if searches > 0 {
        parts.push(format!("searched for {}", count(searches, "pattern", "patterns")));
    }
    let said = parts.join(", ");
    let said = said.get(..1).map_or(String::new(), str::to_uppercase) + said.get(1..).unwrap_or_default();
    format!("{lead}{}{mark} {}", " ".repeat(STEP), style.dim(&fit_width(&said, columns.saturating_sub(width(lead) + STEP + 3).max(8))))
}

/// What a step's output came to: a short result for its first line, the
/// output lines worth showing under it, and how many more there are.
#[derive(Clone)]
pub struct Outcome {
    /// It worked (a green ✓), failed (a red ✗), or neither, like a
    /// declined step (a grey ○).
    pub ok: Option<bool>,
    pub status: String,
    /// What it said after its status, where there's room: ` · ` and a
    /// command's last line.
    pub said: String,
    pub details: Vec<String>,
    pub more: usize,
    /// What `more` counts: `line`, or `file`.
    pub unit: &'static str,
}

pub fn outcome(call: &ToolCall, output: &str, style: Style) -> Outcome {
    let plain = |ok: Option<bool>, status: String| Outcome { ok, status, said: String::new(), details: Vec::new(), more: 0, unit: "line" };
    if let Some(error) = output.strip_prefix("error: ") {
        return plain(Some(false), style.red(&first_line(error)));
    }
    if output.starts_with("The user declined") {
        return plain(None, style.dim("declined"));
    }
    if output.starts_with("The user interrupted") {
        return plain(None, style.dim("interrupted"));
    }
    // You wrote before it started.
    if output.starts_with("Not run: the user sent a message") {
        return plain(None, style.dim("skipped"));
    }
    match ToolName::of(call) {
        Some(ToolName::Bash) => {
            let mut lines: Vec<&str> = output.lines().collect();
            // What the command printed; rein's notes after it are the model's.
            let ended = tools::ended(output);
            if let Some((at, _)) = ended {
                lines.truncate(at);
            }
            let code = match ended {
                Some((_, Ended::Code(code))) => Some(code.to_owned()),
                _ => None,
            };
            let stopped = matches!(ended, Some((_, Ended::Signal)));
            let failed = code.as_deref() != Some("0");
            let status = match code.as_deref() {
                Some("0") => style.dim("exit 0"),
                Some(code) => style.red(&format!("exit {code}")),
                None if stopped => style.red("stopped by a signal"),
                None => style.dim("no exit code"),
            };
            // Gone on in the background: that's how it went, so far.
            let behind = tools::in_background(output);
            if let Some(at) = lines.iter().position(|line| line.starts_with(tools::MOVED)) {
                lines.truncate(at);
            }
            let (failed, status) = match behind {
                Some(id) if output.starts_with(tools::STARTED) => {
                    lines.clear();
                    (false, style.dim(&format!("started as {id}")))
                }
                Some(id) => (false, style.yellow(&format!("goes on in the background as {id}"))),
                None => (failed, status),
            };
            let body: Vec<&str> = lines.into_iter().filter(|line| !line.trim().is_empty()).collect();
            // Success is how it ended, on its line; a failure shows where it
            // says what went wrong, under it.
            if !failed {
                let said = match body.last() {
                    Some(last) if behind.is_none() => style.dim(&format!(" · {}", fit(last.trim(), 80))),
                    _ => String::new(),
                };
                return Outcome { said, ..plain(Some(true), status) };
            }
            let mut shown: Vec<&str> =
                body.iter().copied().filter(|line| ["error", "FAILED", "panicked", "Error", "failed"].iter().any(|w| line.contains(w))).take(SHOWN).collect();
            if shown.is_empty() {
                shown = body[body.len().saturating_sub(SHOWN)..].to_vec();
            }
            Outcome { ok: Some(false), status, said: String::new(), more: body.len() - shown.len(), details: shown.into_iter().map(str::to_owned).collect(), unit: "line" }
        }
        Some(ToolName::Read) if output.starts_with("[unchanged:") => plain(Some(true), style.dim("unchanged: already in the conversation")),
        // An image, as rein said it: `[image a.png, 1280×800]`.
        Some(ToolName::Read) if output.starts_with("[image ") && output.lines().count() == 1 => {
            let size = output.trim_matches(['[', ']']).split(", ").skip(1).collect::<Vec<_>>().join(", ");
            plain(Some(true), style.dim(&format!("image, {}", size.split(':').next().unwrap_or_default())))
        }
        Some(ToolName::Read) => {
            // A partial read ends with a note saying which lines it holds.
            let note = output.lines().last().filter(|line| line.starts_with("[lines "));
            match note {
                // `[lines 1-100 of 164; …]`: the step names the lines it
                // asked for, so then only the total is news.
                Some(note) => {
                    let note = note.trim_matches(['[', ']']).split(';').next().unwrap_or_default();
                    let asked = describe(call).1.contains(", lines ") || describe(call).1.contains(", from line ");
                    let total = note.rsplit(" of ").next().unwrap_or_default();
                    plain(Some(true), style.dim(&if asked { format!("of {total} lines") } else { note.to_owned() }))
                }
                None => {
                    let lines = output.lines().count();
                    plain(Some(true), style.dim(&format!("{lines} line{}", if lines == 1 { "" } else { "s" })))
                }
            }
        }
        // The checklist itself, a line per step.
        Some(ToolName::Plan) => {
            let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
            let details = args["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|step| {
                    let text = step["step"].as_str().unwrap_or_default();
                    match step["status"].as_str() {
                        Some("done") => style.dim(&format!("☑ {text}")),
                        Some("in_progress") => style.bold(&format!("▸ {text}")),
                        _ => format!("☐ {text}"),
                    }
                })
                .collect();
            Outcome { ok: Some(true), status: String::new(), said: String::new(), details, more: 0, unit: "line" }
        }
        // The summary; which files is Ctrl-O away.
        Some(ToolName::Grep | ToolName::Glob) => {
            let summary = first_line(output);
            if summary.starts_with("No ") {
                return plain(Some(true), style.dim(if ToolName::of(call) == Some(ToolName::Grep) { "nothing found" } else { "no files" }));
            }
            plain(Some(true), style.dim(summary.split(';').next().unwrap_or_default()))
        }
        // What it found, by server, as rein said it.
        Some(ToolName::ToolSearch) => plain(Some(true), style.dim(&first_line(output))),
        Some(ToolName::WebSearch) => searched(output, style),
        Some(ToolName::Skill) if output.starts_with("[unchanged:") => plain(Some(true), style.dim("unchanged: already in the conversation")),
        Some(ToolName::Skill) => {
            let lines = output.lines().count().saturating_sub(2);
            plain(Some(true), style.dim(&format!("loaded, {lines} line{}", if lines == 1 { "" } else { "s" })))
        }
        Some(ToolName::Memory) => {
            let said = match output.split_whitespace().next() {
                Some("Saved") => "remembered",
                Some("Updated") => "updated",
                Some("Forgot") => "forgotten",
                _ => "read",
            };
            plain(Some(true), style.dim(said))
        }
        // `A line matched: ready on 3000`, `b1 (npm run dev) is still running.`
        Some(ToolName::Background) => {
            let first = output.lines().next().unwrap_or_default();
            let said = match first.split_once(": ") {
                Some(("A line matched", line)) => line,
                _ => first.split_once(") ").map_or(first, |(_, rest)| rest).trim_end_matches('.'),
            };
            plain(Some(true), style.dim(&fit(said, 100)))
        }
        Some(ToolName::Recall) => {
            let found = output.lines().filter(|line| line.starts_with('#')).count();
            if found == 0 { plain(Some(true), style.dim("nothing found")) } else { plain(Some(true), style.dim(&format!("{found} match{}", if found == 1 { "" } else { "es" }))) }
        }
        // How many lines it adds and takes away, and how many places where
        // it was more than one.
        Some(ToolName::Edit) => {
            let places = first_line(output).rsplit_once("replaced ").and_then(|(_, rest)| rest.split(' ').next()?.parse::<usize>().ok());
            let more = places.filter(|&n| n > 1).map(|n| style.dim(&format!(" · {n} places"))).unwrap_or_default();
            plain(Some(true), format!("{}{more}", change_counts(call, style)))
        }
        // `created a.rs (30 lines)` → `30 lines`: the step already says
        // which file.
        _ => {
            let said = first_line(output);
            // `created a.rs (30 lines)` → `30 lines`.
            let short = match said.rsplit_once(" (") {
                Some((_, rest)) if ToolName::of(call) == Some(ToolName::Write) => rest.trim_end_matches(')').to_owned(),
                _ => said.split_once(": ").map_or(said.as_str(), |(_, rest)| rest).to_owned(),
            };
            plain(Some(true), style.dim(&short))
        }
    }
}

/// The lines under a step: `└` before the first, then a count of the rest.
/// `viewer` adds how to see the rest: Ctrl-O.
pub fn detail_lines(outcome: &Outcome, style: Style, columns: usize, viewer: bool) -> Vec<String> {
    let pad = " ".repeat(INDENT);
    let room = columns.saturating_sub(INDENT + 3).max(20);
    let mut lines: Vec<String> = outcome
        .details
        .iter()
        .enumerate()
        // A plan's lines are styled already; other output is dimmed.
        .map(|(i, line)| {
            let line = if line.contains('\x1b') || line.starts_with('☐') { fit_width(line, room) } else { style.dim(&fit(line, room)) };
            format!("{pad}{}{line}", style.dim(if i == 0 { "└ " } else { "  " }))
        })
        .collect();
    if outcome.more > 0 {
        let s = if outcome.more == 1 { "" } else { "s" };
        let hint = if viewer { " · ctrl-o to see all" } else { "" };
        lines.push(format!("{pad}{}", style.dim(&format!("… {} more {}{s}{hint}", outcome.more, outcome.unit))));
    }
    lines
}

/// A line of a file change as it's shown: what it takes away or adds,
/// at which line of the file, or a note about the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Changed {
    Line { added: bool, number: Option<usize>, text: String },
    Note(String),
}

/// What an edit or a file written changes, with line numbers. Models often
/// repeat a few unchanged lines around a change, to make it unique; only
/// what changes is shown. `applied` says the change is already in the
/// file, so the line numbers come from where the new text is rather than
/// the old.
pub fn change(call: &ToolCall, root: &Path, applied: bool) -> Vec<Changed> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let Some((old, new)) = sides(call, &args) else { return Vec::new() };
    let first = line_of(call, &args, root, if applied { new } else { old });
    let (old, new, same_start) = changed_lines(old, new);
    let first = first.map(|first| first + same_start);
    let mut lines = Vec::new();
    for (added, side) in [(false, old), (true, new)] {
        for (i, text) in side.iter().take(DIFF_LINES).enumerate() {
            lines.push(Changed::Line { added, number: first.map(|first| first + i), text: (*text).to_owned() });
        }
        if side.len() > DIFF_LINES {
            lines.push(Changed::Note(format!("… {} more lines", side.len() - DIFF_LINES)));
        }
    }
    if args["replace_all"].as_bool() == Some(true) {
        lines.push(Changed::Note("(every occurrence)".to_owned()));
    }
    lines
}

/// Where the change starts in the file, for the line numbers: where
/// `found` is, in an edit.
fn line_of(call: &ToolCall, args: &Value, root: &Path, found: &str) -> Option<usize> {
    match ToolName::of(call) {
        Some(ToolName::Edit) => {
            args["path"].as_str().and_then(|path| std::fs::read_to_string(root.join(path)).ok()).and_then(|text| text.find(found).map(|at| text[..at].matches('\n').count() + 1))
        }
        _ => Some(1),
    }
}

/// A change to a file, all of it, for a client to show as it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Whole {
    pub path: String,
    /// What it replaces, for an edit: a file written whole replaces nothing.
    pub old: Option<String>,
    pub new: String,
    /// The line the old text starts on, as far as can be told.
    pub line: Option<usize>,
}

/// What `call` would change, if it changes a file, with nothing left out:
/// a yes goes to all of it. `root` is where its path starts.
pub fn whole_change(call: &ToolCall, root: &Path) -> Option<Whole> {
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let (old, new) = sides(call, &args)?;
    let path = root.join(args["path"].as_str()?).to_string_lossy().into_owned();
    let line = line_of(call, &args, root, old);
    Some(Whole { path, old: (!old.is_empty()).then(|| old.to_owned()), new: new.to_owned(), line })
}

/// A change's lines at `columns`, under the step: the lines it takes away
/// on a red tint and those it adds on a green one, each to the window's
/// edge, as Claude Code shows them; where the terminal didn't say its
/// background, in red and green instead.
pub fn change_lines(change: &[Changed], style: Style, columns: usize) -> Vec<String> {
    let pad = " ".repeat(INDENT);
    let room = columns.saturating_sub(INDENT + 8).max(20);
    change
        .iter()
        .map(|line| match line {
            Changed::Note(note) => format!("{pad}{}", style.dim(&format!("     {note}"))),
            Changed::Line { added, number, text } => {
                let number = style.dim(&number.map_or("    ".to_owned(), |n| format!("{n:>4}")));
                let (tint, sign) = if *added { (Tint::Added, style.green("+")) } else { (Tint::Removed, style.red("-")) };
                let text = fit(text, room);
                let tinted = tint::behind(tint, style).is_some();
                let text = match (tinted, added) {
                    (true, _) => text,
                    (false, true) => style.green(&text),
                    (false, false) => style.red(&text),
                };
                format!("{pad}{}", tint::fill(&format!("{number} {sign} {text}"), tint, style, columns.saturating_sub(INDENT)))
            }
        })
        .collect()
}

/// How a change to a file is said on its step's line: `+5 -1` for an
/// edit, `+2` for one that only adds, `30 lines` for a file written whole.
pub fn change_counts(call: &ToolCall, style: Style) -> String {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let Some((old, new)) = sides(call, &args) else { return String::new() };
    if ToolName::of(call) == Some(ToolName::Write) {
        let lines = new.lines().count();
        return style.dim(&format!("{lines} line{}", if lines == 1 { "" } else { "s" }));
    }
    let (old, new, _) = changed_lines(old, new);
    let added = (!new.is_empty()).then(|| style.green(&format!("+{}", new.len())));
    let removed = (!old.is_empty()).then(|| style.red(&format!("-{}", old.len())));
    added.into_iter().chain(removed).collect::<Vec<_>>().join(" ")
}

/// The text a change takes away and puts in: an edit's, or none and the
/// whole of a file written.
fn sides<'a>(call: &ToolCall, args: &'a Value) -> Option<(&'a str, &'a str)> {
    match ToolName::of(call) {
        Some(ToolName::Edit) => Some((args["old_text"].as_str().unwrap_or_default(), args["new_text"].as_str().unwrap_or_default())),
        Some(ToolName::Write) => Some(("", args["content"].as_str().unwrap_or_default())),
        _ => None,
    }
}

/// The lines that differ between `old` and `new`, without those they
/// start and end with alike, and how many they start with alike.
fn changed_lines<'a>(old: &'a str, new: &'a str) -> (Vec<&'a str>, Vec<&'a str>, usize) {
    let (old, new): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let same_start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let same_end = old[same_start..].iter().rev().zip(new[same_start..].iter().rev()).take_while(|(a, b)| a == b).count();
    (old[same_start..old.len() - same_end].to_vec(), new[same_start..new.len() - same_end].to_vec(), same_start)
}

/// The files a search's output names, as far as it shows them: a path per
/// line after the summary for `glob`, `path (N matches)` per line for
/// `grep`'s files, and each group's first line for the lines it found.
#[cfg(test)]
fn found_files(output: &str) -> Vec<String> {
    let mut lines = output.lines();
    let summary = lines.next().unwrap_or_default();
    if summary.contains(" with matches (") {
        return lines.map(|line| line.rsplit_once(" (").map_or(line, |(path, _)| path).to_owned()).collect();
    }
    if !summary.contains(" match") {
        return lines.map(str::to_owned).collect();
    }
    let mut files = Vec::new();
    let mut next_is_file = true;
    for line in lines {
        if line.is_empty() {
            next_is_file = true;
        } else if std::mem::take(&mut next_is_file) {
            files.push(line.to_owned());
        }
    }
    files
}

/// A web search, as rein said it (see `rein_agent::web`): the sources'
/// hosts and who searched, then the answer's first lines.
fn searched(output: &str, style: Style) -> Outcome {
    let mut lines = output.lines();
    let head = lines.next().unwrap_or_default();
    // "[web search by gpt-6.1-sol, its provider's index: …]"
    let by = head.strip_prefix("[web search by ").and_then(|rest| rest.split_once(':')).map(|(by, _)| by.replace("its provider's index", "cached")).unwrap_or_default();
    let rest: Vec<&str> = lines.collect();
    let ends = rest.iter().position(|line| line.starts_with("Searched for: ") || line.starts_with("Opened: ") || *line == "Sources:").unwrap_or(rest.len());
    let answer: Vec<&str> = rest[..ends].iter().copied().filter(|line| !line.trim().is_empty()).collect();
    let mut hosts: Vec<&str> = Vec::new();
    for line in rest[ends..].iter().filter_map(|line| line.strip_prefix("- ")) {
        let url = line.rsplit_once(": ").map_or(line, |(_, url)| url);
        let host = url.split_once("://").map_or(url, |(_, rest)| rest).split(['/', '?', '#']).next().unwrap_or_default();
        if !host.is_empty() && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    let sources = match hosts.len() {
        0 => "no sources".to_owned(),
        n => format!("{n} source{}: {}", if n == 1 { "" } else { "s" }, hosts.iter().take(3).copied().collect::<Vec<_>>().join(", ") + if n > 3 { ", …" } else { "" }),
    };
    let shown: Vec<String> = answer.iter().take(SHOWN).map(|line| (*line).to_owned()).collect();
    Outcome { ok: Some(true), status: style.dim(&format!("{sources} · {by}")), said: String::new(), more: answer.len() - shown.len(), details: shown, unit: "line" }
}

fn first_line(text: &str) -> String {
    fit(text.lines().next().unwrap_or_default(), 100)
}

/// `text` cut to `max` columns, with `…` if anything was cut.
fn fit(text: &str, max: usize) -> String {
    let text = text.replace('\t', "    ");
    if unicode_width::UnicodeWidthStr::width(text.as_str()) <= max {
        return text;
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_to_another_rein_says_who_to_and_what() {
        let call = |name: &str, args: Value| ToolCall { call_id: "c".into(), name: name.into(), arguments: args.to_string() };
        assert_eq!(describe(&call("message", serde_json::json!({ "to": "api", "text": "is it done?\nno rush" }))), ("Message", "api: is it done? …".to_owned()));
        assert_eq!(doing(&call("delegate", serde_json::json!({ "to": "web", "task": "add /health" }))), "Handing over to web: add /health");
        assert_eq!(doing(&call("peers", serde_json::json!({}))), "Looking at the other reins");
    }
    use rein_llm::serde_json::json;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall { call_id: "c".into(), name: name.into(), arguments: args.to_string() }
    }

    fn status(name: &str, args: Value, output: &str) -> (String, Vec<String>, usize) {
        let o = outcome(&call(name, args), output, Style::plain());
        (o.status + &o.said, o.details, o.more)
    }

    #[test]
    fn a_web_search_says_what_it_went_by_and_who_searched() {
        assert_eq!(describe(&call("web_search", json!({"query": "latest rust"}))), ("Search", "latest rust".into()));
        let said = "[web search by gpt-6.1-sol, its provider's index: what the web says is its own word, not the user's]\n\
                    Rust 1.99.1.\n\nIt added C variadics.\nAnd more.\n\nSearched for: rust\nSources:\n- Blog: https://blog.rust-lang.org/x\n\
                    - https://github.com/rust-lang/rust/releases\n- https://blog.rust-lang.org/y\n- https://a.example/\n- https://b.example/";
        let (said, details, more) = status("web_search", json!({"query": "latest rust"}), said);
        assert_eq!(said, "4 sources: blog.rust-lang.org, github.com, a.example, … · gpt-6.1-sol, cached");
        assert_eq!((details, more), (vec!["Rust 1.99.1.".to_owned(), "It added C variadics.".to_owned()], 1));
        let live = "[web search by sonnet, live: what the web says is its own word, not the user's]\nNothing found.\n";
        assert_eq!(status("web_search", json!({"query": "x"}), live).0, "no sources · sonnet, live");
    }

    #[test]
    fn describes_each_step() {
        assert_eq!(describe(&call("bash", json!({"command": "cargo test"}))), ("Run", "cargo test".into()));
        assert_eq!(describe(&call("bash", json!({"command": "cd x\nmake"}))), ("Run", "cd x …".into()));
        assert_eq!(describe(&call("read", json!({"path": "a.rs", "start_line": 10, "line_count": 5}))), ("Read", "a.rs, lines 10-14".into()));
    }

    #[test]
    fn says_what_a_running_step_is_doing() {
        assert_eq!(doing(&call("bash", json!({"command": "cargo test"}))), "Running cargo test");
        assert_eq!(doing(&call("read", json!({"path": "a.rs"}))), "Reading a.rs");
        assert_eq!(doing(&call("plan", json!({"steps": []}))), "Updating the plan");
    }

    #[test]
    fn a_step_takes_one_line_however_long_its_command() {
        let long = call("bash", json!({"command": format!("python3 -c \"{}\"", "x = 1; ".repeat(40))}));
        let shown = line(&long, "rein   ", "✓", "exit 0", Style::plain(), 60);
        assert!(shown.starts_with("rein     ✓ Run    python3 -c \"x = 1;"), "two columns in from the words: {shown}");
        assert!(shown.ends_with("…  exit 0"), "{shown}");
        assert_eq!(width(&shown), 59);
        // What a command said last is cut before the command is cut to
        // nothing, then left out; how it ended stays.
        let test = call("bash", json!({"command": "cargo test --workspace"}));
        let said = format!(" · {}", "test result: ok. ".repeat(6));
        let shown = line_saying(&test, "       ", "✓", "exit 0", &said, Style::plain(), 70);
        assert!(shown.contains("cargo test --workspace  exit 0 · test result") && shown.ends_with('…'), "{shown}");
        assert!(width(&shown) < 70, "{shown}");
        let narrow = line_saying(&test, "rein   ", "✓", "exit 0", &said, Style::plain(), 46);
        assert!(narrow.ends_with("  exit 0") && narrow.contains("cargo test"), "{narrow}");
    }

    #[test]
    fn the_work_is_grey_but_a_change_to_a_file_and_marks_say_how_it_went() {
        let s = Style::colored();
        let run = line(&call("bash", json!({"command": "ls"})), "", &mark(Some(true), s), "", s, 60);
        assert!(run.contains("\x1b[90mRun") && run.contains("\x1b[32m✓"), "{run:?}");
        let edit = line(&call("edit", json!({"path": "a.rs"})), "", &mark(Some(false), s), "", s, 60);
        assert!(edit.contains("\x1b[1mEdit") && edit.contains("\x1b[31m✗") && !edit.contains("\x1b[90ma.rs"), "{edit:?}");
        assert_eq!(strip(&mark(None, s)), "○", "declined");
    }

    #[test]
    fn a_run_of_reads_and_searches_says_what_it_came_to() {
        assert_eq!(looks_line(2, 1, "rein   ", "✓", Style::plain(), 80), "rein     ✓ Read 2 files, searched for 1 pattern");
        assert_eq!(looks_line(1, 3, "", "✓", Style::plain(), 80), "  ✓ Read 1 file, searched for 3 patterns");
        assert_eq!(looks_line(0, 2, "", "✓", Style::plain(), 80), "  ✓ Searched for 2 patterns");
    }

    #[test]
    fn sums_up_commands() {
        // Success is how it ended, on its line; nothing under it.
        let passed = "Compiling\n\n3 passed in 0.04s\n[exit code 0]";
        assert_eq!(status("bash", json!({}), passed), ("exit 0 · 3 passed in 0.04s".into(), Vec::new(), 0));
        // rein's notes after the exit line don't hide it.
        let noted = "3 passed in 0.04s\n[exit code 0]\n[rein: that was the last step this turn may take.]";
        assert_eq!(status("bash", json!({}), noted), ("exit 0 · 3 passed in 0.04s".into(), Vec::new(), 0));
        assert_eq!(status("bash", json!({}), "[exit code 0]").0, "exit 0", "it printed nothing");
        let failing = "Compiling\nrunning 48 tests\ntest a ... ok\ntest b ... FAILED\nleft: 1\n\nmore\n[exit code 101]";
        let (s, details, more) = status("bash", json!({}), failing);
        assert_eq!(s, "exit 101");
        assert_eq!(details, ["test b ... FAILED"]);
        assert_eq!(more, 5);
        assert_eq!(status("bash", json!({}), "The user declined to run this command.").0, "declined");
        assert_eq!(status("bash", json!({}), rein_agent::agent::SKIPPED).0, "skipped");
        assert_eq!(status("bash", json!({}), "error: the command took longer than 60s and was stopped").0, "the command took longer than 60s and was stopped");
        assert_eq!(outcome(&call("bash", json!({})), "x\n[exit code 2]", Style::plain()).ok, Some(false));
    }

    #[test]
    fn shows_a_plan_as_a_checklist() {
        let plan = call(
            "plan",
            json!({"steps": [
                {"step": "Read the parser", "status": "done"},
                {"step": "Fix trailing commas", "status": "in_progress"},
                {"step": "Run the tests", "status": "pending"}
            ]}),
        );
        assert_eq!(describe(&plan), ("Plan", "1 of 3 done".into()));
        let o = outcome(&plan, "Plan updated: 1 of 3 done.", Style::plain());
        assert_eq!(o.details, ["☑ Read the parser", "▸ Fix trailing commas", "☐ Run the tests"]);
    }

    #[test]
    fn sums_up_reads_and_changes() {
        assert_eq!(status("read", json!({}), "a\nb\nc\n").0, "3 lines");
        assert_eq!(status("read", json!({}), "x\n[lines 10-14 of 900; read again with start_line 15 for more]").0, "lines 10-14 of 900");
        let edit = json!({"path": "a.rs", "old_text": "a\nb\nc", "new_text": "a\nB\nB2\nc"});
        assert_eq!(status("edit", edit.clone(), "edited a.rs: replaced 1 place").0, "+2 -1", "the lines alike at either end don't count");
        assert_eq!(status("edit", edit, "edited a.rs: replaced 3 places").0, "+2 -1 · 3 places");
        assert_eq!(status("write", json!({}), "created a.rs (30 lines)").0, "30 lines");
        let partial = "x\n[lines 1-100 of 164; read again with start_line 101 for more]";
        assert_eq!(status("read", json!({"path": "a", "start_line": 1, "line_count": 100}), partial).0, "of 164 lines");
        assert_eq!(status("recall", json!({}), "#3 tool result:\nx\n\n#9 user:\ny").0, "2 matches");
    }

    #[test]
    fn a_run_of_reads_is_one_line() {
        let paths: Vec<String> = ["src/parser.rs", "src/lexer.rs", "src/ast.rs", "src/main.rs"].map(str::to_owned).to_vec();
        let statuses: Vec<String> = ["240 lines", "120 lines", "200 lines", "52 lines"].map(str::to_owned).to_vec();
        let wide = reads_line(&paths, &statuses, "rein   ", "●", Style::plain(), 100);
        assert_eq!(wide, "rein     ● Read   src/parser.rs, src/lexer.rs, src/ast.rs, src/main.rs  4 files · 612 lines");
        let narrow = reads_line(&paths, &statuses, "rein   ", "●", Style::plain(), 72);
        assert_eq!(narrow, "rein     ● Read   src/parser.rs, src/lexer.rs +2  4 files · 612 lines", "the names that fit, then how many more");
        let parts: Vec<String> = ["240 lines", "lines 10-14 of 900"].map(str::to_owned).to_vec();
        assert!(reads_line(&paths[..2], &parts, "", "●", Style::plain(), 100).ends_with("  2 files"), "parts of files don't add up to lines");
        assert_eq!(status("read", json!({}), "[unchanged: a.rs is as it was…]").0, "unchanged: already in the conversation");
    }

    #[test]
    fn sums_up_searches() {
        let grep = call("grep", json!({"pattern": "parse_list", "path": "src", "glob": "*.rs"}));
        assert_eq!(describe(&grep), ("Search", "\"parse_list\" in src (*.rs)".into()));
        assert_eq!(doing(&grep), "Searching for \"parse_list\" in src (*.rs)");
        // What it came to; which files is Ctrl-O away.
        let lines = "3 matches in 3 files\nsrc/main.rs\n2:    parse_list();\n\nsrc/parser.rs\n2:fn parse_list() {}\n\nsrc/lib.rs\n9:x";
        assert_eq!(status("grep", json!({}), lines), ("3 matches in 3 files".into(), Vec::new(), 0));
        assert_eq!(found_files(lines), ["src/main.rs", "src/parser.rs", "src/lib.rs"]);
        let files = "2 files with matches (4 matching lines)\nsrc/main.rs (1 match)\nsrc/parser.rs (3 matches)";
        assert_eq!(found_files(files), ["src/main.rs", "src/parser.rs"]);
        assert_eq!(status("grep", json!({}), "No matches in 4 files (files that .gitignore leaves out aren't searched).").0, "nothing found");
        let cut = "301 matches in 2 files; not all shown: narrow the pattern\na.txt\n1:match";
        assert_eq!(status("grep", json!({}), cut).0, "301 matches in 2 files");

        let glob = call("glob", json!({"pattern": "*.rs", "path": null}));
        assert_eq!(describe(&glob), ("Find", "*.rs".into()));
        let o = outcome(&glob, "3 files\nsrc/main.rs\nsrc/ui/box.rs\ntests/screen.rs", Style::plain());
        assert_eq!((o.status.as_str(), o.details.len(), o.more), ("3 files", 0, 0));
        assert_eq!(status("glob", json!({}), "No files match \"*.py\" (…).").0, "no files");
    }

    #[test]
    fn shows_a_change_with_line_numbers_and_no_frame() {
        let dir = std::env::temp_dir().join(format!("rein-steps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "one\ntwo\nprint(\"EMPTY\")\n").unwrap();
        let edit = call("edit", json!({"path": "a.py", "old_text": "print(\"EMPTY\")", "new_text": "print(\"none\")", "replace_all": false}));
        let pad = " ".repeat(INDENT);
        let lines = change_lines(&change(&edit, &dir, false), Style::plain(), 80);
        assert_eq!(lines, [format!("{pad}   3 - print(\"EMPTY\")"), format!("{pad}   3 + print(\"none\")")]);
        assert_eq!(change_counts(&edit, Style::plain()), "+1 -1");
        let adds = call("edit", json!({"path": "a.py", "old_text": "one", "new_text": "import re\none"}));
        assert_eq!(change_counts(&adds, Style::plain()), "+1", "no -0");

        // Unchanged lines around the change are left out.
        let padded = call("edit", json!({"path": "a.py", "old_text": "two\nprint(\"EMPTY\")", "new_text": "two\nprint(\"none\")", "replace_all": false}));
        let lines = change_lines(&change(&padded, &dir, false), Style::plain(), 80);
        assert_eq!(lines, [format!("{pad}   3 - print(\"EMPTY\")"), format!("{pad}   3 + print(\"none\")")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changes_lines_are_tinted_to_the_edge_once_the_background_is_known() {
        // Only this test sets the background, and every other one works
        // the same with it or without.
        crate::ui::tint::set_background((21, 23, 28));
        let lines = [Changed::Line { added: true, number: Some(9), text: "x".into() }, Changed::Line { added: false, number: Some(9), text: "y".into() }];
        let shown = change_lines(&lines, Style::colored(), 40);
        assert!(shown.iter().all(|line| width(line) == 40), "to the edge: {shown:?}");
        assert!(shown[0].contains("48;") && shown[1].contains("48;"), "{shown:?}");
        assert!(!shown[0].contains("\x1b[32mx"), "on a tint the text keeps its own colour: {:?}", shown[0]);
    }

    #[test]
    fn shows_details_under_the_step() {
        let o = Outcome { ok: Some(true), status: String::new(), said: String::new(), details: vec!["first".into(), "second".into()], more: 1, unit: "line" };
        let pad = " ".repeat(INDENT);
        assert_eq!(detail_lines(&o, Style::plain(), 80, false), [format!("{pad}└ first"), format!("{pad}  second"), format!("{pad}… 1 more line")]);
        assert_eq!(detail_lines(&o, Style::plain(), 80, true)[2], format!("{pad}… 1 more line · ctrl-o to see all"));
    }
}
