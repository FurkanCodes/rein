//! A conversation as a window holds it: what `session/update`s come to,
//! with no toolkit in sight, so it outlives the choice of one.

mod blocks;
mod saved;
mod watch;

use std::collections::HashMap;
use std::ops::Range;

pub use blocks::Blocks;
pub use saved::{Saved, folder_name};
use serde_json::Value;
pub use watch::{Standing, Watch};

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

impl Message {
    /// How a step stands, read once from the wire's word for it.
    pub fn state(&self) -> StepState {
        StepState::of(self.status.as_deref())
    }

    /// A step's verb and what it's about: `Run`, `cargo test`.
    pub fn step_line(&self) -> (&str, &str) {
        let title = self.text.lines().next().unwrap_or_default();
        title.split_once(' ').unwrap_or((title, ""))
    }

    /// What a step printed.
    pub fn step_output(&self) -> &str {
        self.text.split_once('\n').map_or("", |(_, output)| output)
    }
}

/// How far a step reaches, which is how tall its tick stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Looked,
    Ran,
    Changed,
}

impl Reach {
    /// From ACP's word for what a call does.
    fn of(kind: Option<&str>) -> Reach {
        match kind {
            Some("execute") => Reach::Ran,
            Some("edit") => Reach::Changed,
            _ => Reach::Looked,
        }
    }
}

/// How a step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Done,
    Failed,
    /// Its turn ended before it did.
    Stopped,
}

impl StepState {
    fn of(status: Option<&str>) -> StepState {
        match status {
            Some("completed") => StepState::Done,
            Some("failed") => StepState::Failed,
            Some("stopped") => StepState::Stopped,
            _ => StepState::Running,
        }
    }
}

/// One step of a turn, as the list of agents shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub reach: Reach,
    pub state: StepState,
}

/// How much of what the model can hold the conversation takes up, as the
/// agent said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUse {
    pub used: u64,
    pub size: u64,
}

impl ContextUse {
    /// How full it is, from 0 to 1.
    pub fn share(self) -> f32 {
        if self.size == 0 { 0.0 } else { (self.used as f32 / self.size as f32).clamp(0.0, 1.0) }
    }

    /// In the bar's words: `42k/150k`.
    pub fn shown(self) -> String {
        format!("{}/{}", thousands(self.used), thousands(self.size))
    }
}

fn thousands(count: u64) -> String {
    if count < 1000 { count.to_string() } else { format!("{}k", count / 1000) }
}

/// A mode an agent offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    pub id: String,
    /// As it's shown, and any mark comes with it: `⚠ trust`.
    pub name: String,
}

/// The modes an agent offers, and the one it's in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modes {
    pub current: String,
    pub available: Vec<Mode>,
}

impl Modes {
    /// Read from what `session/new` said of them. An agent with none says
    /// nothing, and a window shows nothing.
    pub fn of(modes: &Value) -> Option<Modes> {
        let current = modes["currentModeId"].as_str()?.to_owned();
        let listed = modes["availableModes"].as_array()?.iter();
        let available = listed.filter_map(|mode| Some(Mode { id: mode["id"].as_str()?.to_owned(), name: mode["name"].as_str()?.to_owned() })).collect();
        Some(Modes { current, available })
    }

    /// What the mode it's in is called.
    pub fn current_name(&self) -> &str {
        self.available.iter().find(|mode| mode.id == self.current).map_or(self.current.as_str(), |mode| mode.name.as_str())
    }
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

/// An answer an agent offers to a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What's sent back if it's picked.
    pub id: String,
    pub name: String,
    pub kind: Answering,
}

/// What a choice says to a question: ACP's four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answering {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl Answering {
    fn of(word: &str) -> Option<Answering> {
        match word {
            "allow_once" => Some(Answering::AllowOnce),
            "allow_always" => Some(Answering::AllowAlways),
            "reject_once" => Some(Answering::RejectOnce),
            "reject_always" => Some(Answering::RejectAlways),
            _ => None,
        }
    }
}

impl Choice {
    pub const fn allows(&self) -> bool {
        matches!(self.kind, Answering::AllowOnce | Answering::AllowAlways)
    }

    /// The answers to give to a question whose `options` the agent listed.
    /// A step nobody can describe can't be allowed, however it's offered, a
    /// way of answering nobody knows isn't offered, whatever it would do, and
    /// an agent that offers no way out still gets a no.
    pub fn offered(options: &Value, known: bool) -> Vec<Choice> {
        let listed = options.as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(|option| {
            Some(Choice { id: option["optionId"].as_str()?.to_owned(), name: option["name"].as_str()?.to_owned(), kind: Answering::of(option["kind"].as_str()?)? })
        });
        let mut choices: Vec<Choice> = listed.filter(|choice| known || !choice.allows()).collect();
        if !choices.iter().any(|choice| !choice.allows()) {
            choices.push(Choice { id: "reject".to_owned(), name: "Reject".to_owned(), kind: Answering::RejectOnce });
        }
        choices
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

impl Update {
    /// What this and `other` did between them.
    pub fn and(self, other: Update) -> Update {
        let touched = match (self.touched.is_empty(), other.touched.is_empty()) {
            (true, _) => other.touched,
            (_, true) => self.touched,
            _ => self.touched.start.min(other.touched.start)..self.touched.end.max(other.touched.end),
        };
        Update { added: self.added + other.added, touched }
    }
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
    /// The steps of this turn, in order: their messages, and how far each reaches.
    turn: Vec<(usize, Reach)>,
    context: Option<ContextUse>,
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

    /// The first line of the first thing you said, which a conversation with
    /// no name goes by.
    pub fn first_said(&self) -> Option<&str> {
        self.messages.iter().find(|message| message.role == Role::You)?.text.lines().next()
    }

    /// The last `most` steps of this turn, oldest first.
    pub fn ticks(&self, most: usize) -> Vec<Tick> {
        let shown = self.turn.iter().rev().take(most).rev();
        shown.filter_map(|&(index, reach)| Some(Tick { reach, state: self.messages.get(index)?.state() })).collect()
    }

    /// How full the context is, once the agent has said.
    pub const fn context(&self) -> Option<ContextUse> {
        self.context
    }

    /// A turn nobody here prompted has started, so its steps start afresh.
    pub fn turn_started(&mut self) {
        self.turn.clear();
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

    /// A turn is over, so no step of it is still going. An agent says
    /// nothing of one a stop cut short, and it would read as pending for
    /// good.
    pub fn turn_over(&mut self) -> Update {
        let mut touched = Update::default();
        for step in self.steps.values() {
            let Some(message) = self.messages.get_mut(step.index) else { continue };
            if matches!(message.status.as_deref(), Some("pending" | "in_progress")) {
                message.status = Some("stopped".to_owned());
                if let Some(&row) = self.step_rows.get(&step.index) {
                    touched = touched.and(Update { added: 0, touched: row..row + 1 });
                }
            }
        }
        touched
    }

    /// You wrote `text`, as a message of its own: an agent says only what
    /// reaches a running turn back.
    pub fn you(&mut self, text: &str) -> Update {
        self.turn.clear();
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
            Some("usage_update") => {
                if let (Some(used), Some(size)) = (update["used"].as_u64(), update["size"].as_u64()) {
                    self.context = Some(ContextUse { used, size });
                }
                Change::Nothing
            }
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
            self.turn.push((index, Reach::of(update["kind"].as_str())));
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

    #[test]
    fn a_way_of_answering_nobody_knows_isnt_offered() {
        let options = json!([{ "optionId": "ok", "name": "Allow", "kind": "allow_once" }, { "optionId": "odd", "name": "Sure", "kind": "allow_forever" }, { "optionId": "odder", "name": "Hm", "kind": "maybe" }]);
        let offered = Choice::offered(&options, true);
        let ids: Vec<&str> = offered.iter().map(|choice| choice.id.as_str()).collect();
        assert_eq!(ids, ["ok", "reject"], "a yes under a name rein can't read would be drawn as anything, and there's always a no");
    }

    /// A step of `kind`, as an agent announces it.
    fn step(id: &str, kind: &str, status: &str) -> Value {
        json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "Run cargo test", "kind": kind, "status": status })
    }

    #[test]
    fn the_strip_holds_this_turns_steps_as_they_stand_and_a_new_turn_starts_it_afresh() {
        let mut transcript = Transcript::default();
        transcript.apply(&step("a", "read", "completed"));
        transcript.apply(&step("b", "execute", "in_progress"));
        transcript.apply(&step("c", "edit", "completed"));
        transcript.apply(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "failed" }));
        let tick = |reach, state| Tick { reach, state };
        assert_eq!(transcript.ticks(8), [tick(Reach::Looked, StepState::Done), tick(Reach::Ran, StepState::Failed), tick(Reach::Changed, StepState::Done)]);
        assert_eq!(transcript.ticks(1), [tick(Reach::Changed, StepState::Done)], "the latest, as the strip ends with them");
        transcript.you("and now");
        assert!(transcript.ticks(8).is_empty());
        transcript.apply(&step("d", "search", "pending"));
        assert_eq!(transcript.ticks(8), [tick(Reach::Looked, StepState::Running)], "a step that hasn't ended is running");
    }

    #[test]
    fn a_step_says_its_verb_and_what_its_about() {
        let mut transcript = Transcript::default();
        transcript.apply(&step("a", "execute", "completed"));
        transcript.apply(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "content": [{ "content": { "text": "ok\ndone" } }] }));
        let message = transcript.get(0).unwrap();
        assert_eq!(message.step_line(), ("Run", "cargo test"));
        assert_eq!(message.step_output(), "ok\ndone");
    }

    #[test]
    fn the_context_is_what_the_agent_last_said_and_stays_in_bounds() {
        let mut transcript = Transcript::default();
        assert_eq!(transcript.context(), None, "a window shows nothing until it's said");
        transcript.apply(&json!({ "sessionUpdate": "usage_update", "used": 42_100, "size": 150_000 }));
        let context = transcript.context().unwrap();
        assert_eq!(context.shown(), "42k/150k");
        assert!((context.share() - 0.28).abs() < 0.01);
        assert_eq!(ContextUse { used: 900, size: 0 }.share(), 0.0, "a size of nothing is no division by it");
        assert_eq!(ContextUse { used: 900, size: 800 }.share(), 1.0);
        assert_eq!(ContextUse { used: 900, size: 800 }.shown(), "900/800");
    }

    #[test]
    fn modes_are_read_with_their_marks_and_no_list_is_no_modes() {
        let said = json!({ "currentModeId": "trust", "availableModes": [{ "id": "ask", "name": "ask" }, { "id": "trust", "name": "⚠ trust" }, { "name": "no id" }] });
        let modes = Modes::of(&said).unwrap();
        assert_eq!(modes.available.len(), 2, "one that can't be chosen isn't offered");
        assert_eq!(modes.current_name(), "⚠ trust");
        assert_eq!(Modes::of(&json!({})), None);
    }

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
    fn the_answers_to_a_question_are_the_agents_own_but_a_step_nobody_can_describe_cant_be_allowed() {
        let options = json!([
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "always", "name": "Allow for this conversation", "kind": "allow_always" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
        ]);
        let ids = |choices: Vec<Choice>| choices.into_iter().map(|choice| choice.id).collect::<Vec<_>>();
        assert_eq!(ids(Choice::offered(&options, true)), ["allow", "always", "reject"]);
        assert_eq!(ids(Choice::offered(&options, false)), ["reject"]);
        assert_eq!(ids(Choice::offered(&json!([]), false)), ["reject"], "there's always a no");
        assert_eq!(ids(Choice::offered(&Value::Null, true)), ["reject"]);
        let no_no = json!([{ "optionId": "ok", "name": "OK", "kind": "allow_once" }]);
        assert_eq!(ids(Choice::offered(&no_no, true)), ["ok", "reject"]);
        assert_eq!(ids(Choice::offered(&no_no, false)), ["reject"]);
    }

    #[test]
    fn a_step_a_turn_left_unfinished_is_said_to_have_stopped() {
        let mut transcript = Transcript::default();
        let step = |id: &str, status: &str| json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": format!("Run {id}"), "status": status });
        transcript.apply(&step("done", "completed"));
        transcript.apply(&step("waiting", "pending"));
        transcript.apply(&chunk("agent_message_chunk", "between"));
        transcript.apply(&step("going", "in_progress"));
        let status = |transcript: &Transcript, index: usize| transcript.get(index).unwrap().status.clone();
        assert_eq!(transcript.turn_over(), Update { added: 0, touched: 1..4 }, "from the first it changed to the last");
        assert_eq!(status(&transcript, 0).as_deref(), Some("completed"), "what finished stays finished");
        assert_eq!(status(&transcript, 1).as_deref(), Some("stopped"));
        assert_eq!(status(&transcript, 3).as_deref(), Some("stopped"));
        assert_eq!(transcript.turn_over(), Update::default(), "nothing's left to stop");
    }

    #[test]
    fn what_two_updates_did_comes_to_one() {
        let (a, b) = (Update { added: 1, touched: 2..3 }, Update { added: 2, touched: 7..9 });
        assert_eq!(a.clone().and(b), Update { added: 3, touched: 2..9 });
        assert_eq!(a.clone().and(Update::default()), a, "an empty range isn't anywhere");
        assert_eq!(Update::default().and(a.clone()), a);
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
