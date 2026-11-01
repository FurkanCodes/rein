//! How each step rein takes looks on screen: a label, what it's about, and
//! a short result, with a few lines of output where they help.
//!
//! ```text
//! read   todo.py  164 lines
//! run    cargo test  ✗ exit 101
//!        └ test parser::trailing_comma ... FAILED
//!        … 209 more lines
//! ```

use std::path::Path;

use rein_llm::ToolCall;
use rein_llm::serde_json::{self, Value};

use super::style::{GUTTER, Style, width};

/// Output lines shown under a step; the rest is summed up.
const SHOWN: usize = 2;
/// Diff lines shown per side in a framed diff.
const DIFF_LINES: usize = 12;

/// The label and what the step is about: `("run", "cargo test")`.
pub fn describe(call: &ToolCall) -> (&'static str, String) {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let path = args["path"].as_str().unwrap_or_default().to_owned();
    match call.name.as_str() {
        "read" => {
            let range = match (args["start_line"].as_u64(), args["line_count"].as_u64()) {
                (None, None) => String::new(),
                (start, Some(count)) => {
                    let start = start.unwrap_or(1);
                    format!(", lines {start}-{}", start + count.max(1) - 1)
                }
                (Some(start), None) => format!(", from line {start}"),
            };
            ("read", path + &range)
        }
        "write" => ("write", path),
        "edit" => ("edit", path),
        "bash" => ("run", args["command"].as_str().unwrap_or_default().to_owned()),
        "recall" => ("recall", args["query"].as_str().unwrap_or_default().to_owned()),
        _ => ("tool", format!("{} {}", call.name, call.arguments)),
    }
}

/// `label  target`, as the first line of a step.
pub fn header(call: &ToolCall, style: Style) -> String {
    let (label, target) = describe(call);
    format!("{}{target}", style.label(label))
}

/// What a step's output came to: a short result for its first line, the
/// output lines worth showing under it, and how many more there are.
pub struct Outcome {
    pub status: String,
    pub details: Vec<String>,
    pub more: usize,
}

pub fn outcome(call: &ToolCall, output: &str, style: Style) -> Outcome {
    let plain = |status: String| Outcome { status, details: Vec::new(), more: 0 };
    if let Some(error) = output.strip_prefix("error: ") {
        return plain(style.red(&format!("✗ {}", first_line(error))));
    }
    if output.starts_with("The user declined") {
        return plain(style.dim("✗ declined"));
    }
    if output.starts_with("The user interrupted") {
        return plain(style.dim("✗ interrupted"));
    }
    match call.name.as_str() {
        "bash" => {
            let mut lines: Vec<&str> = output.lines().collect();
            let code = lines.last().and_then(|last| last.strip_prefix("[exit code ")?.strip_suffix(']')).map(str::to_owned);
            let stopped = lines.last().is_some_and(|last| last.starts_with("[stopped"));
            if code.is_some() || stopped {
                lines.pop();
            }
            let status = match code.as_deref() {
                Some("0") => style.green("✓ exit 0"),
                Some(code) => style.red(&format!("✗ exit {code}")),
                None if stopped => style.red("✗ stopped by a signal"),
                None => style.dim("no exit code"),
            };
            let failed = code.as_deref() != Some("0");
            let body: Vec<&str> = lines.into_iter().filter(|line| !line.trim().is_empty()).collect();
            // A failure shows where it says what went wrong; success, how it ended.
            let mut shown: Vec<&str> = if failed {
                body.iter().copied().filter(|line| ["error", "FAILED", "panicked", "Error", "failed"].iter().any(|w| line.contains(w))).take(SHOWN).collect()
            } else {
                Vec::new()
            };
            if shown.is_empty() {
                shown = body[body.len().saturating_sub(SHOWN)..].to_vec();
            }
            Outcome { status, more: body.len() - shown.len(), details: shown.into_iter().map(str::to_owned).collect() }
        }
        "read" => {
            // A partial read ends with a note saying which lines it holds.
            let note = output.lines().last().filter(|line| line.starts_with("[lines "));
            match note {
                Some(note) => plain(style.dim(note.trim_matches(['[', ']']).split(';').next().unwrap_or_default())),
                None => {
                    let lines = output.lines().count();
                    plain(style.dim(&format!("{lines} line{}", if lines == 1 { "" } else { "s" })))
                }
            }
        }
        "recall" => {
            let found = output.lines().filter(|line| line.starts_with('#')).count();
            if found == 0 {
                plain(style.dim("nothing found"))
            } else {
                plain(style.dim(&format!("{found} match{}", if found == 1 { "" } else { "es" })))
            }
        }
        _ => plain(style.green(&format!("✓ {}", first_line(output)))),
    }
}

/// The lines under a step: `└` before the first, then a count of the rest.
pub fn detail_lines(outcome: &Outcome, style: Style, columns: usize) -> Vec<String> {
    let pad = " ".repeat(GUTTER);
    let room = columns.saturating_sub(GUTTER + 2).max(20);
    let mut lines: Vec<String> = outcome
        .details
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{pad}{}{}", style.dim(if i == 0 { "└ " } else { "  " }), style.dim(&fit(line, room))))
        .collect();
    if outcome.more > 0 {
        let s = if outcome.more == 1 { "" } else { "s" };
        lines.push(format!("{pad}{}", style.dim(&format!("… {} more line{s}", outcome.more))));
    }
    lines
}

/// A file change in a frame, with line numbers: what goes in red, what
/// comes in green.
///
/// ```text
///        ┌──────────────────
///        │ 85 -    print("EMPTY")
///        │ 85 +    print("No tasks yet.")
///        └──────────────────
/// ```
pub fn diff_box(call: &ToolCall, root: &Path, style: Style, columns: usize) -> Vec<String> {
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let (old, new) = match call.name.as_str() {
        "edit" => (args["old_text"].as_str().unwrap_or_default(), args["new_text"].as_str().unwrap_or_default()),
        "write" => ("", args["content"].as_str().unwrap_or_default()),
        _ => return Vec::new(),
    };
    // Where the change starts in the file, for the line numbers.
    let first = match call.name.as_str() {
        "edit" => args["path"]
            .as_str()
            .and_then(|path| std::fs::read_to_string(root.join(path)).ok())
            .and_then(|text| text.find(old).map(|at| text[..at].matches('\n').count() + 1)),
        _ => Some(1),
    };
    // Models often repeat a few unchanged lines around a change, to make
    // it unique; only what changes is shown.
    let (old, new): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let same_start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let same_end = old[same_start..].iter().rev().zip(new[same_start..].iter().rev()).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[same_start..old.len() - same_end], &new[same_start..new.len() - same_end]);
    let first = first.map(|first| first + same_start);
    let pad = " ".repeat(GUTTER);
    let number = |i: usize| first.map_or(String::new(), |first| format!("{:>4} ", first + i));
    let room = columns.saturating_sub(GUTTER + 10).max(20);
    let mut body = Vec::new();
    for (sign, lines, paint) in [("-", old, Style::red as fn(Style, &str) -> String), ("+", new, Style::green)] {
        for (i, line) in lines.iter().take(DIFF_LINES).enumerate() {
            body.push(format!("{}{}", style.dim(&number(i)), paint(style, &format!("{sign} {}", fit(line, room)))));
        }
        if lines.len() > DIFF_LINES {
            body.push(style.dim(&format!("     … {} more lines", lines.len() - DIFF_LINES)));
        }
    }
    if args["replace_all"].as_bool() == Some(true) {
        body.push(style.dim("     (every occurrence)"));
    }
    let rule = "─".repeat(body.iter().map(|line| width(line)).max().unwrap_or(0).clamp(20, columns.saturating_sub(GUTTER + 4).max(20)) + 2);
    let mut lines = vec![format!("{pad}{}", style.dim(&format!("┌{rule}")))];
    lines.extend(body.into_iter().map(|line| format!("{pad}{} {line}", style.dim("│"))));
    lines.push(format!("{pad}{}", style.dim(&format!("└{rule}"))));
    lines
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
    use rein_llm::serde_json::json;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall { call_id: "c".into(), name: name.into(), arguments: args.to_string() }
    }

    fn status(name: &str, args: Value, output: &str) -> (String, Vec<String>, usize) {
        let o = outcome(&call(name, args), output, Style::plain());
        (o.status, o.details, o.more)
    }

    #[test]
    fn describes_each_step() {
        assert_eq!(describe(&call("bash", json!({"command": "cargo test"}))), ("run", "cargo test".into()));
        assert_eq!(describe(&call("read", json!({"path": "a.rs", "start_line": 10, "line_count": 5}))), ("read", "a.rs, lines 10-14".into()));
        assert_eq!(header(&call("edit", json!({"path": "a.rs"})), Style::plain()), "edit   a.rs");
    }

    #[test]
    fn sums_up_commands() {
        assert_eq!(status("bash", json!({}), "3 passed in 0.04s\n[exit code 0]"), ("✓ exit 0".into(), vec!["3 passed in 0.04s".into()], 0));
        let failing = "Compiling\nrunning 48 tests\ntest a ... ok\ntest b ... FAILED\nleft: 1\n\nmore\n[exit code 101]";
        let (s, details, more) = status("bash", json!({}), failing);
        assert_eq!(s, "✗ exit 101");
        assert_eq!(details, ["test b ... FAILED"]);
        assert_eq!(more, 5);
        assert_eq!(status("bash", json!({}), "The user declined to run this command.").0, "✗ declined");
        assert_eq!(status("bash", json!({}), "error: the command took longer than 60s and was stopped").0, "✗ the command took longer than 60s and was stopped");
    }

    #[test]
    fn sums_up_reads_and_changes() {
        assert_eq!(status("read", json!({}), "a\nb\nc\n").0, "3 lines");
        assert_eq!(status("read", json!({}), "x\n[lines 10-14 of 900; read again with start_line 15 for more]").0, "lines 10-14 of 900");
        assert_eq!(status("edit", json!({}), "edited a.rs: replaced 1 place").0, "✓ edited a.rs: replaced 1 place");
        assert_eq!(status("recall", json!({}), "#3 tool result:\nx\n\n#9 user:\ny").0, "2 matches");
    }

    #[test]
    fn frames_a_change_with_line_numbers() {
        let dir = std::env::temp_dir().join(format!("rein-steps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "one\ntwo\nprint(\"EMPTY\")\n").unwrap();
        let edit = call("edit", json!({"path": "a.py", "old_text": "print(\"EMPTY\")", "new_text": "print(\"none\")", "replace_all": false}));
        let lines = diff_box(&edit, &dir, Style::plain(), 80);
        assert_eq!(lines[1], "       │    3 - print(\"EMPTY\")");
        assert_eq!(lines[2], "       │    3 + print(\"none\")");
        assert!(lines[0].starts_with("       ┌──") && lines[3].starts_with("       └──"));

        // Unchanged lines around the change are left out.
        let padded = call("edit", json!({"path": "a.py", "old_text": "two\nprint(\"EMPTY\")", "new_text": "two\nprint(\"none\")", "replace_all": false}));
        let lines = diff_box(&padded, &dir, Style::plain(), 80);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1], "       │    3 - print(\"EMPTY\")");
    }

    #[test]
    fn shows_details_under_the_step() {
        let o = Outcome { status: String::new(), details: vec!["first".into(), "second".into()], more: 1 };
        assert_eq!(detail_lines(&o, Style::plain(), 80), ["       └ first", "         second", "       … 1 more line"]);
    }
}
