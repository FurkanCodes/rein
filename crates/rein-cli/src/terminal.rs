//! The terminal rein talks through: what it shows of a turn as it
//! happens, and how it asks, as the agent's [`Ui`].

use crate::format::thousands;
use crate::input::{Input, Question, Read};
use crate::peers::node::Incoming;
use crate::peers::{Node, State};
use crate::ui::asked::{Asked, INSTEAD};
use crate::ui::history::History;
use crate::ui::markdown::Markdown;
use crate::ui::screen::Progress;
use crate::ui::spinner::Spinner;
use crate::ui::steps;
use crate::ui::style::{GUTTER, Style, strip};
use crate::ui::tint::Tint;
use crate::ui::wrap::Wrap;
use rein_agent::agent::{Approval, Compacted, Heard, Ui};
use rein_agent::compact;
use rein_agent::permissions::{Mode, Rule, Sandboxing};
use rein_agent::sandbox::Sandbox;
use rein_agent::tools::ToolName;
use rein_agent::undo::{Changed, Watched};
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
    /// Whether commands run in the sandbox: when they do, what one asks
    /// for past it is what's asked about.
    pub(crate) sandboxing: Sandboxing,
    pub(crate) input: Input,
    /// Ctrl-C pressed at a question; the turn waits on this to stop.
    pub(crate) interrupted: std::sync::Arc<tokio::sync::Notify>,
    /// When the conversation gets compacted, in tokens.
    pub(crate) context_limit: u64,
    /// How many tokens the conversation took at the last count.
    pub(crate) context_used: u64,
    /// How much of the last finished turn came from the prompt cache, in percent.
    pub(crate) last_cache: Option<u64>,
    /// How much of your limits is used, as the provider last said.
    pub(crate) meters: crate::meters::Meters,
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
    /// The steps that only looked on screen as one line, which the next
    /// joins if nothing came between.
    looks: Option<Looks>,
    /// The key the next run of looks is shown under.
    next_key: u64,
    pub(crate) turn: TurnStats,
    /// This rein among the others on this computer, when it joined them.
    pub(crate) peers: Option<Node>,
    /// Tasks other reins handed over in this turn: where each one's
    /// result goes, and its ID.
    pub(crate) tasks: Vec<(String, String)>,
    /// What came in while no turn ran that starts none: notes of files
    /// other reins changed, and what you did in a sub-agent's tab, for the
    /// next turn's start.
    pub(crate) notes: Vec<Heard>,
    /// The rein that started this one has gone: this one goes once it's
    /// idle, unless it's been adopted.
    pub(crate) closing: bool,
    /// You've typed into this sub-agent: it's yours now, and stays when
    /// the rein that started it goes.
    pub(crate) adopted: bool,
    /// The task whose report went back to the rein that started this one,
    /// where it went and its ID: what you do here after is told there too.
    pub(crate) followed: Option<(String, String)>,
    /// You said always to opening tabs for sub-agents.
    pub(crate) spawn_always: bool,
    /// It's been said how Terminal can open tabs, not windows.
    pub(crate) said_tabs: bool,
    /// The model in use, as `provider/model`: what a sub-agent gets unless
    /// it's given another.
    pub(crate) spec: String,
    /// The `tab_command` setting: how to open a tab here, if rein doesn't know.
    pub(crate) tab_command: Option<String>,
    /// The pages the model makes, which open in your browser.
    pub(crate) pages: Option<rein_agent::pages::Pages>,
    /// Pages being opened in the browser: they stop with rein.
    opening: tokio::task::JoinSet<bool>,
}

impl Drop for Terminal {
    /// The tab's bar goes with rein.
    fn drop(&mut self) {
        crate::ui::screen::progress(Progress::Off);
    }
}

/// Steps in a row that only looked (reads and searches), shown as one
/// line: `Read 2 files, searched for 1 pattern`.
pub(crate) struct Looks {
    key: u64,
    lead: String,
    /// The first: alone, the run looks like any step.
    first: ToolCall,
    first_status: String,
    /// The reads' paths and what each came to, for a run of reads alone.
    paths: Vec<String>,
    statuses: Vec<String>,
    searches: usize,
}

impl Looks {
    fn draw(&self, s: Style) -> crate::ui::transcript::Draw {
        let (lead, first, first_status) = (self.lead.clone(), self.first.clone(), self.first_status.clone());
        let (paths, statuses, searches) = (self.paths.clone(), self.statuses.clone(), self.searches);
        Box::new(move |width| {
            let mark = s.green("✓");
            vec![match (paths.len(), searches) {
                (1, 0) | (0, 1) => steps::line(&first, &lead, &mark, &first_status, s, width),
                (_, 0) => steps::reads_line(&paths, &statuses, &lead, &mark, s, width),
                (reads, searches) => steps::looks_line(reads, searches, &lead, &mark, s, width),
            }]
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

/// What one turn cost, for the status bar, and what it did, for the line
/// that ends it.
#[derive(Default)]
pub(crate) struct TurnStats {
    input: u64,
    cached: u64,
    /// The steps it ran.
    steps: usize,
    /// The files it changed, each once.
    changed: Vec<String>,
    pub(crate) started: Option<std::time::Instant>,
    /// How long it took, once it's over.
    took: Option<Duration>,
}

impl TurnStats {
    /// A step ran.
    pub(crate) const fn stepped(&mut self) {
        self.steps += 1;
    }

    /// It changed a file.
    pub(crate) const fn changed_files(&self) -> bool {
        !self.changed.is_empty()
    }

    /// What a turn that ran steps came to: `4 steps · changed slug.py ·
    /// 24s`. A turn that only talked has none.
    pub(crate) fn summary(&self) -> Option<String> {
        let steps = match self.steps {
            0 => return None,
            1 => "1 step".to_owned(),
            n => format!("{n} steps"),
        };
        let changed = match self.changed.as_slice() {
            [] => "changed no files".to_owned(),
            [one] => format!("changed {one}"),
            many => format!("changed {} files", many.len()),
        };
        let took = self.took.map(|took| format!(" · {}", crate::peers::strip::short(took.as_secs()))).unwrap_or_default();
        Some(format!("{steps} · {changed}{took}"))
    }
}

/// What a check came to, in a line.
pub(crate) fn check_said(checked: &rein_agent::check::Checked) -> String {
    use rein_agent::check::Verdict;
    let took = format!("{:.1}s", checked.took.as_secs_f32());
    match checked.verdict {
        Verdict::Clean => format!("{} · no new errors · {took}", checked.name),
        Verdict::New(count) => format!("{} · {count} new error{} · {took}", checked.name, if count == 1 { "" } else { "s" }),
        Verdict::Fixed => format!("{} · the errors these changes brought in are fixed · {took}", checked.name),
        Verdict::Busy => format!("{} · not run: another command had the build", checked.name),
    }
}

/// The most lines of your message the conversation shows again; the rest
/// is counted.
pub(crate) const ECHO_LINES: usize = 12;

impl Terminal {
    /// The key of the answer you chose to `question`. With nothing to show
    /// its answers on, it's asked as yes or no, and comes to `y` or `n`.
    pub(crate) async fn answer(&mut self, question: Question) -> Option<String> {
        if !self.input.pinned() && !self.input.has_stand_in() {
            return Some(if self.yes(question).await { "y" } else { "n" }.to_owned());
        }
        match self.input.ask(question).await {
            Read::Line(key) => Some(key),
            _ => None,
        }
    }

    /// Asks `question`, whose answers are `y` and `n`: in the box, or on a
    /// line of its own without one. Only yes is yes.
    pub(crate) async fn yes(&mut self, question: Question) -> bool {
        if !self.input.pinned() && !self.input.has_stand_in() {
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
            sandboxing: Sandboxing::Off,
            input,
            interrupted,
            context_limit: compact::DEFAULT_LIMIT,
            context_used: 0,
            last_cache: None,
            meters: crate::meters::Meters::default(),
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
            looks: None,
            next_key: 0,
            turn: TurnStats::default(),
            peers: None,
            tasks: Vec::new(),
            notes: Vec::new(),
            closing: false,
            adopted: false,
            followed: None,
            spawn_always: false,
            said_tabs: false,
            spec: String::new(),
            tab_command: None,
            pages: None,
            opening: tokio::task::JoinSet::new(),
        }
    }

    /// What another rein sent, as the agent hears it. A task's result is
    /// owed to its sender at the end of the turn it goes into, and sent
    /// even if rein is ended first. `None` for what isn't for the model:
    /// the rein that started this one having gone, which the conversation
    /// acts on after the turn.
    pub(crate) fn heard_of(&mut self, incoming: Incoming) -> Option<Heard> {
        let text = incoming.text();
        Some(match incoming {
            Incoming::Changed { from, path } => Heard::Changed { by: from, path },
            Incoming::Task { from, reply_to, id, .. } => {
                crate::peers::farewell::owe(&reply_to, &id);
                self.tasks.push((reply_to, id));
                Heard::Peer { from, text }
            }
            Incoming::Message { from, .. } => Heard::Peer { from, text },
            // Shown whole as it came, by whoever took it in.
            Incoming::Result { from, .. } => Heard::Reported { from, text },
            Incoming::Followed { from, .. } => Heard::Told { from, text },
            Incoming::ParentGone { .. } => {
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
        // A patch changes each file it names.
        if let Ok(patch) = rein_agent::tools::patch_of(call) {
            for path in patch.paths() {
                self.changed_path(path.trim_start_matches("./").to_owned());
            }
            return;
        }
        let args: rein_llm::serde_json::Value = rein_llm::serde_json::from_str(&call.arguments).unwrap_or_default();
        if let Some(path) = args["path"].as_str() {
            self.changed_path(path.trim_start_matches("./").to_owned());
        }
    }

    /// What came in from the other reins, oldest first.
    pub(crate) fn take_incoming(&mut self) -> Vec<Incoming> {
        let mut incoming = Vec::new();
        if let Some(peers) = &mut self.peers {
            while let Ok(one) = peers.inbox.try_recv() {
                incoming.push(one);
            }
        }
        incoming
    }

    /// A step starts: the others see it, and a task's result will say so.
    /// These five hold for any `Ui` running this conversation.
    pub(crate) fn step_started(&self, call: &ToolCall) {
        let (name, target) = steps::describe(call);
        crate::peers::farewell::running(&format!("{name} {target}"));
        if let Some(peers) = &self.peers {
            peers.doing(&steps::doing(call));
        }
    }

    /// A step was turned down, for a task's result.
    pub(crate) fn step_blocked(&self, call: &ToolCall, reason: &str) {
        let (name, target) = steps::describe(call);
        crate::peers::farewell::stepped(&format!("{name} {target} · blocked: {reason}"));
    }

    /// A step ran, for a task's result and the others in this folder.
    pub(crate) fn step_finished(&mut self, call: &ToolCall, outcome: &steps::Outcome) {
        let (name, target) = steps::describe(call);
        crate::peers::farewell::stepped(&format!("{name} {target} · {}", strip(&outcome.status)));
        if ToolName::of(call).is_some_and(ToolName::changes_files) && outcome.ok == Some(true) {
            self.changed_file(call);
        }
        if ToolName::of(call) == Some(ToolName::Page) && outcome.ok == Some(true) {
            self.open_page(call);
        }
    }

    /// Opens the page a step saved in the browser, unless it's showing
    /// there already: then it has reloaded itself.
    fn open_page(&mut self, call: &ToolCall) {
        let Some(name) = rein_agent::pages::Pages::saved_by(&call.arguments) else { return };
        let Some(link) = self.pages.as_ref().and_then(|pages| pages.to_open(&name)) else { return };
        while self.opening.try_join_next().is_some() {}
        self.opening.spawn(async move { crate::browser::open(&link).await });
    }

    /// What a request used, which the others see.
    pub(crate) fn tokens_used(&self, usage: &Usage) {
        if let Some(peers) = &self.peers {
            peers.add_tokens(usage.input_tokens.saturating_add(usage.output_tokens));
        }
    }

    /// What a command did to files, for a task's result and the others in
    /// this folder.
    pub(crate) fn step_watched(&mut self, changed: &Changed) {
        for path in changed.changed.iter().chain(&changed.made).chain(&changed.removed) {
            self.changed_path(path.clone());
        }
    }

    /// Notes that this turn changed `path`, for a task's result and the
    /// other reins.
    fn changed_path(&mut self, path: String) {
        if !self.turn.changed.contains(&path) {
            self.turn.changed.push(path.clone());
        }
        crate::peers::farewell::changed(&path);
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
        crate::peers::farewell::turn_started();
        self.state(State::Working);
        self.last = Last::You;
        self.labeled = false;
        if self.fancy
            && let Some(message) = echo
        {
            show_you_said(&self.input.shown(message), self.err);
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
        // Where a turn that did some work ended, and what it came to.
        if let Some(summary) = self.turn.summary() {
            say!();
            say!("{}{}", " ".repeat(GUTTER), self.err.dim(&format!("── {summary} ──")));
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

    /// What the provider said of how much of your limits is used: in the
    /// bar, and in a line when one gets close.
    pub(crate) fn take_meters(&mut self, meters: Vec<rein_llm::Meter>) {
        for line in self.meters.read(meters, std::time::SystemTime::now()) {
            self.quiet();
            self.gap_before_step();
            let lead = self.lead();
            say!("{lead}{}", self.err.yellow(&format!("↳ {line}")));
        }
        if self.input.pinned() {
            self.input.set_bar(self.bar());
        }
    }

    /// The model and its effort, the mode, how full the context is, how
    /// much of your limits is used, how much came from the cache, and how
    /// long the last turn took. What's used of your limits goes first when
    /// the window is too narrow for it all.
    pub(crate) fn bar(&self) -> String {
        // What's used of your limits goes first, then the word for the
        // sandbox, before the context is cut off at the edge.
        let fits = |bar: &String| terminal_width().is_none_or(|columns| crate::ui::style::width(bar) <= columns);
        [Room::All, Room::NoMeters].into_iter().map(|room| self.bar_with(room)).find(fits).unwrap_or_else(|| self.bar_with(Room::Least))
    }

    fn bar_with(&self, room: Room) -> String {
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
        // The modes that ask less stand out, and those with nothing to
        // hold a command back most of all.
        let mode = match self.mode {
            mode if crate::modes::dangerous(mode, self.sandboxing) => s.red(&crate::modes::shown_name(mode, self.sandboxing)),
            mode @ (Mode::Edits | Mode::Auto) => s.yellow(mode.as_str()),
            mode => mode.to_string(),
        };
        // Off is where rein starts, so only on says so.
        let mode = match self.sandboxing {
            Sandboxing::On if room >= Room::NoMeters => format!("{mode} {}", s.dim("sandboxed")),
            Sandboxing::Missing if room >= Room::NoMeters => format!("{mode} {}", s.yellow("no sandbox")),
            _ => mode,
        };
        let cache = (self.turn.cached * 100).checked_div(self.turn.input).or(self.last_cache);
        let cache = cache.map(|cache| format!("{dot}{} {cache}%", s.dim("cache"))).unwrap_or_default();
        // The time only once the turn is over; the spinner counts until then.
        let took = self.turn.took.map(|t| format!("{dot}{}", s.dim(&format!("{:.1}s", t.as_secs_f32())))).unwrap_or_default();
        let meters = if room == Room::All { self.meters.bar(s, std::time::SystemTime::now()).iter().map(|meter| format!("{dot}{meter}")).collect() } else { String::new() };
        format!(" {model}{dot}{mode}{dot}{} {gauge} {pct}%{meters}{cache}{took}", s.dim("context"))
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

    /// A read or a search that went well: it joins those on screen just
    /// before it, if nothing came between, or starts a run of its own.
    fn show_look(&mut self, call: &ToolCall, status: String, lead: String) {
        let s = self.err;
        let read = ToolName::of(call) == Some(ToolName::Read);
        let args: rein_llm::serde_json::Value = rein_llm::serde_json::from_str(&call.arguments).unwrap_or_default();
        let path = args["path"].as_str().unwrap_or_default().to_owned();
        if let Some(looks) = &mut self.looks {
            if read {
                looks.paths.push(path.clone());
                looks.statuses.push(status.clone());
            } else {
                looks.searches += 1;
            }
            if crate::ui::screen::redraw(looks.key, looks.draw(s)) {
                return;
            }
        }
        self.next_key += 1;
        let (paths, statuses) = if read { (vec![path], vec![status.clone()]) } else { (Vec::new(), Vec::new()) };
        let looks = Looks { key: self.next_key, lead, first: call.clone(), first_status: status, paths, statuses, searches: usize::from(!read) };
        crate::ui::screen::show_as(looks.key, looks.draw(s));
        self.looks = Some(looks);
    }

    /// Prints a step's line before its question, with the change it would
    /// make, unless it's on screen already.
    fn show_header(&mut self, call: &ToolCall) {
        let shown = self.step.as_ref().is_some_and(|step| step.call_id == call.call_id && step.header_shown);
        if !shown {
            self.gap_before_step();
            let lead = self.lead();
            let s = self.err;
            let changes = ToolName::of(call).is_some_and(ToolName::changes_files);
            let change = if changes { steps::change(call, &self.root, false) } else { Vec::new() };
            let counts = if changes { steps::change_counts(call, s) } else { String::new() };
            let (call_, lead_) = (call.clone(), lead.clone());
            self.show(move |width| {
                let mut lines = vec![steps::line(&call_, &lead_, &s.dim("○"), &counts, s, width)];
                lines.extend(steps::change_lines(&change, s, width));
                lines
            });
            self.step = Some(Step { call_id: call.call_id.clone(), lead, header_shown: true, done: false });
        }
    }
}

/// How much of the status bar fits the window, from the least.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Room {
    /// The model, the mode and the context.
    Least,
    /// The word for the sandbox too.
    NoMeters,
    /// What's used of your limits too.
    All,
}

/// A width nothing is wrapped at: text goes to the full screen whole, and
/// is laid out there.
pub(crate) const UNWRAPPED: usize = usize::MAX / 4;

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

/// Your message as the conversation shows it, at `columns`: under your
/// label, wrapped, and only its first lines if it's long, like a big
/// paste. Where the terminal said its background, it's on a band to the
/// window's edge, so you find your messages at a glance; without one,
/// it's bold, as it was.
pub(crate) fn you_said(message: &str, s: Style, columns: usize) -> Vec<String> {
    let lines: Vec<&str> = message.lines().collect();
    let shown = if lines.len() > ECHO_LINES { &lines[..ECHO_LINES - 1] } else { &lines[..] };
    let band = crate::ui::tint::behind(Tint::Band, s).is_some();
    let mut wrap = Wrap::new(columns, GUTTER, GUTTER);
    let text = wrap.push(&shown.join("\n")) + &wrap.finish();
    let mut rows: Vec<String> = text
        .split('\n')
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 { s.you() } else { " ".repeat(GUTTER) };
            let row = row.strip_prefix(&" ".repeat(GUTTER)).unwrap_or(row);
            format!("{lead}{}", if band { row.to_owned() } else { s.bold(row) })
        })
        .collect();
    if lines.len() > shown.len() {
        let rest = lines.len() - shown.len();
        rows.push(format!("{}{}", " ".repeat(GUTTER), s.dim(&format!("… {rest} more lines"))));
    }
    rows.into_iter().map(|row| crate::ui::tint::fill(&row, Tint::Band, s, columns)).collect()
}

/// Shows your message, after a blank line, laid out for the window's
/// width now and after every resize.
pub(crate) fn show_you_said(message: &str, s: Style) {
    say!();
    let message = message.to_owned();
    crate::ui::screen::show(Box::new(move |width| you_said(&message, s, width)));
}

impl Ui for Terminal {
    fn text(&mut self, delta: &str) {
        self.quiet();
        if self.answer.is_none() {
            let width = self.wrap_width();
            // The model's words start with a bullet, which sets them apart
            // from its steps; its lines after line up under the words. A
            // summary has neither label nor bullet: it isn't an answer to you.
            let indent = if self.compacting { GUTTER } else { GUTTER + 2 };
            if width.is_some() {
                if matches!(std::mem::take(&mut self.last), Last::Step | Last::You) {
                    out!("\n");
                }
                let lead = if self.compacting { " ".repeat(GUTTER) } else { format!("{}{} ", self.lead(), self.err.bright("●")) };
                // The label came from stderr's style; stdout may differ.
                let lead = if self.out.on() { lead } else { strip(&lead) };
                out!("{lead}");
            }
            self.answer = Some(Answer { markdown: width.map(|_| Markdown::new(self.out.on())), wrap: width.map(|width| Wrap::new(width, indent, indent)) });
            if !self.compacting {
                crate::peers::farewell::writing();
            }
            if self.input.pinned() && !self.compacting {
                self.doing("Writing");
            }
        }
        if !self.compacting {
            crate::peers::farewell::wrote(delta);
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

    fn checking(&mut self, check: &rein_agent::check::Check) {
        self.doing(&format!("Checking with {}", check.name()));
    }

    /// `↳ cargo check · no new errors · 0.4s` under the step, or what broke:
    /// `✗ cargo check · 2 new errors · 0.6s` and the first few. Ctrl-O shows
    /// them all.
    fn checked(&mut self, checked: &rein_agent::check::Checked, printed: &str) {
        use rein_agent::check::Verdict;
        self.end_line();
        let s = self.err;
        let pad = " ".repeat(steps::INDENT);
        let said = check_said(checked);
        let line = match checked.verdict {
            Verdict::Clean | Verdict::Busy => s.dim(&format!("↳ {said}")),
            Verdict::New(_) => s.red(&format!("✗ {said}")),
            Verdict::Fixed => format!("{} {}", s.green("✓"), s.dim(&said)),
        };
        say!("{pad}{line}");
        let width = self.wrap_width().unwrap_or(80).saturating_sub(steps::INDENT + 2).max(20);
        for shown in checked.shown.iter().take(3) {
            let first = rein_agent::background::plain(shown.lines().next().unwrap_or_default());
            say!("{pad}  {}", s.dim(&crate::ui::style::fit(&first, width)));
        }
        if checked.shown.len() > 3 {
            say!("{pad}  {}", s.dim(&format!("… {} more · ctrl-o shows them", checked.shown.len() - 3)));
        }
        if self.input.pinned() && !printed.is_empty() {
            self.input.keep_output(format!("Check  {} · {}", checked.name, strip(&line).trim_start_matches(['↳', '✗', '✓', ' '])), printed);
        }
    }

    /// `↳ b1 npm run dev ended: exit 1`, where the model is told.
    fn background_ended(&mut self, ended: &rein_agent::background::Ended) {
        self.end_line();
        self.gap_before_step();
        let lead = self.lead();
        let command = rein_agent::background::plain(ended.command.lines().next().unwrap_or_default());
        let line = format!("↳ {} {command} ended: {}", ended.id, ended.exit.said());
        say!("{lead}{}", if ended.exit.failed() { self.err.red(&line) } else { self.err.dim(&line) });
    }

    fn meters(&mut self, meters: &[rein_llm::Meter]) {
        self.take_meters(meters.to_vec());
    }

    async fn approve(&mut self, call: &ToolCall, rule: Option<&Rule>) -> Approval {
        self.end_line();
        self.input.busy(None, std::time::Instant::now());
        self.step = None;
        crate::ui::screen::progress(Progress::Waiting);
        // An MCP server's tool, by its server's name and its own.
        let server_tool = rein_mcp::parts(&call.name).filter(|_| ToolName::of(call).is_none());
        let asking = Asked::of(call, rule, self.sandboxing);
        let Asked { question, always, reason, .. } = asking.clone();
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
        let answers = vec![("y", "Yes".to_owned()), ("a", always.to_owned()), ("n", "No".to_owned()), ("esc", INSTEAD.to_owned())];
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
            self.noted = Some(asking.wont_ask_again());
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
        self.step_started(call);
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
        self.step_blocked(call, reason);
        let (call_, lead_, status) = (call.clone(), lead.clone(), s.red(&format!("blocked: {reason}")));
        self.show(move |width| vec![steps::line(&call_, &lead_, &s.red("✗"), &status, s, width)]);
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
        self.turn.steps += 1;
        let asked = step.as_ref().is_some_and(|step| step.header_shown);
        let mut outcome = steps::outcome(call, output, s);
        self.step_finished(call, &outcome);
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
        let dot = steps::mark(outcome.ok, s);
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
        // Reads and searches in a row share a line, when there's nothing
        // to say under one.
        if ToolName::of(call).is_some_and(ToolName::only_looks)
            && outcome.ok == Some(true)
            && note.is_none()
            && hooked.is_empty()
            && let Some(lead) = lead.clone()
        {
            self.show_look(call, outcome.status, lead);
            self.last = Last::Step;
            return;
        }
        // An edit made without asking shows what it changed; a question
        // showed it already. A new file's whole text is Ctrl-O away.
        let change = if !asked && matches!(ToolName::of(call), Some(ToolName::Edit | ToolName::ApplyPatch)) && outcome.ok == Some(true) {
            steps::change(call, &self.root, true)
        } else {
            Vec::new()
        };
        // A change asked about shows what it changes above it; under, that
        // it's in.
        let saved = asked && ToolName::of(call).is_some_and(ToolName::changes_files) && outcome.ok == Some(true);
        let (call_, viewer) = (call.clone(), self.input.pinned());
        self.show(move |width| {
            let mut lines = vec![match &lead {
                Some(lead) => steps::line_saying(&call_, lead, &dot, &outcome.status, &outcome.said, s, width),
                None if saved => format!("{}{dot} {}", " ".repeat(steps::INDENT), s.dim("saved")),
                // Cut at the edge, like any of a step's lines.
                None => format!("{}{dot} {}{}", " ".repeat(steps::INDENT), outcome.status, outcome.said),
            }];
            lines.extend(steps::detail_lines(&outcome, s, width, viewer));
            lines.extend(watched.iter().map(|line| crate::ui::style::fit(line, width)));
            lines.extend(hooked.iter().map(|line| crate::ui::style::fit(line, width)));
            lines.extend(note.clone());
            lines.extend(steps::change_lines(&change, s, width));
            lines
        });
        self.last = Last::Step;
    }

    fn usage(&mut self, usage: &Usage) {
        self.tokens_used(usage);
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
                self.step_watched(changed);
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
            self.show_look(call, outcome.status, lead);
            return;
        }
        let dot = steps::mark(Some(outcome.ok != Some(false)), s);
        let (call_, viewer) = (call.clone(), self.input.pinned());
        self.show(move |width| {
            let mut lines = vec![steps::line(&call_, &lead, &dot, &outcome.status, s, width)];
            lines.extend(steps::detail_lines(&outcome, s, width, viewer));
            lines
        });
    }

    /// Your messages, then what came in from other reins. A sub-agent's
    /// report shows whole, here, as it goes in.
    fn steering(&mut self) -> Vec<Heard> {
        let mut heard: Vec<Heard> = self.input.steering().into_iter().map(Heard::You).collect();
        // Typed into a sub-agent while it works: it's yours now too.
        self.adopted |= !heard.is_empty();
        for one in self.take_incoming() {
            if self.fancy
                && let Some(shown) = crate::peers::talk::reported(&one, self.err, self.wrap_width().unwrap_or(80))
            {
                self.end_line();
                say!("\n{shown}");
            }
            heard.extend(self.heard_of(one));
        }
        heard
    }

    fn waiting(&mut self) -> Vec<Heard> {
        std::mem::take(&mut self.notes)
    }

    /// What was heard, where it went into the turn: between the steps it
    /// came between, so the screen reads as the model saw it.
    fn steered(&mut self, heard: &Heard) {
        self.end_line();
        let columns = self.wrap_width().unwrap_or(80);
        match heard {
            Heard::You(message) if self.fancy => show_you_said(&self.input.shown(message), self.err),
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
