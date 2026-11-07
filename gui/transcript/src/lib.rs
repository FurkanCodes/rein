//! A conversation as a window holds it: what `session/update`s come to,
//! with no toolkit in sight, so it outlives the choice of one.

mod blocks;

use std::collections::HashMap;
use std::ops::Range;

pub use blocks::Blocks;
use serde_json::Value;

/// Who a message is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    You,
    Agent,
    Thought,
    Step,
    /// Another rein, which is never you.
    Peer,
    /// The window's own word, which nobody in the conversation said.
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    /// Markdown, but for a step: its title, then what it printed.
    pub text: String,
    /// How a step stands: `in_progress`, `completed`, `failed`.
    pub status: Option<String>,
    blocks: Blocks,
}

/// What a window draws: one block of one message. Rows come in order, and
/// only the last few of them ever change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub message: usize,
    pub block: usize,
}

/// A place in the conversation: `offset` bytes into row `row`'s own text.
/// Selection is held as these, never as anything on screen: a row that has
/// scrolled away is gone from the screen, and a selection held there goes
/// with it. Every project that added this late kept losing rows from what
/// it copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spot {
    pub row: usize,
    pub offset: usize,
}

/// What's selected from `a` to `b`, taken as far as all of row `row`, as a
/// shift-click does. Above what's selected it keeps the far end, or what was
/// selected would be lost.
pub fn extended((a, b): (Spot, Spot), row: usize) -> (Spot, Spot) {
    let (first, last) = (a.min(b), a.max(b));
    if row < first.row { (Spot { row, offset: 0 }, last) } else { (first, Spot { row, offset: usize::MAX }) }
}

/// What an update did to the rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Update {
    /// How many new rows went on the end.
    pub added: usize,
    /// Rows that were there already and aren't what they were.
    pub touched: Range<usize>,
}

/// What a step would do, put to you as a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub title: String,
    /// All of what it would run or write, where the title shows less: a
    /// title has one line of a command, and a yes goes to the whole of it.
    pub detail: Option<String>,
}

impl Question {
    /// Read from a `tool_call`. One with no title doesn't say what it is.
    fn of(update: &Value) -> Option<Question> {
        let title = update["title"].as_str()?.to_owned();
        let input = &update["rawInput"];
        // Only a command step is all in its command: any other tool's
        // arguments are what you'd be saying yes to, each of them.
        let command = if update["kind"] == "execute" { input["command"].as_str() } else { None };
        let whole = (!input.is_null()).then(|| command.or(input.as_str()).map_or_else(|| pretty(input), str::to_owned));
        let detail = whole.filter(|whole| !title.contains(whole.as_str()));
        Some(Question { title, detail })
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Where a step is, and what it would be asked as.
#[derive(Debug)]
struct Step {
    index: usize,
    question: Option<Question>,
}

/// What an update did to the messages.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    Nothing,
    /// A new message, at this index.
    Added(usize),
    /// The message at `index` grew by `added`, a range of its text.
    Grew {
        index: usize,
        added: Range<usize>,
    },
    /// The message at this index isn't what it was.
    Changed(usize),
}

/// The most of a step's output a window holds: rows can't show more, and
/// a command can print gigabytes.
const STEP_OUTPUT: usize = 16_384;

#[derive(Debug, Default)]
pub struct Transcript {
    messages: Vec<Message>,
    /// Each step, by its id.
    steps: HashMap<String, Step>,
    rows: Vec<Row>,
    /// Each message's first row, while it might still grow: the last's.
    last_first_row: usize,
    /// Each step's row.
    step_rows: HashMap<usize, usize>,
}

impl Transcript {
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Message> {
        self.messages.get(index)
    }

    /// What step `id` would do, if the conversation has shown it. `None`
    /// is a question nobody can answer yes to.
    pub fn question(&self, id: &str) -> Option<&Question> {
        self.steps.get(id)?.question.as_ref()
    }

    /// How many rows there are.
    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    pub fn row(&self, index: usize) -> Option<Row> {
        self.rows.get(index).copied()
    }

    /// Row `index` as Markdown of its own.
    pub fn row_text(&self, index: usize) -> Option<std::borrow::Cow<'_, str>> {
        let Row { message, block } = self.row(index)?;
        let message = self.messages.get(message)?;
        if message.role == Role::Step {
            return Some(message.text.as_str().into());
        }
        message.blocks.row(block, &message.text)
    }

    /// The place `shown` bytes into what row `row` shows. Where that isn't in
    /// what was said, `otherwise` bytes in.
    pub fn spot(&self, row: usize, shown: usize, otherwise: usize) -> Spot {
        let offset = self.row(row).and_then(|at| {
            let message = self.messages.get(at.message)?;
            message.blocks.in_source(at.block, &message.text, shown)
        });
        Spot { row, offset: offset.unwrap_or(otherwise) }
    }

    /// The bytes of what was said that row `index` covers.
    fn said(&self, index: usize) -> Option<&str> {
        let Row { message, block } = self.row(index)?;
        let message = self.messages.get(message)?;
        if message.role == Role::Step {
            return Some(&message.text);
        }
        message.blocks.source(block, &message.text).map(|range| &message.text[range])
    }

    /// What was said from `a` to `b`, whichever comes first. It's read from
    /// the conversation, so rows nobody has drawn are in it, and what's
    /// added to show a row isn't.
    pub fn between(&self, a: Spot, b: Spot) -> String {
        let (from, to) = (a.min(b), a.max(b));
        let mut copied = String::new();
        for index in from.row..=to.row {
            let Some(text) = self.said(index) else { continue };
            let floor = |mut at: usize| {
                at = at.min(text.len());
                while !text.is_char_boundary(at) {
                    at -= 1;
                }
                at
            };
            let start = if index == from.row { floor(from.offset) } else { 0 };
            let end = if index == to.row { floor(to.offset) } else { text.len() };
            copied.push_str(&text[start..end.max(start)]);
            // A message is a paragraph of its own in what's pasted.
            let next = self.row(index + 1).map(|row| row.message);
            if index < to.row && next.is_some() && next != self.row(index).map(|row| row.message) && !copied.ends_with("\n\n") {
                copied.push_str(if copied.ends_with('\n') { "\n" } else { "\n\n" });
            }
        }
        copied
    }

    /// Takes in one `session/update`'s `update`.
    pub fn apply(&mut self, update: &Value) -> Update {
        let change = self.change(update);
        self.rows_for(&change)
    }

    /// You wrote `text`, as a message of its own: an agent says only what
    /// reaches a running turn back.
    pub fn you(&mut self, text: &str) -> Update {
        let change = self.add_said(Role::You, text);
        self.rows_for(&change)
    }

    /// The window has something to say, like why a turn failed.
    pub fn note(&mut self, text: &str) -> Update {
        let change = self.add_said(Role::Note, text);
        self.rows_for(&change)
    }

    /// Another rein said `text`, which goes in as its own.
    pub fn heard(&mut self, from: &str, text: &str) -> Update {
        let change = self.add_said(Role::Peer, &format!("**{from}**: {text}"));
        self.rows_for(&change)
    }

    fn rows_for(&mut self, change: &Change) -> Update {
        let before = self.rows.len();
        let index = match change {
            Change::Nothing => return Update::default(),
            Change::Changed(index) => {
                let Some(&row) = self.step_rows.get(index) else { return Update::default() };
                return Update { added: 0, touched: row..row + 1 };
            }
            Change::Added(index) | Change::Grew { index, .. } => *index,
        };
        if matches!(change, Change::Added(_)) {
            self.last_first_row = before;
        }
        let message = &mut self.messages[index];
        if message.role == Role::Step {
            self.step_rows.insert(index, before);
            self.rows.push(Row { message: index, block: 0 });
            return Update { added: 1, touched: 0..0 };
        }
        let count = message.blocks.grew(&message.text);
        let first = self.last_first_row;
        // The row that was growing, and any that it has ended up as.
        let was_tail = before.saturating_sub(1).max(first);
        self.rows.extend((before - first..count).map(|block| Row { message: index, block }));
        Update { added: self.rows.len() - before, touched: was_tail.min(before)..before }
    }

    fn change(&mut self, update: &Value) -> Change {
        let text = || update["content"]["text"].as_str().unwrap_or_default();
        match update["sessionUpdate"].as_str() {
            Some("user_message_chunk") => self.say(Role::You, text()),
            Some("agent_message_chunk") => self.say(Role::Agent, text()),
            Some("agent_thought_chunk") => self.say(Role::Thought, text()),
            Some("tool_call") => self.step(update),
            Some("tool_call_update") => self.step_update(update),
            _ => Change::Nothing,
        }
    }

    fn add_said(&mut self, role: Role, text: &str) -> Change {
        self.add(Message { role, text: text.to_owned(), status: None, blocks: Blocks::default() })
    }

    fn add(&mut self, message: Message) -> Change {
        self.messages.push(message);
        Change::Added(self.messages.len() - 1)
    }

    /// `text` goes on the last message if `role` said that too: a message
    /// comes in chunks, and a new speaker starts a new one.
    fn say(&mut self, role: Role, text: &str) -> Change {
        if text.is_empty() {
            return Change::Nothing;
        }
        let index = self.messages.len().wrapping_sub(1);
        if let Some(last) = self.messages.last_mut().filter(|last| last.role == role) {
            let from = last.text.len();
            last.text.push_str(text);
            return Change::Grew { index, added: from..last.text.len() };
        }
        self.add_said(role, text)
    }

    fn step(&mut self, update: &Value) -> Change {
        let Some(id) = update["toolCallId"].as_str() else { return Change::Nothing };
        let title = update["title"].as_str().unwrap_or("a step");
        let status = update["status"].as_str().map(str::to_owned);
        let change = self.add(Message { role: Role::Step, text: title.to_owned(), status, blocks: Blocks::default() });
        if let Change::Added(index) = change {
            self.steps.insert(id.to_owned(), Step { index, question: Question::of(update) });
        }
        change
    }

    fn step_update(&mut self, update: &Value) -> Change {
        let Some(index) = update["toolCallId"].as_str().and_then(|id| self.steps.get(id)).map(|step| step.index) else { return Change::Nothing };
        let message = &mut self.messages[index];
        if let Some(status) = update["status"].as_str() {
            message.status = Some(status.to_owned());
        }
        if let Some(shown) = update["content"][0]["content"]["text"].as_str() {
            let title = message.text.lines().next().unwrap_or_default().to_owned();
            let from = (shown.len().saturating_sub(STEP_OUTPUT)..shown.len()).find(|at| shown.is_char_boundary(*at)).unwrap_or(0);
            message.text = format!("{title}\n{}", &shown[from..]);
        }
        Change::Changed(index)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn chunk(kind: &str, text: &str) -> Value {
        json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } })
    }

    #[test]
    fn a_message_comes_in_chunks_and_a_new_speaker_starts_a_new_one() {
        let mut transcript = Transcript::default();
        assert_eq!(transcript.change(&chunk("user_message_chunk", "hi")), Change::Added(0));
        assert_eq!(transcript.change(&chunk("agent_message_chunk", "hel")), Change::Added(1));
        assert_eq!(transcript.change(&chunk("agent_message_chunk", "lo")), Change::Grew { index: 1, added: 3..5 });
        assert_eq!(transcript.get(1).unwrap().text, "hello");
        assert_eq!(transcript.change(&chunk("agent_thought_chunk", "hm")), Change::Added(2));
        assert_eq!(transcript.change(&chunk("agent_message_chunk", "")), Change::Nothing);
        assert_eq!(transcript.change(&json!({ "sessionUpdate": "usage_update" })), Change::Nothing);
    }

    #[test]
    fn a_step_is_updated_where_it_is_and_keeps_only_the_end_of_what_it_printed() {
        let mut transcript = Transcript::default();
        transcript.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "a", "title": "Run ls", "status": "in_progress" }));
        transcript.apply(&chunk("agent_message_chunk", "and now"));
        let shown = "é".repeat(STEP_OUTPUT);
        let update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": shown } }] });
        assert_eq!(transcript.change(&update), Change::Changed(0));
        let step = transcript.get(0).unwrap();
        assert_eq!(step.status.as_deref(), Some("completed"));
        assert!(step.text.starts_with("Run ls\n") && step.text.len() <= STEP_OUTPUT + 8, "{}", step.text.len());
        let unknown = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "nope", "status": "completed" });
        assert_eq!(transcript.change(&unknown), Change::Nothing);
    }

    #[test]
    fn what_was_said_is_copied_across_rows_and_messages_whatever_was_drawn() {
        let mut transcript = Transcript::default();
        transcript.apply(&chunk("user_message_chunk", "héllo there"));
        let code: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let said = format!("First block.\n\n```rust\n{code}```\nLast.\n");
        transcript.apply(&chunk("agent_message_chunk", &said));
        assert!(transcript.rows() > 3, "{}", transcript.rows());
        let last = transcript.rows() - 1;
        let copied = transcript.between(Spot { row: 0, offset: 7 }, Spot { row: last, offset: 4 });
        assert_eq!(copied, format!("there\n\nFirst block.\n\n```rust\n{code}```\nLast"), "the fences put in to show a piece aren't in it");
        assert_eq!(copied, transcript.between(Spot { row: last, offset: 4 }, Spot { row: 0, offset: 7 }), "dragged upward, the same");
        assert_eq!(transcript.between(Spot { row: 0, offset: 2 }, Spot { row: 0, offset: 3 }), "é", "a spot inside a character falls back to its start");
        let everything = transcript.between(Spot { row: 0, offset: 0 }, Spot { row: 999, offset: 0 });
        assert!(everything.starts_with("héllo there") && everything.ends_with("Last.\n"), "past the end is the end: {everything}");
    }

    #[test]
    fn rows_grow_at_the_end_and_only_the_last_ones_change() {
        let mut transcript = Transcript::default();
        assert_eq!(transcript.apply(&chunk("user_message_chunk", "hi")), Update { added: 1, touched: 0..0 });
        assert!(transcript.apply(&chunk("agent_message_chunk", "One.")).touched.is_empty(), "a new row isn't one that changed");
        assert_eq!(transcript.apply(&chunk("agent_message_chunk", " Still one.")), Update { added: 0, touched: 1..2 });
        assert_eq!(transcript.apply(&chunk("agent_message_chunk", "\n\nTwo")), Update { added: 1, touched: 1..2 }, "the first has ended, and a second has begun");
        assert_eq!(transcript.apply(&chunk("agent_message_chunk", ".")), Update { added: 0, touched: 2..3 }, "only the last is growing");
        assert_eq!(transcript.row(2), Some(Row { message: 1, block: 1 }));
        assert_eq!(transcript.row_text(1).unwrap(), "One. Still one.\n\n");
        assert_eq!(transcript.row_text(2).unwrap(), "Two.");
        assert_eq!(transcript.row_text(3), None);
        transcript.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "a", "title": "Run ls", "status": "in_progress" }));
        transcript.apply(&chunk("agent_message_chunk", "After."));
        let update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed" });
        assert_eq!(transcript.apply(&update), Update { added: 0, touched: 3..4 }, "a step changes where it is");
    }

    #[test]
    fn what_youre_asked_is_all_of_what_a_step_would_do() {
        let mut transcript = Transcript::default();
        let step = |id: &str, title: &str, input: Value| json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute", "status": "pending", "rawInput": input });
        let command = "echo hi\nrm -rf ~/work";
        transcript.apply(&step("long", "Run echo hi …", json!({ "command": command })));
        let asked = Question { title: "Run echo hi …".to_owned(), detail: Some(command.to_owned()) };
        assert_eq!(transcript.question("long"), Some(&asked), "a title has only the first line: the rest is what you'd be saying yes to");

        let printed = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "long", "status": "in_progress", "content": [{ "type": "content", "content": { "type": "text", "text": "hi" } }] });
        transcript.apply(&printed);
        assert_eq!(transcript.question("long"), Some(&asked), "what it printed doesn't change what was asked");

        transcript.apply(&step("short", "Run ls", json!({ "command": "ls" })));
        assert_eq!(transcript.question("short").unwrap().detail, None, "the title is all of it");
        transcript.apply(&step("write", "Write notes.md", json!({ "path": "notes.md", "content": "the words" })));
        assert!(transcript.question("write").unwrap().detail.as_ref().unwrap().contains("the words"), "what a write would write is shown, not only where");
        transcript.apply(&step("broken", "Run ", json!("not json at all")));
        assert_eq!(transcript.question("broken").unwrap().detail.as_deref(), Some("not json at all"), "arguments that can't be read are shown as they came");
        let another_tools = json!({ "sessionUpdate": "tool_call", "toolCallId": "mcp", "title": "MCP deploy", "kind": "other", "rawInput": { "command": "deploy", "target": "prod", "force": true } });
        transcript.apply(&another_tools);
        let detail = transcript.question("mcp").unwrap().detail.clone().unwrap();
        assert!(detail.contains("prod") && detail.contains("force"), "only a command step is all in its command: {detail}");
        transcript.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "plain", "title": "Peers the other reins" }));
        assert_eq!(transcript.question("plain").unwrap().detail, None);

        assert_eq!(transcript.question("nope"), None, "a step never shown can't be put as a question");
        transcript.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "untitled", "rawInput": { "command": "ls" } }));
        assert_eq!(transcript.question("untitled"), None, "nor one that doesn't say what it is");
    }

    #[test]
    fn what_you_wrote_and_what_the_window_says_go_in_as_their_own() {
        let mut transcript = Transcript::default();
        assert_eq!(transcript.you("hi"), Update { added: 1, touched: 0..0 });
        transcript.you("again");
        assert_eq!(transcript.len(), 2, "two prompts aren't one message");
        assert_eq!(transcript.get(1).unwrap().role, Role::You);
        let note = transcript.note("That didn't work.");
        assert!(note.added == 1 && note.touched.is_empty(), "{note:?}");
        assert_eq!(transcript.get(2).unwrap().role, Role::Note);
        transcript.apply(&chunk("agent_message_chunk", "hello"));
        assert_eq!(transcript.get(3).unwrap().role, Role::Agent, "and what comes next starts anew");
    }

    #[test]
    fn a_shift_click_takes_a_selection_to_the_row_clicked_either_way() {
        let (first, last) = (Spot { row: 50, offset: 7 }, Spot { row: 60, offset: 9 });
        let upward = (Spot { row: 10, offset: 0 }, last);
        assert_eq!(extended((first, last), 10), upward, "all of the row clicked, and what was selected stays");
        assert_eq!(extended((last, first), 10), upward, "however it was dragged");
        assert_eq!(extended((first, last), 70), (first, Spot { row: 70, offset: usize::MAX }));
    }

    #[test]
    fn a_place_shown_is_held_as_a_place_in_what_was_said() {
        let mut transcript = Transcript::default();
        let lines: String = (0..100).map(|i| format!("line {i}\n")).collect();
        transcript.apply(&chunk("agent_message_chunk", &format!("```rust\n{lines}```\n")));
        let shown = transcript.row_text(1).unwrap().find("line 70").unwrap();
        let (from, to) = (transcript.spot(1, shown, 0), transcript.spot(1, shown + 7, usize::MAX));
        assert_eq!(transcript.between(from, to), "line 70", "copying gets what was selected, not 8 bytes on");
        let mut arriving = Transcript::default();
        arriving.apply(&chunk("agent_message_chunk", "| a |\n|---|\n| 1 |\n"));
        assert_eq!(arriving.spot(0, 5, 0).offset, 0, "a table still arriving has no such place");
    }

    #[test]
    fn another_reins_word_is_never_yours() {
        let mut transcript = Transcript::default();
        transcript.heard("tests", "they pass");
        assert_eq!(transcript.get(0).unwrap().role, Role::Peer);
    }
}
