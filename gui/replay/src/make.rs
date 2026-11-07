//! The workloads that find what breaks: each is what a real conversation
//! does at its worst, made up the same way every time, so a result on one
//! toolkit can be set beside a result on another.

use std::path::Path;

use serde_json::{Value, json};

use crate::Said;

pub(crate) const NAMES: [&str; 8] = ["long", "code", "table", "burst", "tiny", "tool-output", "history", "reading"];

/// A small generator, so a workload is the same on every run and machine.
struct Dice(u64);

impl Dice {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % n as u64).expect("below a usize")
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.below(from.len())]
    }
}

const WORDS: [&str; 24] = [
    "the",
    "agent",
    "reads",
    "each",
    "file",
    "before",
    "it",
    "changes",
    "anything",
    "and",
    "keeps",
    "what",
    "matters",
    "in",
    "view",
    "while",
    "a",
    "long",
    "transcript",
    "streams",
    "past",
    "every",
    "pane",
    "stays",
];

fn sentence(dice: &mut Dice) -> String {
    let count = 6 + dice.below(14);
    let mut words: Vec<String> = (0..count).map(|_| dice.pick(&WORDS).to_owned()).collect();
    if dice.below(3) == 0 {
        let at = dice.below(words.len());
        words[at] = format!("`{}`", words[at]);
    }
    if dice.below(5) == 0 {
        let at = dice.below(words.len());
        words[at] = format!("**{}**", words[at]);
    }
    format!("{}.", words.join(" "))
}

fn paragraph(dice: &mut Dice) -> String {
    let count = 2 + dice.below(5);
    (0..count).map(|_| sentence(dice)).collect::<Vec<_>>().join(" ")
}

fn code(dice: &mut Dice, lines: usize) -> String {
    let mut text = String::from("```rust\n");
    for i in 0..lines {
        let line = match dice.below(4) {
            0 => format!("fn step_{i}(input: &str) -> usize {{ input.len() + {i} }}\n"),
            1 => format!("    let value_{i} = compute({}, \"{}\");\n", dice.below(1000), dice.pick(&WORDS)),
            2 => format!("    // {}\n", sentence(dice)),
            _ => format!("    if value > {} {{ return Err(Error::TooBig); }}\n", dice.below(100)),
        };
        text.push_str(&line);
    }
    text.push_str("```\n\n");
    text
}

fn table(dice: &mut Dice, rows: usize) -> String {
    let mut text = String::from("| name | state | notes |\n|---|---|---|\n");
    for i in 0..rows {
        text.push_str(&format!("| item-{i} | {} | {} |\n", dice.pick(&["open", "done", "stuck"]), sentence(dice)));
    }
    text.push('\n');
    text
}

/// Mixed Markdown of about `size` bytes.
fn document(dice: &mut Dice, size: usize) -> String {
    let mut text = String::new();
    while text.len() < size {
        match dice.below(8) {
            0 => text.push_str(&format!("## {}\n\n", sentence(dice))),
            1 => {
                let lines = 8 + dice.below(30);
                text.push_str(&code(dice, lines));
            }
            2 => text.push_str(&table(dice, 4)),
            3 => {
                for _ in 0..4 {
                    text.push_str(&format!("- {}\n", sentence(dice)));
                }
                text.push('\n');
            }
            _ => text.push_str(&format!("{}\n\n", paragraph(dice))),
        }
    }
    text
}

fn agent(text: &str) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } })
}

fn user(text: &str) -> Value {
    json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": text } })
}

/// `text` as chunks of about `size` bytes, each `gap` ms after the last.
fn streamed(text: &str, size: usize, gap: u64, start: u64) -> Vec<Said> {
    let mut said = Vec::new();
    let (mut from, mut ms) = (0, start);
    while from < text.len() {
        let mut to = (from + size).min(text.len());
        while !text.is_char_boundary(to) {
            to += 1;
        }
        said.push(Said { ms, update: agent(&text[from..to]) });
        from = to;
        ms += gap;
    }
    said
}

/// A step with `output`, in pieces of `piece` bytes.
fn tool(id: &str, output: &str, piece: usize, gap: u64, start: u64) -> Vec<Said> {
    let call = json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "Run cargo test", "kind": "execute", "status": "in_progress" });
    let mut said = vec![Said { ms: start, update: call }];
    let mut ms = start;
    let mut shown = String::new();
    let mut rest = output;
    while !rest.is_empty() {
        let mut to = piece.min(rest.len());
        while !rest.is_char_boundary(to) {
            to += 1;
        }
        shown.push_str(&rest[..to]);
        rest = &rest[to..];
        ms += gap;
        // ACP has a step's content whole in each update, as rein sends it.
        let keep = shown.len().saturating_sub(16_384);
        let start_at = (keep..shown.len()).find(|at| shown.is_char_boundary(*at)).unwrap_or(0);
        let content = [json!({ "type": "content", "content": { "type": "text", "text": &shown[start_at..] } })];
        said.push(Said { ms, update: json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress", "content": content }) });
    }
    said.push(Said { ms: ms + gap, update: json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed" }) });
    said
}

fn history(dice: &mut Dice, messages: usize) -> Vec<Said> {
    let mut said = Vec::with_capacity(messages);
    for i in 0..messages {
        let update = if i % 2 == 0 {
            user(&sentence(dice))
        } else {
            let size = 200 + dice.below(4000);
            agent(&document(dice, size))
        };
        said.push(Said { ms: 0, update });
    }
    said
}

/// The workload `name`, or `None` if there's no such one.
fn workload(name: &str) -> Option<Vec<Said>> {
    let mut dice = Dice(0x9E37_79B9_7F4A_7C15);
    Some(match name {
        // 100 chunks a second of about 4 bytes: faster than a model streams.
        "long" => streamed(&document(&mut dice, 200_000), 4, 10, 0),
        "code" => streamed(&code(&mut dice, 20_000), 8, 10, 0),
        // A row at a time: each new row can change every row before it.
        "table" => streamed(&table(&mut dice, 500), 16, 10, 0),
        "burst" => streamed(&document(&mut dice, 500_000), 10_000, 100, 0),
        "tiny" => streamed(&document(&mut dice, 30_000), 1, 1, 0),
        "tool-output" => {
            let output: String = (0..60_000).map(|i| format!("test step_{i} ... ok\n")).collect();
            tool("t1", &output, 4_096, 10, 0)
        }
        "history" => history(&mut dice, 10_000),
        // Scrolled back through what was said, as an answer comes in below.
        "reading" => {
            let mut said = history(&mut dice, 2_000);
            said.extend(streamed(&document(&mut dice, 200_000), 4, 10, 500));
            said
        }
        _ => return None,
    })
}

pub(crate) fn run(name: &str, out: &Path) -> Result<(), String> {
    let said = workload(name).ok_or_else(|| format!("no workload {name}: try {}", NAMES.join(", ")))?;
    crate::write(out, &said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workload_is_the_same_every_time_and_keeps_what_it_says_whole() {
        for name in NAMES {
            let (one, two) = (workload(name).unwrap(), workload(name).unwrap());
            assert!(!one.is_empty(), "{name}");
            assert_eq!(one, two, "{name}");
            assert!(one.windows(2).all(|pair| pair[0].ms <= pair[1].ms), "{name} goes back in time");
        }
        assert!(workload("no such one").is_none());
        let joined: String = streamed("héllo wörld", 2, 1, 0).iter().map(|one| one.update["content"]["text"].as_str().unwrap().to_owned()).collect();
        assert_eq!(joined, "héllo wörld", "a chunk ends between characters");
    }

    #[test]
    fn a_recording_reads_back_as_it_was_written() {
        let path = std::env::temp_dir().join(format!("rein-replay-{}.jsonl", std::process::id()));
        let said = streamed("hello there", 4, 10, 5);
        crate::write(&path, &said).unwrap();
        assert_eq!(crate::read(&path).unwrap(), said);
        std::fs::remove_file(path).unwrap();
    }
}
