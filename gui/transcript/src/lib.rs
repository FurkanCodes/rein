//! A conversation as a window holds it: what `session/update`s come to,
//! with no toolkit in sight, so it outlives the choice of one.

mod asking;
mod blocks;
pub mod boards;
pub mod changes;
pub mod composing;
pub mod config;
// One rule for finding a path, wherever it's typed.
#[path = "../../../crates/rein-cli/src/ui/fuzzy.rs"]
pub mod fuzzy;
// One rule for narrowing a list, wherever it's drawn.
#[allow(dead_code)]
#[path = "../../../crates/rein-cli/src/ui/narrow.rs"]
mod narrow;
mod question;
mod saved;
mod settings;
pub mod signing;
mod watch;

use std::collections::HashMap;
use std::ops::Range;

pub use asking::{Asking, Command, Item, Menu, Offered, OwnCommand, Sets, says_back};
pub use blocks::Blocks;
pub use question::{Answering, ChangedLine, Choice, FileChange, Question, Words};
pub use saved::{Saved, folder_name};
use serde_json::Value;
pub use settings::{Choosable, Meter, Setting};
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
    /// What a command you ran said, laid out as it was: not the model's.
    Said,
    /// The line that ends a turn: what it came to.
    Turn,
}

/// Starts what a command said: it's laid out for a terminal, in columns and
/// lines, so it's kept as code. Longer than any fence in what it holds.
const SAID_OPENS: &str = "`````\n";
/// Marks text as a command's, not the model's.
const SAID: &str = "io.github.furkancodes/said";
/// Marks a message of yours as whole: chunks of one role run together
/// otherwise, and two commands in a row read `/model/effort`.
const WHOLE: &str = "io.github.furkancodes/whole";
/// Marks a thought as rein's own note: a check, a hook, a command ending.
const NOTE: &str = "io.github.furkancodes/note";
/// Marks a note as the line that ends a turn, and the turn to undo from it.
const TURN: &str = "io.github.furkancodes/turn";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    /// Markdown, but for a step: its title, then what it printed.
    pub text: String,
    /// How a step stands: `in_progress`, `completed`, `failed`.
    pub status: Option<String>,
    /// What a finished step came to, in short, as rein said it.
    pub summary: Option<Summary>,
    /// The rein that said it, for another rein's words.
    pub from: Option<String>,
    /// What a check after edits came to, for a check's step.
    pub checked: Option<Checked>,
    blocks: Blocks,
}

/// What a check after edits came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub verdict: Verdict,
    pub took: std::time::Duration,
    /// How many errors there were before the turn's first change, which
    /// aren't counted against it.
    pub before: u64,
}

/// How a check after edits went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing new broke.
    Clean,
    /// This many errors the turn's changes brought in.
    New(u64),
    /// What the turn broke, it fixed.
    Fixed,
    /// It didn't run: another command had the build.
    Busy,
}

/// Where `rein serve` says it, in a check's `_meta`.
const CHECKED: &str = "io.github.furkancodes/checked";

impl Checked {
    fn of(said: &Value) -> Option<Checked> {
        let verdict = match said["verdict"].as_str()? {
            "clean" => Verdict::Clean,
            "new" => Verdict::New(said["count"].as_u64().unwrap_or(1)),
            "fixed" => Verdict::Fixed,
            "busy" => Verdict::Busy,
            _ => return None,
        };
        Some(Checked { verdict, took: std::time::Duration::from_millis(said["tookMs"].as_u64().unwrap_or(0)), before: said["before"].as_u64().unwrap_or(0) })
    }

    /// In a few words: `passed · 4.1s`.
    pub fn said(&self) -> String {
        let took = format!("{:.1}s", self.took.as_secs_f32());
        match self.verdict {
            Verdict::Clean => format!("passed · {took}"),
            Verdict::New(1) => format!("1 new error · {took}"),
            Verdict::New(count) => format!("{count} new errors · {took}"),
            Verdict::Fixed => format!("fixed what it broke · {took}"),
            Verdict::Busy => "not run: another command had the build".to_owned(),
        }
    }
}

/// What a finished step came to, in short: `exit 101`, how much an edit
/// changed, how many lines a read brought in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// rein's word for how it went, for what has no number of its own.
    pub status: String,
    /// What it said after, where there's room: a command's last line.
    pub said: String,
    pub failed: bool,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    pub lines: Option<u64>,
}

/// Where `rein serve` says it, in a step's `_meta`.
const SUMMARY: &str = "io.github.furkancodes/summary";

impl Summary {
    fn of(said: &Value) -> Option<Summary> {
        let text = |key: &str| said[key].as_str().unwrap_or_default().to_owned();
        said.is_object().then(|| Summary {
            status: text("status"),
            said: text("said"),
            failed: said["failed"] == true,
            added: said["added"].as_u64(),
            removed: said["removed"].as_u64(),
            lines: said["lines"].as_u64(),
        })
    }
}

/// Reads one after another, said on one line: `a.rs, b.rs +1`, three files,
/// so many lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reads {
    pub paths: Vec<String>,
    /// How many lines they brought in, if each one says.
    pub lines: Option<u64>,
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
    /// What it lets through, in a line.
    pub about: String,
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
        let available = listed
            .filter_map(|mode| {
                Some(Mode { id: mode["id"].as_str()?.to_owned(), name: mode["name"].as_str()?.to_owned(), about: mode["description"].as_str().unwrap_or_default().to_owned() })
            })
            .collect();
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
    commands: Vec<Command>,
    /// The model's plan, as it last said.
    plan: Vec<boards::Planned>,
    /// The turn each turn line can undo back to, by message.
    undo_points: HashMap<usize, usize>,
}

impl Transcript {
    /// The model's plan, as it last said.
    pub fn plan(&self) -> &[boards::Planned] {
        &self.plan
    }

    /// The turn message `index` can undo back to, if it's a turn's line and
    /// that turn changed files.
    pub fn undo_point(&self, index: usize) -> Option<usize> {
        self.undo_points.get(&index).copied()
    }

    /// What the check after edits last said, if it's run.
    pub fn last_check(&self) -> Option<(&str, StepState)> {
        let check = self.messages.iter().rev().find(|message| message.role == Role::Step && message.step_line().0 == "Check")?;
        Some((check.step_line().1, check.state()))
    }

    /// What the last check after edits came to, once one has finished.
    pub fn last_checked(&self) -> Option<&Checked> {
        self.messages.iter().rev().find(|message| message.role == Role::Step && message.step_line().0 == "Check")?.checked.as_ref()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Whether message `index` is a read that worked, which goes on the
    /// line of the reads before it.
    fn read_done(&self, index: usize) -> bool {
        self.messages.get(index).is_some_and(|message| message.role == Role::Step && message.step_line().0 == "Read" && message.state() == StepState::Done)
    }

    /// Whether message `index` is said on the line of a read before it.
    pub fn read_after_read(&self, index: usize) -> bool {
        self.read_done(index) && index.checked_sub(1).is_some_and(|above| self.read_done(above))
    }

    /// The reads that start at message `index`, if there's more than one.
    pub fn reads_from(&self, index: usize) -> Option<Reads> {
        if !self.read_done(index) || self.read_after_read(index) {
            return None;
        }
        let run: Vec<&Message> = (index..).take_while(|&at| self.read_done(at)).filter_map(|at| self.messages.get(at)).collect();
        (run.len() > 1).then(|| Reads {
            paths: run.iter().map(|message| message.step_line().1.to_owned()).collect(),
            lines: run.iter().map(|message| message.summary.as_ref().and_then(|summary| summary.lines)).sum(),
        })
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
        self.messages.iter().find(|message| message.role == Role::You && !says_back(&message.text))?.text.lines().find(|line| !line.trim().is_empty())
    }

    /// What can be run from the box, as the agent last said.
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// The last thing the model said.
    pub fn last_answer(&self) -> Option<&str> {
        self.messages.iter().rev().find(|message| message.role == Role::Agent).map(|message| message.text.trim())
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
        // Its name goes above it, so its words are its words alone.
        let change =
            self.add(Message { role: Role::Peer, text: text.to_owned(), status: None, summary: None, from: Some(from.to_owned()), checked: None, blocks: Blocks::default() });
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
            Some("user_message_chunk") if update["_meta"][WHOLE] == true => self.add_said(Role::You, text()),
            Some("user_message_chunk") => self.say(Role::You, text()),
            Some("agent_message_chunk") if update["_meta"][SAID] == true => self.command_said(text()),
            Some("agent_message_chunk") => self.say(Role::Agent, text()),
            Some("agent_thought_chunk") if update["_meta"][NOTE] == true => self.noted(update, text()),
            Some("agent_thought_chunk") => self.say(Role::Thought, text()),
            Some("plan") => {
                self.plan = boards::Planned::listed(update);
                Change::Nothing
            }
            Some("available_commands_update") => {
                self.commands = Command::listed(update);
                Change::Nothing
            }
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

    fn command_said(&mut self, text: &str) -> Change {
        if self.messages.last().is_some_and(|last| last.role == Role::Said) {
            return self.say(Role::Said, text);
        }
        self.say(Role::Said, &format!("{SAID_OPENS}{text}"))
    }

    /// A note of rein's own, whole; the line that ends a turn is one.
    fn noted(&mut self, update: &Value, text: &str) -> Change {
        let Some(turn) = update["_meta"].get(TURN) else { return self.add_said(Role::Note, text) };
        let change = self.add_said(Role::Turn, text);
        if let (Change::Added(index), Some(turn)) = (&change, turn.as_u64().and_then(|turn| usize::try_from(turn).ok())) {
            self.undo_points.insert(*index, turn);
        }
        change
    }

    fn add_said(&mut self, role: Role, text: &str) -> Change {
        self.add(Message { role, text: text.to_owned(), status: None, summary: None, from: None, checked: None, blocks: Blocks::default() })
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
        let change = self.add(Message { role: Role::Step, text: title.to_owned(), status, summary: None, from: None, checked: None, blocks: Blocks::default() });
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
        if let Some(summary) = Summary::of(&update["_meta"][SUMMARY]) {
            message.summary = Some(summary);
        }
        if let Some(checked) = Checked::of(&update["_meta"][CHECKED]) {
            message.checked = Some(checked);
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
    fn a_step_keeps_what_it_came_to_and_reads_in_a_row_go_on_one_line() {
        let mut transcript = Transcript::default();
        let read = |id: &str, path: &str, lines: u64| {
            vec![
                json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": format!("Read {path}"), "kind": "read", "status": "in_progress" }),
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed", "_meta": { SUMMARY: { "status": format!("{lines} lines"), "said": "", "lines": lines } } }),
            ]
        };
        for update in [read("a", "a.rs", 10), read("b", "b.rs", 5), read("c", "c.rs", 1)].concat() {
            transcript.apply(&update);
        }
        transcript.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "d", "title": "Run cargo test", "kind": "execute", "status": "in_progress" }));
        transcript.apply(
            &json!({ "sessionUpdate": "tool_call_update", "toolCallId": "d", "status": "failed", "_meta": { SUMMARY: { "status": "exit 101", "said": "", "failed": true } } }),
        );
        assert_eq!(transcript.reads_from(0), Some(Reads { paths: vec!["a.rs".to_owned(), "b.rs".to_owned(), "c.rs".to_owned()], lines: Some(16) }));
        assert!(!transcript.read_after_read(0) && transcript.read_after_read(1) && transcript.read_after_read(2));
        assert_eq!(transcript.reads_from(1), None, "it's on the first one's line");
        assert!(!transcript.read_after_read(3), "a command isn't a read");
        let failed = transcript.get(3).and_then(|message| message.summary.clone()).unwrap();
        assert_eq!((failed.status.as_str(), failed.failed), ("exit 101", true));
    }

    #[test]
    fn a_message_is_named_by_its_first_words_past_any_blank_lines() {
        let mut transcript = Transcript::default();
        transcript.you("\n  \npasted\nthe rest");
        assert_eq!(transcript.first_said(), Some("pasted"), "an empty name would beat the one a conversation has until then");
    }

    #[test]
    fn what_a_command_says_is_a_block_of_its_own_and_isnt_the_models() {
        let mut transcript = Transcript::default();
        let said = |text: &str| json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text }, "_meta": { SAID: true } });
        transcript.apply(&said("model   gpt-5\n"));
        transcript.apply(&said("mode    ask\n"));
        assert_eq!(transcript.messages.len(), 1, "one command, one block");
        assert_eq!(transcript.get(0).unwrap().role, Role::Said);
        assert!(transcript.row_text(0).unwrap().contains("model   gpt-5\nmode    ask"), "as it was laid out: {:?}", transcript.row_text(0));
        assert_eq!(transcript.last_answer(), None, "/copy is for what the model said");
        transcript.apply(&chunk("agent_message_chunk", "words"));
        assert_eq!(transcript.get(1).unwrap().role, Role::Agent);
    }

    #[test]
    fn two_commands_in_a_row_are_two_messages() {
        let whole = |text: &str| json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": text }, "_meta": { WHOLE: true } });
        let mut transcript = Transcript::default();
        transcript.apply(&whole("/model"));
        transcript.apply(&whole("/effort"));
        assert_eq!(transcript.messages.len(), 2, "{:?}", transcript.messages);
        assert_eq!(transcript.rows(), 2);
    }

    #[test]
    fn a_conversation_isnt_named_after_a_command() {
        let mut transcript = Transcript::default();
        transcript.apply(&chunk("user_message_chunk", "/status"));
        assert_eq!(transcript.first_said(), None);
        transcript.you("fix the tests");
        assert_eq!(transcript.first_said(), Some("fix the tests"));
    }

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
        let asked = Question { title: "Run echo hi …".to_owned(), detail: Some(command.to_owned()), change: None, task: None };
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
    fn the_plan_is_what_was_last_said_of_it() {
        use boards::{PlanState, Planned};
        let mut transcript = Transcript::default();
        assert!(transcript.plan().is_empty());
        let entries = json!([{ "content": "Find it", "status": "completed" }, { "content": "Fix it", "status": "in_progress" }, { "content": "Test it", "status": "pending" }]);
        transcript.apply(&json!({ "sessionUpdate": "plan", "entries": entries }));
        let planned = |text: &str, state| Planned { text: text.to_owned(), state };
        assert_eq!(transcript.plan(), [planned("Find it", PlanState::Done), planned("Fix it", PlanState::Running), planned("Test it", PlanState::Waiting)]);
        transcript.apply(&json!({ "sessionUpdate": "plan", "entries": [{ "content": "Test it", "status": "completed" }] }));
        assert_eq!(transcript.plan(), [planned("Test it", PlanState::Done)], "a plan is said whole, so it replaces the last");
    }

    #[test]
    fn a_note_is_rein_s_own_and_a_turn_s_line_can_say_what_it_undoes() {
        let mut transcript = Transcript::default();
        let note = |text: &str, meta: Value| json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text }, "_meta": meta });
        transcript.apply(&note("compacted", json!({ NOTE: true })));
        transcript.apply(&note("1 step", json!({ NOTE: true })));
        transcript.apply(&note("2 steps · changed a.rs", json!({ NOTE: true, TURN: 7 })));
        assert_eq!(transcript.get(0).map(|message| message.role), Some(Role::Note));
        assert_eq!(transcript.len(), 3, "each is whole, not a chunk of the one before");
        assert_eq!(transcript.get(2).map(|message| message.role), Some(Role::Turn));
        assert_eq!(transcript.undo_point(2), Some(7));
        assert_eq!(transcript.undo_point(1), None);
        assert_eq!(transcript.undo_point(0), None, "a note undoes nothing");
    }

    #[test]
    fn a_check_says_what_it_came_to_and_the_errors_from_before() {
        let mut transcript = Transcript::default();
        transcript
            .apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": "check-1", "title": "Check cargo check · no new errors · 4.1s", "kind": "other", "status": "completed" }));
        assert_eq!(transcript.last_checked(), None, "an agent that doesn't say says nothing");
        transcript.apply(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "check-1", "status": "completed", "_meta": { CHECKED: { "verdict": "clean", "count": 0, "tookMs": 4100, "before": 2 } } }));
        let checked = transcript.last_checked().unwrap();
        assert_eq!((checked.said(), checked.before), ("passed · 4.1s".to_owned(), 2));
        assert_eq!(Checked::of(&json!({ "verdict": "new", "count": 3, "tookMs": 500 })).map(|checked| checked.said()), Some("3 new errors · 0.5s".to_owned()));
        assert_eq!(Checked::of(&json!({ "verdict": "maybe" })), None);
    }

    #[test]
    fn the_last_check_is_the_latest_as_it_stands() {
        let mut transcript = Transcript::default();
        assert_eq!(transcript.last_check(), None);
        let check = |id: &str, title: &str| json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": title, "kind": "execute", "status": "in_progress" });
        transcript.apply(&check("a", "Run cargo test"));
        assert_eq!(transcript.last_check(), None, "a command isn't the check");
        transcript.apply(&check("b", "Check cargo check"));
        assert_eq!(transcript.last_check(), Some(("cargo check", StepState::Running)));
        transcript.apply(&json!({ "sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "failed" }));
        assert_eq!(transcript.last_check(), Some(("cargo check", StepState::Failed)));
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
        let said = transcript.get(0).unwrap();
        assert_eq!((said.role, said.from.as_deref(), said.text.as_str()), (Role::Peer, Some("tests"), "they pass"));
    }
}
