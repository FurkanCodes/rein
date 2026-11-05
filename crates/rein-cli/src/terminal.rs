//! The terminal rein talks through: what it shows of a turn as it
//! happens, and how it asks, as the agent's [`Ui`].

use crate::format::thousands;
use crate::input::{Input, Question, Read};
use crate::peers::node::Incoming;
use crate::peers::{Node, State};
use crate::ui::history::History;
use crate::ui::markdown::Markdown;
use crate::ui::screen::Progress;
use crate::ui::spinner::Spinner;
use crate::ui::steps;
use crate::ui::style::{GUTTER, Style, strip};
use crate::ui::wrap::Wrap;
use rein_agent::agent::{Approval, Compacted, Heard, Ui};
use rein_agent::compact;
use rein_agent::permissions::{Mode, Rule, Whose};
use rein_agent::sandbox::{Access, Sandbox};
use rein_agent::tools::{BashCall, ToolName};
use rein_agent::undo::Watched;
use rein_llm::{Effort, ToolCall, Usage};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

/// The width of the terminal showing rein's answers, if stdout is one.
pub(crate) fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a `winsize` into the struct we pass.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// The terminal as rein's [`Ui`]: answers on stdout, steps, questions and
/// the status bar on stderr. Each step sits under a label in a column:
///
/// ```text
/// you    fix the trailing comma bug
///
/// rein   ● Read   src/parser.rs, lines 190-240
///        ● Run    cargo test  exit 101
///          └ test parser::trailing_comma ... FAILED
///
///        Found it: …
/// ```
///
/// When stdout or stderr isn't a terminal, that stream gets plain text.
/// What the terminal showed last, of what decides the spacing: an answer
/// and a step have a blank line between them, and so has your message and
/// whatever comes after it (which waits until then, so the spinner's own gap
/// isn't doubled).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Last {
    #[default]
    Nothing,
    You,
    Step,
    Text,
}

pub(crate) struct Terminal {
    /// Colours for stdout and stderr.
    out: Style,
    pub(crate) err: Style,
    /// stderr is a terminal: spinners, labels and the status bar.
    pub(crate) fancy: bool,
    /// The project folder, for line numbers in file changes.
    root: PathBuf,
    pub(crate) model: String,
    pub(crate) effort: Option<Effort>,
    pub(crate) mode: Mode,
    /// Commands run in a sandbox: what one asks for past it is what's asked about.
    pub(crate) sandboxed: bool,
    pub(crate) input: Input,
    /// Ctrl-C pressed at a question; the turn waits on this to stop.
    pub(crate) interrupted: std::sync::Arc<tokio::sync::Notify>,
    /// When the conversation gets compacted, in tokens.
    pub(crate) context_limit: u64,
    /// How many tokens the conversation took at the last count.
    pub(crate) context_used: u64,
    /// How much of the last finished turn came from the prompt cache, in percent.
    pub(crate) last_cache: Option<u64>,
    /// The answer being written, if one is.
    answer: Option<Answer>,
    mid_line: bool,
    /// Without the box, what rein is doing spins on a line of its own.
    spinner: Option<Spinner>,
    /// The step on screen, between its question and its result.
    step: Option<Step>,
    /// A note to show under the step once its line is up, like that it
    /// won't be asked about again.
    noted: Option<String>,
    /// What the step did to the project's files, to show under it.
    watched: Option<String>,
    /// What your hooks did to the step on its way, said under its line.
    hooked: Vec<String>,
    /// The images pasted with Ctrl-V into the message being typed, each by
    /// its label in the box.
    pub(crate) pasted: Vec<(String, rein_llm::Image)>,
    /// What was shown last, which says whether what comes next starts
    /// after a blank line.
    pub(crate) last: Last,
    /// rein's label has been shown this turn; what follows lines up under it.
    labeled: bool,
    /// What the model says it was thinking about, as it streams in, and
    /// how many of its headlines are on screen.
    thoughts: String,
    thoughts_shown: usize,
    /// The plan's steps as last shown in full: an update that only moves
    /// along it gets one line.
    plan: Vec<String>,
    /// The model is writing a summary, not an answer.
    compacting: bool,
    /// It's been said that requests go uncompressed.
    pub(crate) said_uncompressed: bool,
    /// The reads on screen as one line, which the next joins if nothing
    /// came between.
    reads: Option<Reads>,
    /// The key the next run of reads is shown under.
    next_key: u64,
    pub(crate) turn: TurnStats,
    /// This rein among the others on this computer, when it joined them.
    pub(crate) peers: Option<Node>,
    /// Tasks other reins handed over in this turn: where each one's
    /// result goes, and its ID.
    pub(crate) tasks: Vec<(String, String)>,
    /// The files this turn changed, for a task's result.
    pub(crate) changed: Vec<String>,
    /// What came in while no turn ran that isn't a message: notes of files
    /// other reins changed, for the next turn.
    pub(crate) notes: Vec<Heard>,
    /// The rein that started this one is done with it: it goes once it's
    /// idle, unless it's been adopted.
    pub(crate) closing: bool,
    /// You've typed into this sub-agent: it's yours now, and stays.
    pub(crate) adopted: bool,
    /// You said always to opening tabs for sub-agents.
    pub(crate) spawn_always: bool,
    /// It's been said how Terminal can open tabs, not windows.
    pub(crate) said_tabs: bool,
    /// The model in use, as `provider/model`: what a sub-agent gets unless
    /// it's given another.
    pub(crate) spec: String,
    /// The `tab_command` setting: how to open a tab here, if rein doesn't know.
    pub(crate) tab_command: Option<String>,
}

impl Drop for Terminal {
    /// The tab's bar goes with rein.
    fn drop(&mut self) {
        crate::ui::screen::progress(Progress::Off);
    }
}

/// A run of reads, shown as one line.
pub(crate) struct Reads {
    key: u64,
    lead: String,
    /// The first read: alone, the run looks like any step.
    first: ToolCall,
    paths: Vec<String>,
    statuses: Vec<String>,
}

impl Reads {
    fn draw(&self, s: Style) -> crate::ui::transcript::Draw {
        let (lead, first, paths, statuses) = (self.lead.clone(), self.first.clone(), self.paths.clone(), self.statuses.clone());
        Box::new(move |width| {
            let dot = s.green("●");
            vec![if paths.len() == 1 { steps::line(&first, &lead, &dot, &statuses[0], s, width) } else { steps::reads_line(&paths, &statuses, &lead, &dot, s, width) }]
        })
    }
}

pub(crate) struct Answer {
    markdown: Option<Markdown>,
    wrap: Option<Wrap>,
}

pub(crate) struct Step {
    call_id: String,
    /// What starts its line: rein's label, or room for it.
    lead: String,
    /// Its line is on screen already, from the question.
    header_shown: bool,
    /// Its result is on screen already (it was blocked).
    done: bool,
}

/// What one turn cost, for the status bar.
#[derive(Default)]
pub(crate) struct TurnStats {
    input: u64,
    cached: u64,
    pub(crate) started: Option<std::time::Instant>,
    /// How long it took, once it's over.
    took: Option<Duration>,
}

/// The most lines of your message the conversation shows again; the rest
/// is counted.
pub(crate) const ECHO_LINES: usize = 12;

impl Terminal {
    /// Asks `question`, whose answers are `y` and `n`: in the box, or on a
    /// line of its own without one. Only yes is yes.
    pub(crate) async fn yes(&mut self, question: Question) -> bool {
        if !self.input.pinned() {
            let pad = " ".repeat(GUTTER);
            say!("{}", question.title);
            if let Some(command) = &question.command {
                say!("{pad}{command}");
            }
            if let Some(detail) = &question.detail {
                say!("{pad}{}", self.err.dim(detail));
            }
            return matches!(self.input.read(&format!("{pad}[y]es  [n]o › "), false).await, Read::Line(answer) if matches!(answer.trim(), "y" | "Y" | "yes"));
        }
        matches!(self.input.ask(question).await, Read::Line(key) if key == "y")
    }

    pub(crate) fn new(root: PathBuf, sandbox: std::sync::Arc<Sandbox>, mode: Mode, pinned: bool, history: History, bell: bool) -> Terminal {
        let interrupted: std::sync::Arc<tokio::sync::Notify> = Default::default();
        let input = Input::start(pinned, interrupted.clone(), history, bell, root.clone(), sandbox);
        Terminal {
            out: Style::stdout(),
            err: Style::stderr(),
            fancy: std::io::stderr().is_terminal(),
            root,
            model: String::new(),
            effort: None,
            mode,
            sandboxed: false,
            input,
            interrupted,
            context_limit: compact::DEFAULT_LIMIT,
            context_used: 0,
            last_cache: None,
            answer: None,
            mid_line: false,
            spinner: None,
            step: None,
            noted: None,
            watched: None,
            hooked: Vec::new(),
            pasted: Vec::new(),
            last: Last::Nothing,
            labeled: false,
            thoughts: String::new(),
            thoughts_shown: 0,
            plan: Vec::new(),
            compacting: false,
            said_uncompressed: false,
            reads: None,
            next_key: 0,
            turn: TurnStats::default(),
            peers: None,
            tasks: Vec::new(),
            changed: Vec::new(),
            notes: Vec::new(),
            closing: false,
            adopted: false,
            spawn_always: false,
            said_tabs: false,
            spec: String::new(),
            tab_command: None,
        }
    }

    /// What another rein sent, as the agent hears it. A task's result is
    /// owed to its sender at the end of the turn it goes into.
    /// `None` for what isn't for the model: the rein that started this one
    /// being done with it, which the conversation acts on after the turn.
    pub(crate) fn heard_of(&mut self, incoming: Incoming) -> Option<Heard> {
        let text = incoming.text();
        Some(match incoming {
            Incoming::Changed { from, path } => Heard::Changed { by: from, path },
            Incoming::Task { from, reply_to, id, .. } => {
                self.tasks.push((reply_to, id));
                Heard::Peer { from, text }
            }
            Incoming::Message { from, .. } | Incoming::Result { from, .. } => Heard::Peer { from, text },
            Incoming::Close { .. } => {
                self.closing = true;
                return None;
            }
        })
    }

    /// Says how this rein is doing, to the others and on its tab.
    pub(crate) fn state(&self, state: State) {
        crate::ui::screen::progress(match state {
            State::Working => Progress::Working,
            State::Waiting => Progress::Waiting,
            State::Idle | State::Done => Progress::Off,
        });
        if let Some(peers) = &self.peers {
            peers.set_state(state, None);
        }
    }

    /// The question is answered: the others stop showing it, and the tab
    /// shows rein working again.
    fn answered(&self, answer: Approval) -> Approval {
        crate::ui::screen::progress(Progress::Working);
        if let Some(peers) = &self.peers {
            peers.answered();
        }
        answer
    }

    /// `call` changed a file: the others in this folder hear which.
    fn changed_file(&mut self, call: &ToolCall) {
        let args: rein_llm::serde_json::Value = rein_llm::serde_json::from_str(&call.arguments).unwrap_or_default();
        if let Some(path) = args["path"].as_str() {
            self.changed_path(path.trim_start_matches("./").to_owned());
        }
    }

    /// Notes that this turn changed `path`, for a task's result and the
    /// other reins.
    fn changed_path(&mut self, path: String) {
        if !self.changed.contains(&path) {
            self.changed.push(path.clone());
        }
        if let Some(peers) = &mut self.peers {
            peers.changed(&path);
        }
    }

    /// Stops the spinner, if one is going, which also erases its line.
    pub(crate) fn quiet(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop();
        }
    }

    /// Says what rein is doing: over the box, with the turn's time, or
    /// without one, on a spinning line of its own until something prints.
    pub(crate) fn doing(&mut self, what: &str) {
        if self.input.pinned() {
            self.input.busy(Some(what.to_owned()), self.turn.started.unwrap_or_else(std::time::Instant::now));
            return;
        }
        if !self.fancy {
            return;
        }
        self.quiet();
        let s = self.err;
        let lead = self.peek_lead();
        let what = what.to_owned();
        self.spinner = Some(Spinner::start(move |frame, seconds| {
            let line = format!("{lead}{} {what}… {}", s.yellow(&frame.to_string()), s.dim(&format!("({seconds}s · ctrl-c to stop)")));
            crate::ui::style::fit(&line, crate::ui::screen::size().1.saturating_sub(1))
        }));
    }

    /// Finishes whatever answer is on screen, so something else can follow.
    pub(crate) fn end_line(&mut self) {
        self.quiet();
        if let Some(mut answer) = self.answer.take() {
            let mut rest = answer.markdown.as_mut().map(Markdown::finish).unwrap_or_default();
            if let Some(wrap) = &mut answer.wrap {
                rest = wrap.push(&rest) + &wrap.finish();
            }
            out!("{rest}");
            if !rest.is_empty() {
                self.mid_line = !rest.ends_with('\n');
            }
            self.last = Last::Text;
        }
        if self.mid_line {
            out!("\n");
            self.mid_line = false;
        }
        std::io::stdout().flush().ok();
    }

    /// A blank line before a step that follows an answer or your message.
    fn gap_before_step(&mut self) {
        if matches!(std::mem::take(&mut self.last), Last::Text | Last::You) && self.fancy {
            say!();
        }
    }

    /// A new turn: your message under its label, then a blank line.
    pub(crate) fn start_turn(&mut self, echo: Option<&str>) {
        self.turn = TurnStats { started: Some(std::time::Instant::now()), ..TurnStats::default() };
        self.changed.clear();
        self.state(State::Working);
        self.last = Last::You;
        self.labeled = false;
        if self.fancy
            && let Some(message) = echo
        {
            let shown = self.input.shown(message);
            say!("\n{}", you_said(&shown, self.err, self.wrap_width().unwrap_or(80)));
        }
        // Without the box, the spinner's line comes straight after this one.
        if self.fancy && !self.input.pinned() {
            say!();
            self.last = Last::Nothing;
        }
        // The last turn's time goes; the spinner counts this one.
        self.input.set_bar(self.bar());
    }

    /// The end of a turn: the status bar.
    ///
    /// ```text
    /// ────────────────────────────────────────────
    ///  gpt-6.1-sol · ask · context ▰▱▱▱▱▱▱▱▱▱ 2% · cache 97% · 6.4s
    /// ```
    pub(crate) fn end_turn(&mut self) {
        self.end_line();
        self.input.busy(None, std::time::Instant::now());
        self.input.look_for_paths();
        self.turn.took = self.turn.started.map(|t| t.elapsed());
        if let Some(cache) = (self.turn.cached * 100).checked_div(self.turn.input) {
            self.last_cache = Some(cache);
        }
        if !self.fancy {
            return;
        }
        let bar = self.bar();
        if self.input.pinned() {
            self.input.set_bar(bar);
            return;
        }
        let columns = terminal_width().unwrap_or(80);
        say!();
        say!("{}", self.err.dim(&"─".repeat(columns.min(100))));
        say!("{bar}");
    }

    /// The model and its effort, the mode, how full the context is, how
    /// much came from the cache, and how long the last turn took.
    pub(crate) fn bar(&self) -> String {
        let s = self.err;
        let used = self.context_used;
        // Never 0% once anything is in there, nor a full cell short of it.
        let pct = if used == 0 { 0 } else { (used * 100).div_ceil(self.context_limit.max(1)).min(100) };
        let cells = pct.div_ceil(10) as usize;
        let gauge = s.blue(&"▰".repeat(cells)) + &s.dim(&"▱".repeat(10 - cells));
        let dot = s.dim("  ·  ");
        let model = match &self.effort {
            Some(effort) => format!("{} {}", s.bold(&self.model), s.dim(effort.as_str())),
            None => s.bold(&self.model),
        };
        // The modes that ask less stand out.
        let mode = match self.mode {
            Mode::Trust => s.red(Mode::Trust.as_str()),
            mode @ (Mode::Edits | Mode::Auto) => s.yellow(mode.as_str()),
            mode => mode.to_string(),
        };
        let cache = (self.turn.cached * 100).checked_div(self.turn.input).or(self.last_cache);
        let cache = cache.map(|cache| format!("{dot}{} {cache}%", s.dim("cache"))).unwrap_or_default();
        // The time only once the turn is over; the spinner counts until then.
        let took = self.turn.took.map(|t| format!("{dot}{}", s.dim(&format!("{:.1}s", t.as_secs_f32())))).unwrap_or_default();
        format!(" {model}{dot}{mode}{dot}{} {gauge} {pct}%{cache}{took}", s.dim("context"))
    }

    /// What starts rein's next line: its label the first time in a turn,
    /// then room for it, so everything rein does lines up under one name.
    fn lead(&mut self) -> String {
        if std::mem::replace(&mut self.labeled, true) { " ".repeat(GUTTER) } else { self.err.label("rein") }
    }

    /// The same, without using up the label: for a spinner, whose line
    /// goes away again.
    fn peek_lead(&self) -> String {
        if self.labeled { " ".repeat(GUTTER) } else { self.err.label("rein") }
    }

    /// How wide text is wrapped as it's printed. On the full screen it
    /// isn't: the screen lays it out itself, for whatever width the window
    /// has, then and after every resize.
    pub(crate) fn wrap_width(&self) -> Option<usize> {
        if self.input.pinned() { Some(UNWRAPPED) } else { terminal_width() }
    }

    /// Shows lines laid out for the window's width by `draw`: a step. On the
    /// full screen they're laid out again whenever the width changes.
    fn show(&self, draw: impl Fn(usize) -> Vec<String> + Send + 'static) {
        crate::ui::screen::show(Box::new(draw));
    }

    /// A read that went well: it joins the reads on screen just before it,
    /// if nothing came between, or starts a run of its own.
    fn show_read(&mut self, call: &ToolCall, status: String, lead: String) {
        let s = self.err;
        let args: rein_llm::serde_json::Value = rein_llm::serde_json::from_str(&call.arguments).unwrap_or_default();
        let path = args["path"].as_str().unwrap_or_default().to_owned();
        if let Some(reads) = &mut self.reads {
            reads.paths.push(path.clone());
            reads.statuses.push(status.clone());
            if crate::ui::screen::redraw(reads.key, reads.draw(s)) {
                return;
            }
        }
        self.next_key += 1;
        let reads = Reads { key: self.next_key, lead, first: call.clone(), paths: vec![path], statuses: vec![status] };
        crate::ui::screen::show_as(reads.key, reads.draw(s));
        self.reads = Some(reads);
    }

    /// Prints a step's line before its question, with the change it would
    /// make, unless it's on screen already.
    /// What to ask about a command: with a sandbox, what it wants past it;
    /// without one, all of it.
    fn command_question(&self, bash: Option<&BashCall>) -> &'static str {
        match bash.map(BashCall::access) {
            _ if !self.sandboxed => "Allow this command?",
            Some(Access::Write) => "Allow this command to change files?",
            Some(Access::Network) => "Allow this command to use the network?",
            Some(Access::Outside) => "Allow this command to run outside the sandbox?",
            Some(Access::Look) | None => "Allow this command?",
        }
    }

    fn show_header(&mut self, call: &ToolCall) {
        let shown = self.step.as_ref().is_some_and(|step| step.call_id == call.call_id && step.header_shown);
        if !shown {
            self.gap_before_step();
            let lead = self.lead();
            let s = self.err;
            let diff = if matches!(ToolName::of(call), Some(ToolName::Edit | ToolName::Write)) { steps::diff_box(call, &self.root, s, WIDE, false) } else { Vec::new() };
            let (call_, lead_) = (call.clone(), lead.clone());
            self.show(move |width| {
                let mut lines = vec![steps::line(&call_, &lead_, &s.dim("○"), "", s, width)];
                lines.extend(diff.iter().map(|line| crate::ui::style::fit(line, width)));
                lines
            });
            self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: true, done: false });
        }
    }
}

/// A width nothing is wrapped at: text goes to the full screen whole, and
/// is laid out there.
pub(crate) const UNWRAPPED: usize = usize::MAX / 4;

/// The width a file change's frame is made at once, to be cut to the
/// window's width as it's shown: it's worked out from the file as it is
/// then, which may change later.
pub(crate) const WIDE: usize = 400;

/// Your message as the conversation shows it: under your label, wrapped,
/// and only its first lines if it's long, like a big paste.
/// What another rein said, as the conversation shows it: who from, then
/// its words, under the gutter, as long as your message would be shown.
pub(crate) fn peer_said(from: &str, message: &str, s: Style, columns: usize) -> String {
    let lines: Vec<&str> = message.lines().collect();
    let shown = if lines.len() > ECHO_LINES { &lines[..ECHO_LINES - 1] } else { &lines[..] };
    let mut wrap = Wrap::new(columns, GUTTER, GUTTER);
    let text = wrap.push(&shown.join("\n")) + &wrap.finish();
    let mut out = format!("{}{}\n{}{text}", " ".repeat(GUTTER), s.yellow(&format!("✉ from {from}")), " ".repeat(GUTTER));
    if lines.len() > shown.len() {
        out += &format!("\n{}{}", " ".repeat(GUTTER), s.dim(&format!("… {} more lines", lines.len() - shown.len())));
    }
    out
}

pub(crate) fn you_said(message: &str, s: Style, columns: usize) -> String {
    let lines: Vec<&str> = message.lines().collect();
    let shown = if lines.len() > ECHO_LINES { &lines[..ECHO_LINES - 1] } else { &lines[..] };
    let mut wrap = Wrap::new(columns, GUTTER, GUTTER);
    let text = wrap.push(&shown.join("\n")) + &wrap.finish();
    let mut out = format!("{}{}", s.you(), s.bold(&text));
    if lines.len() > shown.len() {
        let rest = lines.len() - shown.len();
        out += &format!("\n{}{}", " ".repeat(GUTTER), s.dim(&format!("… {rest} more lines")));
    }
    out
}

impl Ui for Terminal {
    fn text(&mut self, delta: &str) {
        self.quiet();
        if self.answer.is_none() {
            let width = self.wrap_width();
            if width.is_some() {
                if matches!(std::mem::take(&mut self.last), Last::Step | Last::You) {
                    out!("\n");
                }
                // A summary has no label: it isn't an answer to you.
                let lead = if self.compacting { " ".repeat(GUTTER) } else { self.lead() };
                // The label came from stderr's style; stdout may differ.
                let lead = if self.out.on() { lead } else { strip(&lead) };
                out!("{lead}");
            }
            self.answer = Some(Answer { markdown: width.map(|_| Markdown::new(self.out.on())), wrap: width.map(|width| Wrap::new(width, GUTTER, GUTTER)) });
            if self.input.pinned() && !self.compacting {
                self.doing("Writing");
            }
        }
        let answer = self.answer.as_mut().expect("set above");
        let mut text = match &mut answer.markdown {
            Some(markdown) => markdown.push(delta),
            None => delta.to_owned(),
        };
        let width = if self.input.pinned() { None } else { terminal_width() };
        if let Some(wrap) = &mut answer.wrap {
            if let Some(width) = width {
                wrap.set_width(width);
            }
            text = wrap.push(&text);
        }
        out!("{text}");
        if !text.is_empty() {
            self.mid_line = !text.ends_with('\n');
        }
    }

    fn thinking(&mut self) {
        self.end_line();
        if self.fancy {
            self.thoughts.clear();
            self.thoughts_shown = 0;
            self.doing("Thinking");
        }
    }

    fn starting_servers(&mut self) {
        self.end_line();
        if self.fancy {
            self.doing("Starting MCP servers");
        }
    }

    /// Each headline of the model's reasoning summary, once it's complete,
    /// becomes one dim line: what the model was thinking about. (On this
    /// route they arrive when the thinking is done, not during it.)
    fn thought(&mut self, delta: &str) {
        self.thoughts.push_str(delta);
        let headlines = headlines(&self.thoughts);
        if headlines.len() > self.thoughts_shown && self.fancy {
            self.quiet();
            self.gap_before_step();
            for headline in &headlines[self.thoughts_shown..] {
                let lead = self.lead();
                say!("{lead}{}", self.err.dim(&format!("✻ {headline}")));
            }
            self.thoughts_shown = headlines.len();
            if !self.input.pinned() {
                self.doing("Thinking");
            }
        }
    }

    fn note(&mut self, note: &str) {
        self.quiet();
        self.gap_before_step();
        let lead = self.lead();
        say!("{lead}{}", self.err.dim(&format!("↳ {note}")));
    }

    async fn approve(&mut self, call: &ToolCall, rule: Option<&Rule>) -> Approval {
        self.end_line();
        self.input.busy(None, std::time::Instant::now());
        self.step = None;
        crate::ui::screen::progress(Progress::Waiting);
        let bash = BashCall::of(call);
        // An MCP server's tool, by its server's name and its own.
        let server_tool = rein_mcp::parts(&call.name).filter(|_| ToolName::of(call).is_none());
        let (question, always) = match (ToolName::of(call), server_tool) {
            (Some(ToolName::Bash), _) => (self.command_question(bash.as_ref()).to_owned(), "Yes, and don't ask again for this command"),
            (Some(ToolName::Edit), _) => ("Allow this edit?".to_owned(), "Yes, and don't ask again for file changes"),
            (Some(ToolName::Write), _) => ("Allow writing this file?".to_owned(), "Yes, and don't ask again for file changes"),
            (None, Some((server, tool))) => (format!("Let {server}'s {tool} run?"), "Yes, and don't ask again for this tool"),
            _ => ("Allow this?".to_owned(), "Yes, and don't ask again"),
        };
        // Why it needs to go past the sandbox, as the model put it, and the
        // rule that asks, which is yours to change.
        let why = bash.as_ref().and_then(BashCall::reason).map(|reason| format!("Why: {reason}"));
        let asks = rule.map(|rule| match rule.whose() {
            Whose::Yours => format!("Your rule `{rule}` asks first · /permissions changes your rules"),
            Whose::Project => format!("This project's rule `{rule}` asks first"),
        });
        let reason = match (why, asks) {
            (Some(why), Some(asks)) => Some(format!("{why} · {asks}")),
            (why, asks) => why.or(asks),
        };
        if !self.input.pinned() {
            self.show_header(call);
            let pad = " ".repeat(steps::INDENT);
            if let Some(reason) = &reason {
                say!("{pad}{}", self.err.dim(reason));
            }
            let prompt = format!("{pad}{question}  [y]es  [n]o  [a]lways › ");
            return match self.input.read(&prompt, false).await {
                Read::Line(answer) => self.answered(approval(&answer)),
                // Ctrl-C at a question stops the whole turn.
                Read::Interrupted => {
                    // The turn is waiting on this; nothing is kept for later.
                    self.interrupted.notify_waiters();
                    std::future::pending().await
                }
                // No answer (input has ended) means no.
                Read::Eof | Read::NextMode | Read::Picked(_) | Read::Paste => Approval::No,
            };
        }
        // A command is asked about in full, in the box, and its line comes
        // once it's done. A file change shows its change above first, and
        // so does a command too long for the box.
        // A server's tool is asked about with all it's given, as a command is.
        let command = match ToolName::of(call) {
            Some(ToolName::Bash) => steps::command(call),
            None if server_tool.is_some() => Some(steps::arguments(call)),
            _ => None,
        };
        let long = command.as_deref().is_some_and(|command| crate::input::command_lines(command, self.err, self.input.text_width()).len() > crate::input::COMMAND_LINES);
        if command.is_none() || long {
            self.show_header(call);
        }
        if long && let Some(command) = command.clone() {
            let s = self.err;
            self.show(move |width| {
                let pad = " ".repeat(steps::INDENT);
                crate::input::command_lines(&command, s, width.saturating_sub(steps::INDENT)).into_iter().map(|line| format!("{pad}{line}")).collect()
            });
        }
        let answers = vec![("y", "Yes".to_owned()), ("a", always.to_owned()), ("n", "No".to_owned()), ("esc", "No, and tell rein what to do instead".to_owned())];
        // The other reins see it's waiting, and you can answer from another tab.
        let asked = format!("{question} {}", command.clone().unwrap_or_else(|| command_line(call)));
        let elsewhere = self.peers.as_mut().map(|peers| peers.question(&shorten_line(&asked)));
        let here = self.input.ask(Question { title: question.clone(), detail: reason, command, answers });
        let answer = match elsewhere {
            Some(there) => {
                // Never answered elsewhere if nothing over there can answer.
                let there = async {
                    match there.await {
                        Ok(yes) => yes,
                        Err(_) => std::future::pending().await,
                    }
                };
                tokio::select! {
                    read = here => Some(read),
                    yes = there => {
                        self.input.drop_question();
                        self.noted = Some(format!("answered {} from another tab", if yes { "yes" } else { "no" }));
                        return self.answered(if yes { Approval::Yes } else { Approval::No });
                    }
                }
            }
            None => Some(here.await),
        };
        let answer = match answer {
            Some(Read::Line(key)) => approval(&key),
            _ => Approval::No,
        };
        let answer = self.answered(answer);
        if answer == Approval::Always {
            let what = match ToolName::of(call) {
                Some(ToolName::Bash) => "this command",
                None if server_tool.is_some() => "this tool",
                _ => "file changes",
            };
            self.noted = Some(format!("won't ask again for {what} in this conversation"));
        }
        answer
    }

    async fn peer(&mut self, call: &ToolCall) -> String {
        if ToolName::of(call) == Some(ToolName::Spawn) {
            return crate::peers::talk::spawn(call, self).await;
        }
        match &self.peers {
            Some(peers) => peers.tool(call).await,
            None => format!("{}this rein isn't among the others: start it in a conversation, with peers on", rein_agent::tools::ERROR),
        }
    }

    fn tool_started(&mut self, call: &ToolCall) {
        self.end_line();
        let asked = self.step.as_ref().is_some_and(|step| step.call_id == call.call_id);
        if !asked {
            // Not asked about: the step's line shows once it's done, with
            // how it went.
            self.gap_before_step();
            let lead = self.lead();
            self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: false, done: false });
        }
        if let Some(peers) = &self.peers {
            peers.doing(&steps::doing(call));
        }
        if self.input.pinned() {
            self.doing(&steps::doing(call));
            return;
        }
        if self.fancy {
            let s = self.err;
            let step = self.step.as_ref().expect("set above");
            // A step already on screen spins under itself; otherwise its
            // own line spins, and turns into its result when it's done.
            let (call, lead, own_line) = (call.clone(), step.lead.clone(), !step.header_shown);
            self.spinner = Some(Spinner::start(move |frame, seconds| {
                let (_, columns) = crate::ui::screen::size();
                let time = s.dim(&format!("{seconds}s"));
                let line = if own_line {
                    steps::line(&call, &lead, &s.yellow(&frame.to_string()), &time, s, columns)
                } else {
                    format!("{}{} {}", " ".repeat(steps::INDENT), s.yellow(&frame.to_string()), s.dim(&format!("running · {seconds}s")))
                };
                crate::ui::style::fit(&line, columns.saturating_sub(1))
            }));
        }
    }

    fn tool_blocked(&mut self, call: &ToolCall, reason: &str) {
        self.end_line();
        self.gap_before_step();
        self.step = None;
        let lead = self.lead();
        let s = self.err;
        let (call_, lead_, status) = (call.clone(), lead.clone(), s.red(&format!("blocked: {reason}")));
        self.show(move |width| vec![steps::line(&call_, &lead_, &s.red("●"), &status, s, width)]);
        self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: true, done: true });
        self.last = Last::Step;
    }

    fn tool_finished(&mut self, call: &ToolCall, output: &str) {
        self.quiet();
        let step = self.step.take().filter(|step| step.call_id == call.call_id);
        if step.as_ref().is_some_and(|step| step.done) {
            return;
        }
        let s = self.err;
        let asked = step.as_ref().is_some_and(|step| step.header_shown);
        let mut outcome = steps::outcome(call, output, s);
        if ToolName::of(call).is_some_and(ToolName::changes_files) && outcome.ok == Some(true) {
            self.changed_file(call);
        }
        if ToolName::of(call) == Some(ToolName::Plan) {
            // The same steps as before: just say where things stand.
            let names: Vec<String> = outcome.details.iter().map(|line| strip(line).chars().skip(2).collect()).collect();
            if names == self.plan {
                let now = outcome.details.iter().find(|line| strip(line).starts_with('▸')).map(|line| strip(line));
                outcome.status = s.dim(&now.map_or("all done".to_owned(), |now| format!("· {now}")));
                outcome.details.clear();
            } else {
                self.plan = names;
            }
        }
        if self.input.pinned() {
            let (name, target) = steps::describe(call);
            self.input.keep_output(format!("{name}  {target} · {}", strip(&outcome.status)), output);
        }
        let dot = match outcome.ok {
            Some(true) => s.green("●"),
            Some(false) => s.red("●"),
            None => s.dim("●"),
        };
        // Asked about first: its line is up there; the result goes under it.
        let lead = match step {
            Some(step) if step.header_shown => None,
            step => {
                // Never started, like a declined step: it's the first time it shows.
                if step.is_none() {
                    self.gap_before_step();
                }
                Some(step.map_or_else(|| self.lead(), |step| step.lead))
            }
        };
        let note = self.noted.take().map(|note| format!("{}{} {}", " ".repeat(steps::INDENT), s.green("✓"), s.dim(&note)));
        let watched = self.watched.take().map(|watched| format!("{}{}", " ".repeat(steps::INDENT), s.dim(&format!("↳ {watched}"))));
        let hooked: Vec<String> = std::mem::take(&mut self.hooked).iter().map(|what| format!("{}{}", " ".repeat(steps::INDENT), s.dim(&format!("↳ {what}")))).collect();
        // Reads in a row share a line.
        if ToolName::of(call) == Some(ToolName::Read)
            && outcome.ok == Some(true)
            && note.is_none()
            && let Some(lead) = lead.clone()
        {
            self.show_read(call, outcome.status, lead);
            self.last = Last::Step;
            return;
        }
        // An edit made without asking shows what it changed; a question
        // showed it already. A new file's whole text is Ctrl-O away.
        let diff = if !asked && ToolName::of(call) == Some(ToolName::Edit) && outcome.ok == Some(true) { steps::diff_box(call, &self.root, s, WIDE, true) } else { Vec::new() };
        let (call_, viewer) = (call.clone(), self.input.pinned());
        self.show(move |width| {
            let mut lines = vec![match &lead {
                Some(lead) => steps::line(&call_, lead, &dot, &outcome.status, s, width),
                None => format!("{}{dot} {}", " ".repeat(steps::INDENT), outcome.status),
            }];
            lines.extend(steps::detail_lines(&outcome, s, width, viewer));
            lines.extend(watched.iter().map(|line| crate::ui::style::fit(line, width)));
            lines.extend(hooked.iter().map(|line| crate::ui::style::fit(line, width)));
            lines.extend(note.clone());
            lines.extend(diff.iter().map(|line| crate::ui::style::fit(line, width)));
            lines
        });
        self.last = Last::Step;
    }

    fn usage(&mut self, usage: &Usage) {
        if let Some(peers) = &self.peers {
            peers.add_tokens(usage.input_tokens.saturating_add(usage.output_tokens));
        }
        self.turn.input += usage.input_tokens;
        self.turn.cached += usage.cached_input_tokens;
        self.context_used = usage.input_tokens;
        // The box's bar follows along during the turn.
        if self.input.pinned() {
            self.input.set_bar(self.bar());
        }
    }

    /// What a command did to the project's files: said under its line.
    fn hooked(&mut self, _call: &ToolCall, what: &str) {
        self.hooked.push(what.to_owned());
    }

    fn away(&self) -> bool {
        self.input.away()
    }

    fn watched(&mut self, _call: &ToolCall, what: &Watched) {
        self.watched = Some(match what {
            Watched::Changed(changed) => {
                for path in changed.changed.iter().chain(&changed.made).chain(&changed.removed) {
                    self.changed_path(path.clone());
                }
                crate::format::changes(changed)
            }
            Watched::NotHere(off) => format!("/undo won't put back what commands do here: {off}"),
        });
    }

    /// Lookups asked for together: one line over the box says so.
    fn tools_started(&mut self, calls: &[ToolCall]) {
        self.end_line();
        self.gap_before_step();
        self.step = None;
        let reads = calls.iter().all(|call| ToolName::of(call) == Some(ToolName::Read));
        self.doing(&if reads { format!("Reading {} files", calls.len()) } else { format!("Looking at {} things at once", calls.len()) });
    }

    /// What your message pointed at, under it: in rein's column, without
    /// rein's label, as you're the one who asked.
    fn attached(&mut self, call: &ToolCall, output: &str) {
        self.end_line();
        let s = self.err;
        let outcome = steps::outcome(call, output, s);
        if self.input.pinned() {
            let (name, target) = steps::describe(call);
            self.input.keep_output(format!("{name}  {target} · {}", strip(&outcome.status)), output);
        }
        let lead = " ".repeat(GUTTER);
        if ToolName::of(call) == Some(ToolName::Read) && outcome.ok == Some(true) {
            self.show_read(call, outcome.status, lead);
            return;
        }
        let dot = if outcome.ok == Some(false) { s.red("●") } else { s.green("●") };
        let (call_, viewer) = (call.clone(), self.input.pinned());
        self.show(move |width| {
            let mut lines = vec![steps::line(&call_, &lead, &dot, &outcome.status, s, width)];
            lines.extend(steps::detail_lines(&outcome, s, width, viewer));
            lines
        });
    }

    /// Your messages, then what came in from other reins.
    fn steering(&mut self) -> Vec<Heard> {
        let mut heard: Vec<Heard> = self.input.steering().into_iter().map(Heard::You).collect();
        // Typed into a sub-agent while it works: it's yours now too.
        self.adopted |= !heard.is_empty();
        heard.append(&mut self.notes);
        let mut incoming = Vec::new();
        if let Some(peers) = &mut self.peers {
            while let Ok(one) = peers.inbox.try_recv() {
                incoming.push(one);
            }
        }
        heard.extend(incoming.into_iter().filter_map(|one| self.heard_of(one)));
        heard
    }

    /// What was heard, where it went into the turn: between the steps it
    /// came between, so the screen reads as the model saw it.
    fn steered(&mut self, heard: &Heard) {
        self.end_line();
        let columns = self.wrap_width().unwrap_or(80);
        match heard {
            Heard::You(message) if self.fancy => {
                let shown = self.input.shown(message);
                say!("\n{}", you_said(&shown, self.err, columns));
            }
            Heard::Peer { from, text } if self.fancy => say!("\n{}", peer_said(from, text, self.err, columns)),
            Heard::Changed { by, path } => {
                say!("{}{}", " ".repeat(GUTTER), self.err.dim(&format!("↳ {by} changed {path}, which this conversation has seen")));
                return;
            }
            _ => {}
        }
        // What rein does next starts under its label again, after a gap.
        self.labeled = false;
        self.last = Last::You;
    }

    fn compacting(&mut self) {
        self.end_line();
        self.gap_before_step();
        let lead = self.lead();
        say!("{lead}{}", self.err.dim("compacting the conversation…"));
        self.compacting = true;
        self.doing("Compacting the conversation");
    }

    fn compacted(&mut self, report: &Compacted) {
        self.end_line();
        self.compacting = false;
        let retired = match report.retired {
            0 => String::new(),
            1 => ", 1 stale tool output retired".to_owned(),
            n => format!(", {n} stale tool outputs retired"),
        };
        let how = if report.summarized { "summarized" } else { "no summary needed" };
        let line = format!("compacted: about {} → {} tokens ({how}{retired})", thousands(report.before), thousands(report.after));
        say!("{}{}", " ".repeat(GUTTER), self.err.dim(&line));
        self.last = Last::Step;
        self.context_used = report.after;
        self.input.set_bar(self.bar());
    }
}

/// The finished `**headlines**` in a reasoning summary, which is how the
/// model titles each part: `Checking the tests`.
pub(crate) fn headlines(thoughts: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = thoughts;
    while let Some(start) = rest.find("**") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("**") else { break };
        let title = after[..end].trim();
        if !title.is_empty() {
            found.push(title.to_owned());
        }
        rest = &after[end + 2..];
    }
    found
}

/// A command's whole text, for saying what a question is about.
fn command_line(call: &ToolCall) -> String {
    let (name, target) = steps::describe(call);
    format!("{name} {target}")
}

/// `text` on one line, short enough for a tab or the dashboard.
fn shorten_line(text: &str) -> String {
    crate::peers::node::preview(text)
}

/// Reads a typed answer: y or a; anything else, including nothing, is no.
pub(crate) fn approval(answer: &str) -> Approval {
    match answer.trim() {
        "y" | "Y" | "yes" => Approval::Yes,
        "a" | "A" | "always" => Approval::Always,
        // Esc, in the box: no, and stop to say what to do instead.
        "esc" | "stop" => Approval::Stop,
        _ => Approval::No,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_answers() {
        assert_eq!(approval("y\n"), Approval::Yes);
        assert_eq!(approval("a\n"), Approval::Always);
        assert_eq!(approval("\n"), Approval::No);
        assert_eq!(approval("yolo"), Approval::No);
    }

    #[test]
    fn finds_the_headlines() {
        assert_eq!(headlines("**Deriving the list**\n\nsome text **Checking Pell solutions**"), ["Deriving the list", "Checking Pell solutions"]);
        assert!(headlines("**Half writ").is_empty());
        assert_eq!(headlines("**First** and **Sec"), ["First"]);
    }
}
