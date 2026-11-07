//! One agent: its transcript, scrolled by message, on a rail that runs down
//! into a box to write in.

mod ask;
mod foot;
#[cfg(test)]
mod for_tests;
mod heard;
mod panel;
mod pickers;
mod review;
mod rows;
mod settings_page;
mod undo;
mod welcome;
mod write;

#[cfg(test)]
pub(crate) use panel::Tab;
pub(crate) use pickers::Picking;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui_kit::base::TextViewState;
use gpui_kit::component::IconName;
use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::*;
use rein_transcript::{Asking, Choice, Meter, Modes, OwnCommand, Question, Sets, Setting, Spot, Standing, Tick, Transcript, Watch, Words, extended, folder_name, says_back};
use serde_json::Value;

use crate::agent::{Agent, Attached, Stopping, SubAgent};
use crate::look::{Look, THREAD_FROM};
use crate::parts::{Press, STRIP, halo, lip};
use crate::probe::Probe;

/// How often a pane takes what its agent said: every chunk would be a
/// frame, and a frame shows them all anyway. VS Code's chat waits 50 ms.
const TAKE_EVERY: Duration = Duration::from_millis(33);
/// Text views kept: more can be made again, from the transcript.
const VIEWS_KEPT: usize = 256;
/// How often what shows work moves. Eight steps a second, as the design's
/// marks turn, is eight frames a second, and only while a turn runs: an
/// animation frame by frame would be 120.
const BEAT: Duration = Duration::from_millis(125);
/// Where the rail runs, in from a pane's left edge.
const RAIL: f32 = 31.;
/// The longest a box grows, in lines, before it scrolls.
const BOX_LINES: usize = 12;
/// What the box says when there's nothing in it.
const WRITE: &str = "Write to rein";
/// What it says while a command waits for your answer, which isn't a turn
/// to steer.
const ASKED_ABOVE: &str = "Answer above first";

/// A row's text view, and what it holds now.
struct Kept {
    view: Entity<TextViewState>,
    shown: String,
}

/// Who a pane is, before anything's been said.
#[derive(Default)]
pub(crate) struct About {
    /// What to call it. With none, it goes by the first thing you write.
    pub(crate) name: Option<String>,
    pub(crate) cwd: String,
    /// The saved conversation it's opening, if it is.
    pub(crate) session: Option<String>,
    /// How many messages make this conversation loaded, for a run to time.
    pub(crate) settles_at: Option<(usize, Probe)>,
    /// The pane that asked for this one, and what it asked under: it waits
    /// to hear this one has started.
    pub(crate) started_for: Option<(WeakEntity<Pane>, Value)>,
    /// The pane this is a sub-agent of, which it's listed under.
    pub(crate) parent: Option<WeakEntity<Pane>>,
    /// It's the pane to start with, on a desk with nothing else: drawn as
    /// the first run's box until something's sent from it.
    pub(crate) draft: bool,
}

/// What a pane tells the window.
pub(crate) enum Told {
    /// Something the list of agents shows has changed.
    Changed,
    /// You've gone to this pane.
    Looked,
    /// Its agent asks for a sub-agent, under this number.
    SubAgent {
        id: Value,
        asked: Option<SubAgent>,
    },
    Hide,
    Close,
    /// `/new`: another conversation in this folder, in this one's place.
    New,
    /// `/resume`: the saved conversations.
    Resume,
    /// The pane to start with has had something sent from it, and is a
    /// conversation like any other now.
    Begun,
    /// The pane to start with wants another folder.
    Folder,
    /// Go to the sub-agent this one started, by the name it gave it.
    OpenSubAgent(String),
    /// Your settings now keep this look: `light`, `dark` or `follow`.
    Look(String),
}

impl EventEmitter<Told> for Pane {}

/// Whether what the box did sends it. Shift-Enter says it pressed Enter too,
/// and it's a new line.
const fn sends(event: &InputEvent) -> bool {
    matches!(event, InputEvent::PressEnter { shift: false, .. })
}

/// What's sent of what's in the box: the Enter that sent it may have reached
/// the box as well.
fn to_send(value: &str) -> &str {
    value.trim_end_matches('\n')
}

/// What a request was for, until it's answered: what comes of it means
/// something else for each.
enum Sent {
    /// What you wrote, in case it has to go in another way.
    Prompt(String, Vec<Attached>),
    Steer(String),
    /// The mode it asks for: it's in that one only once it says so.
    Mode(String),
    /// What you sent from this folder before.
    Recall,
    /// What `@` can point at.
    Paths,
    Tasks,
    Room,
    /// A background command to stop, and a turn to undo, which rein says it
    /// did or didn't.
    Stopping,
    UndoTurns(Option<usize>),
    UndoPlan,
    Undoing,
    /// How much the conversation changed, for the header.
    Totals,
    /// What a span changed, with its lines.
    Changes(rein_transcript::changes::Span),
    /// What can be set, as the list of models comes.
    Models,
    /// A setting changing: rein takes nothing else until it has.
    Setting,
    /// Signing in, before there's a conversation.
    SignIn,
    /// The ways there are to sign in to more, for the list of models.
    Ways,
    /// Your settings, for the settings page; a key of them set to this, or
    /// taken out; a rule of yours added or taken out.
    Config,
    SetConfig(rein_transcript::config::Key, Option<String>),
    Rule,
}

/// A question waiting for your answer.
struct Waiting {
    id: Value,
    /// What it's about: `None` for a step nobody can describe.
    question: Option<Question>,
    /// How rein puts it, past the step.
    words: Words,
    choices: Vec<Choice>,
}

pub(crate) struct Pane {
    transcript: Transcript,
    agent: Option<Agent>,
    scroller: Entity<MessageScrollerState>,
    pub(crate) input: Entity<TextareaState>,
    /// A view for each row that's been on screen lately.
    views: HashMap<usize, Kept>,
    newest: VecDeque<usize>,
    next_request: u64,
    /// What's been asked of its agent and not yet answered, by number.
    sent: HashMap<u64, Sent>,
    about: About,
    watch: Watch,
    /// You're at this pane, so what ends in it has been seen.
    looking: bool,
    /// What's selected, held as places in the conversation: a row that has
    /// scrolled away takes the toolkit's selection with it, but not this.
    selected: Option<(Spot, Spot)>,
    /// A press began in the list, so its release may have selected something.
    pressed_in_list: bool,
    /// A turn is running, so what you write steers it.
    running: bool,
    /// What the agent asks, waiting for your answer. Nothing answers but you,
    /// and a step nobody can describe can't be allowed.
    asked: VecDeque<Waiting>,
    /// The modes its agent offers, if it says.
    modes: Option<Modes>,
    /// What can be set, as its agent last said.
    settings: Vec<Setting>,
    /// The branch it's on, and how much of your limits is used.
    branch: Option<String>,
    /// How many commands are running in the background.
    background: u64,
    meters: Vec<Meter>,
    /// Why its last turn failed.
    failure: Option<String>,
    /// When how it stands last changed.
    since: Instant,
    /// The steps whose output you've opened, by message.
    open_steps: HashSet<usize>,
    /// How far what shows work has moved, and what moves it while a turn runs.
    beat: u8,
    beating: Option<Task<()>>,
    /// What the box says when it's empty, which depends on how it stands.
    box_says: &'static str,
    /// What holds the keys while a question waits for them.
    card: FocusHandle,
    /// The question that was last offered them, and when.
    keys_for: Option<Value>,
    asked_at: Instant,
    /// What a command asks, once it's on screen, and until then.
    asking: Option<ask::Open>,
    to_ask: Option<(Value, Asking)>,
    /// What the box holds besides its text.
    writing: write::Writing,
    /// `^O`: every step shows its output, but those opened or closed by hand.
    all_open: bool,
    /// What's open beside the conversation, and what's to open, which
    /// takes a window.
    panel: Option<panel::Panel>,
    review: review::Review,
    to_panel: Option<panel::Tab>,
    /// The sheet that puts the project back.
    undoing: Option<undo::Sheet>,
    /// The model or mode list, over the box.
    picker: Option<pickers::Picker>,
    /// It's the pane to start with: see [`About::draft`].
    draft: bool,
    /// Signed in to nothing that runs the model: the ways there are.
    signing: Option<welcome::Signing>,
    /// The providers there are still to sign in to, in a few words, as the
    /// list of models offers them.
    more_ways: Option<String>,
    /// The settings page, while it's open over the window.
    page: Option<settings_page::Page>,
}

impl Pane {
    pub(crate) fn new(mut agent: Option<Agent>, about: About, window: &mut Window, cx: &mut Context<Self>) -> Pane {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, BOX_LINES).submit_on_enter(true).placeholder(WRITE));
        // Both look different with the keys in them, and this view is cached.
        let card = cx.focus_handle();
        for focus in [input.read(cx).focus_handle(cx), card.clone()] {
            cx.on_focus(&focus, window, |_, _, cx| cx.notify()).detach();
            cx.on_blur(&focus, window, |_, _, cx| cx.notify()).detach();
        }
        cx.subscribe_in(&input, window, |pane, _, event: &InputEvent, window, cx| {
            if sends(event) {
                pane.send_box(window, cx);
            } else if matches!(event, InputEvent::Change) {
                pane.box_changed(cx);
            }
        })
        .detach();
        if let Some(mut said) = agent.as_mut().and_then(|agent| agent.said.take()) {
            cx.spawn(async move |pane, cx| {
                // Woken by what the agent says, never by a clock: a pane
                // with nothing to show costs nothing.
                while let Some(first) = said.next().await {
                    cx.background_executor().timer(TAKE_EVERY).await;
                    let mut taken = vec![first];
                    while let Ok(one) = said.try_recv() {
                        taken.push(one);
                    }
                    if pane.update(cx, |pane, cx| pane.take(taken, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        let draft = about.draft;
        Pane {
            draft,
            signing: None,
            more_ways: None,
            transcript: Transcript::default(),
            agent,
            scroller,
            input,
            views: HashMap::new(),
            newest: VecDeque::new(),
            next_request: 100,
            sent: HashMap::new(),
            about,
            watch: Watch::default(),
            looking: false,
            selected: None,
            pressed_in_list: false,
            running: false,
            asked: VecDeque::new(),
            modes: None,
            settings: Vec::new(),
            branch: None,
            background: 0,
            meters: Vec::new(),
            failure: None,
            since: Instant::now(),
            open_steps: HashSet::new(),
            beat: 0,
            beating: None,
            box_says: WRITE,
            card,
            keys_for: None,
            asked_at: Instant::now(),
            asking: None,
            to_ask: None,
            writing: write::Writing::default(),
            all_open: false,
            panel: None,
            review: review::Review::default(),
            to_panel: None,
            undoing: None,
            picker: None,
            page: None,
        }
    }

    /// Sends what's in the box. With no agent to take it, it stays there.
    fn send_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent.is_none() {
            return;
        }
        // It waits for an answer, and what's in the box stays for after.
        if self.asking.is_some() {
            self.focus_asking(window, cx);
            return;
        }
        if let Some((text, images)) = self.take_box(window, cx) {
            if images.is_empty() {
                self.run_chosen(&text, window, cx);
            } else {
                self.run(&text, images, cx);
            }
        }
    }

    /// Does what `text` says: what's the window's own, what sets something,
    /// or it goes to rein. While a turn runs, a command and what comes with
    /// an image go after it. While a setting changes everything does: rein
    /// would say it's busy, and no turn would come to end what it was sent.
    fn run(&mut self, text: &str, images: Vec<Attached>, cx: &mut Context<Self>) {
        // A panel only looks: it has no need to wait for anything.
        let panel = matches!(OwnCommand::of(text), Some(OwnCommand::Tasks | OwnCommand::Changes));
        if !panel && (self.setting_changing() || self.running && (says_back(text) || !images.is_empty())) {
            self.writing.waits(text.to_owned(), images);
            cx.notify();
            return;
        }
        match OwnCommand::of(text) {
            Some(OwnCommand::New) => cx.emit(Told::New),
            Some(OwnCommand::Resume) => cx.emit(Told::Resume),
            Some(OwnCommand::Exit) => cx.emit(Told::Close),
            Some(OwnCommand::Copy) => self.copy_last_answer(cx),
            Some(OwnCommand::Undo) => self.open_undo(None, cx),
            Some(OwnCommand::Tasks) => self.open_panel(panel::Tab::Tasks, cx),
            Some(OwnCommand::Changes) => self.open_panel(panel::Tab::Changes, cx),
            None => match Sets::of(text).filter(|sets| images.is_empty() && self.setting(sets.option).is_some()) {
                Some(sets) => self.set_option(sets, cx),
                None => self.send_with(text, images, cx),
            },
        }
    }

    /// Whether a setting is on its way and rein hasn't said it's done.
    fn setting_changing(&self) -> bool {
        self.sent.values().any(|sent| matches!(sent, Sent::Setting))
    }

    /// Changes a setting. Nothing's written: the bar shows what it comes to,
    /// and only what stops it is said.
    fn set_option(&mut self, sets: Sets<'_>, cx: &mut Context<Self>) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.set_option(self.next_request, sets.option, sets.value)) {
            self.sent.insert(self.next_request, Sent::Setting);
        }
        cx.notify();
    }

    /// `/copy`: the last answer, as it was written.
    fn copy_last_answer(&mut self, cx: &mut Context<Self>) {
        let said = match self.transcript.last_answer() {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                "Copied rein's last answer."
            }
            None => "Nothing to copy yet: rein hasn't answered here.",
        };
        let update = self.transcript.note(said);
        self.show(update.added, update.touched, cx);
    }

    /// Moves what shows work, for as long as a turn runs and isn't waiting
    /// for you: it starts here and stops by being dropped.
    fn keep_beat(&mut self, cx: &mut Context<Self>) {
        if !self.running || self.is_asking() {
            self.beating = None;
        } else if self.beating.is_none() {
            self.beating = Some(cx.spawn(async move |pane, cx| {
                loop {
                    cx.background_executor().timer(BEAT).await;
                    let beat = pane.update(cx, |pane, cx| {
                        pane.beat = pane.beat.wrapping_add(1);
                        cx.notify();
                    });
                    if beat.is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// What's asked, in words for the list: `wants to run git push`.
    pub(crate) fn wanting(&self) -> Option<String> {
        let question = self.asked.front()?.question.as_ref()?;
        if question.title.starts_with("Sub-agent ") {
            return Some("wants to start a sub-agent".to_owned());
        }
        let mut letters = question.title.chars();
        let first = letters.next()?;
        Some(format!("wants to {}{}", first.to_lowercase(), letters.as_str()))
    }

    /// Why its last turn failed.
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// How long it's stood as it does.
    pub(crate) fn age(&self) -> Duration {
        self.since.elapsed()
    }

    /// This turn's latest steps.
    pub(crate) fn ticks(&self) -> Vec<Tick> {
        self.transcript.ticks(STRIP)
    }

    /// The conversation it holds, once it's known.
    pub(crate) fn session(&self) -> Option<&str> {
        self.about.session.as_deref()
    }

    /// The folder its agent works in, whole.
    pub(crate) fn cwd(&self) -> &str {
        &self.about.cwd
    }

    /// The pane this is a sub-agent of.
    pub(crate) fn parent(&self) -> Option<&WeakEntity<Pane>> {
        self.about.parent.as_ref()
    }

    /// What it's called in the list.
    pub(crate) fn title(&self) -> String {
        self.about.name.as_deref().or_else(|| self.transcript.first_said()).unwrap_or("New conversation").to_owned()
    }

    /// The name of the folder its agent works in.
    pub(crate) fn folder(&self) -> &str {
        folder_name(&self.about.cwd)
    }

    pub(crate) const fn standing(&self) -> Standing {
        self.watch.standing()
    }

    pub(crate) const fn is_looking(&self) -> bool {
        self.looking
    }

    /// You've come to this pane, or gone to another.
    /// You're at this pane.
    pub(crate) const fn is_looked_at(&self) -> bool {
        self.looking
    }

    /// What can be run here, as its agent last said.
    pub(crate) fn commands(&self) -> &[rein_transcript::Command] {
        self.transcript.commands()
    }

    /// Runs what you chose or typed. What has a list of its own opens it:
    /// the effort is with the model, and the sandbox with the mode. What
    /// says how full the conversation is, or makes room, opens the panel
    /// that shows it, where Compact asks what to keep in mind.
    pub(crate) fn run_chosen(&mut self, line: &str, window: &mut Window, cx: &mut Context<Self>) {
        match line.trim() {
            "/model" | "/effort" if self.setting("model").is_some() => self.open_picker(Picking::Model, window, cx),
            "/mode" | "/sandbox" if self.modes.is_some() => self.open_picker(Picking::Mode, window, cx),
            // How full the conversation is, and making room, are the Context panel's.
            "/status" | "/compact" if self.agent.is_some() => {
                if !self.panel_shows(panel::Tab::Context) {
                    self.open_panel(panel::Tab::Context, cx);
                }
            }
            _ => self.run(line, Vec::new(), cx),
        }
    }

    pub(crate) fn set_looking(&mut self, looking: bool, cx: &mut Context<Self>) {
        self.looking = looking;
        if looking {
            self.watch.seen();
        }
        cx.notify();
    }

    /// Says the sub-agent asked for under `id` can't be started.
    pub(crate) fn refuse(&mut self, id: &Value, why: &str) {
        if let Some(agent) = &mut self.agent {
            agent.refuse(id, why);
        }
    }

    /// Its agent couldn't be started, and this is why.
    pub(crate) fn couldnt_start(&mut self, why: &str, cx: &mut Context<Self>) {
        let update = self.transcript.note(why);
        self.show(update.added, update.touched, cx);
        self.watch.failed();
        self.tell_parent(Err(why), cx);
    }

    /// It's being closed: whichever pane asked for it stops waiting.
    pub(crate) fn closing(&mut self, cx: &mut Context<Self>) {
        self.tell_parent(Err("its pane was closed"), cx);
    }

    /// Whichever pane asked for this one hears how it went, once.
    fn tell_parent(&mut self, outcome: Result<(), &str>, cx: &mut Context<Self>) {
        let Some((parent, id)) = self.about.started_for.take() else { return };
        let _ = parent.update(cx, |parent, _| {
            if let Some(agent) = &mut parent.agent {
                match outcome {
                    Ok(()) => agent.opened(&id),
                    Err(why) => agent.refuse(&id, why),
                }
            }
        });
    }

    /// Reads what the toolkit has selected in the rows on screen, as places
    /// in the conversation. A press that selected nothing clears it, unless
    /// it's what `extended` it.
    pub(crate) fn note_selection(&mut self, extended: bool, cx: &mut Context<Self>) {
        if extended {
            return;
        }
        let mut spots: Vec<(usize, std::ops::Range<usize>)> =
            self.views.iter().filter_map(|(row, kept)| kept.view.read(cx).selected_source_range().filter(|range| !range.is_empty()).map(|range| (*row, range))).collect();
        spots.sort_by_key(|(row, _)| *row);
        if let (Some(first), Some(last)) = (spots.first(), spots.last()) {
            // What's shown isn't always what was said, and this is held as that.
            self.selected = Some((self.transcript.spot(first.0, first.1.start, 0), self.transcript.spot(last.0, last.1.end, usize::MAX)));
            cx.notify();
        } else if self.selected.take().is_some() {
            cx.notify();
        }
    }

    /// Shift-click: takes what's selected as far as the end of row `row`,
    /// which needn't have been near it.
    fn extend_selection(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected {
            self.selected = Some(extended(selected, row));
            cx.notify();
        }
    }

    /// What's selected, read from what was said.
    pub(crate) fn selection(&self) -> Option<String> {
        self.selected.map(|(from, to)| self.transcript.between(from, to))
    }

    /// Row `row` is between the ends of what's selected, so it's whole.
    fn selected_whole(&self, row: usize) -> bool {
        self.selected.is_some_and(|(a, b)| a.row.min(b.row) < row && row < a.row.max(b.row))
    }

    /// Sends `text` to the agent, as if it were typed: it starts a turn,
    /// or steers the one that's running.
    pub(crate) fn send(&mut self, text: &str, cx: &mut Context<Self>) {
        self.send_with(text, Vec::new(), cx);
    }

    /// The same, with `images`.
    fn send_with(&mut self, text: &str, images: Vec<Attached>, cx: &mut Context<Self>) {
        let Some(agent) = self.agent.as_mut().filter(|_| !text.trim().is_empty()) else { return };
        self.next_request += 1;
        if std::mem::replace(&mut self.running, true) {
            // It says a steer back, once that reaches the turn.
            agent.steer(self.next_request, text);
            self.sent.insert(self.next_request, Sent::Steer(text.to_owned()));
            self.writing.steering(text);
            cx.notify();
        } else {
            agent.prompt(self.next_request, text, &images);
            self.sent.insert(self.next_request, Sent::Prompt(text.to_owned(), images));
            self.begun(cx);
            self.watch.started();
            self.failure = None;
            // A command's line may hold a key: rein says it back without it.
            if !says_back(text) {
                let update = self.transcript.you(text);
                self.show(update.added, update.touched, cx);
            }
            self.stands_anew(cx);
        }
    }

    /// How it stands has changed, so the list hears, and the beat follows.
    fn stands_anew(&mut self, cx: &mut Context<Self>) {
        self.since = Instant::now();
        self.keep_beat(cx);
        cx.emit(Told::Changed);
    }

    /// Stops the turn that's running. What ends it says so, unless it never
    /// began, and then there's nothing to: it's said here.
    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(agent) = &mut self.agent else { return };
        self.writing.bring_back();
        cx.notify();
        if agent.stop() == Stopping::NeverBegan {
            self.running = false;
            self.watch.turn_over(self.looking);
            let update = self.transcript.turn_over().and(self.transcript.note("Stopped before it began."));
            self.show(update.added, update.touched, cx);
            self.stands_anew(cx);
        }
    }

    /// Whether a question is waiting for you.
    pub(crate) fn is_asking(&self) -> bool {
        !self.asked.is_empty() || self.asking.is_some() || self.to_ask.is_some()
    }

    /// Answers the question `drawn_for`, as you pressed. Only if it's the one
    /// waiting, and has waited: of a double click on Allow, the second press
    /// lands where the next question's Allow is, on what nobody has read.
    fn answer(&mut self, drawn_for: &Value, chosen: &str, cx: &mut Context<Self>) {
        let waited = self.asked_at.elapsed();
        if self.asked.front().is_none_or(|waiting| waiting.id != *drawn_for) || waited < foot::KEYS_AFTER {
            return;
        }
        if let (Some(waiting), Some(agent)) = (self.asked.pop_front(), &mut self.agent) {
            agent.answer(&waiting.id, chosen);
            self.asked_at = Instant::now();
            self.watch.answered();
            self.stands_anew(cx);
            cx.notify();
        }
    }

    /// What can be set has changed. The mode's names go with it: what's
    /// marked with a ⚠ depends on the sandbox.
    fn settings_changed(&mut self, settings: Vec<Setting>) {
        if let (Some(modes), Some(mode)) = (&mut self.modes, settings.iter().find(|setting| setting.id == "mode")) {
            modes.current.clone_from(&mode.current);
            for choice in &mode.choices {
                if let Some(known) = modes.available.iter_mut().find(|known| known.id == choice.value) {
                    known.name.clone_from(&choice.name);
                    // What each lets through depends on the sandbox too.
                    if !choice.about.is_empty() {
                        known.about.clone_from(&choice.about);
                    }
                }
            }
        }
        self.settings = settings;
    }

    fn setting(&self, id: &str) -> Option<&Setting> {
        self.settings.iter().find(|setting| setting.id == id)
    }

    /// How much of your limits is used, as its agent last said.
    pub(crate) fn meters(&self) -> &[Meter] {
        &self.meters
    }

    /// Brings the list up to `added` rows more, and rows `changed` as they are now.
    fn show(&mut self, added: usize, changed: Range<usize>, cx: &mut Context<Self>) {
        for row in changed.clone() {
            self.sync(row, cx);
        }
        self.scroller.update(cx, |scroller, cx| {
            if added > 0 {
                scroller.append(added, cx);
            }
            if !changed.is_empty() {
                scroller.remeasure_items(changed, cx);
            }
        });
        cx.notify();
    }

    /// How much is selected, if it's more than the toolkit can hold.
    fn selection_size(&self) -> Option<usize> {
        self.selected.filter(|(a, b)| a.row != b.row).and_then(|_| self.selection()).map(|text| text.len())
    }

    /// Brings the view of row `row`, if it has one, up to its text: what's
    /// added is added, so only what's new is read again.
    fn sync(&mut self, row: usize, cx: &mut Context<Self>) {
        let (Some(kept), Some(text)) = (self.views.get_mut(&row), self.transcript.row_text(row)) else { return };
        if let Some(more) = text.strip_prefix(kept.shown.as_str()) {
            if !more.is_empty() {
                kept.view.update(cx, |view, cx| view.push_str(more, cx));
                kept.shown.push_str(more);
            }
        } else {
            kept.view.update(cx, |view, cx| view.set_text(&text, cx));
            kept.shown = text.into_owned();
        }
    }

    /// The view of row `index`, made if it isn't kept.
    fn view(&mut self, index: usize, cx: &mut Context<Self>) -> Option<Entity<TextViewState>> {
        if let Some(kept) = self.views.get(&index) {
            return Some(kept.view.clone());
        }
        let text = self.transcript.row_text(index)?.into_owned();
        let view = cx.new(|cx| TextViewState::markdown(&text, cx));
        self.views.insert(index, Kept { view: view.clone(), shown: text });
        self.newest.push_back(index);
        if self.newest.len() > VIEWS_KEPT
            && let Some(oldest) = self.newest.pop_front()
        {
            self.views.remove(&oldest);
        }
        Some(view)
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.settle(window, cx);
        if self.page.is_some() {
            return self.page_view(cx);
        }
        // Signed out, any pane shows the ways in, not only the first run's:
        // a pane started with ⌘N would otherwise wait for good.
        if self.signing.is_some() && !self.draft {
            let look = Look::of(cx);
            let ways = self.welcome(window, cx);
            let asking = self.asking_card(window, cx);
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(72.))
                .px(px(20.))
                .rounded(px(10.))
                .bg(look.bg)
                .shadow(vec![halo(look.line, 1.)])
                .child(div().relative().w_full().max_w(px(620.)).flex().flex_col().gap(px(16.)).child(ways).children(asking))
                .into_any_element();
        }
        if self.draft {
            let welcome = self.welcome(window, cx);
            let picker = self.picker_card(cx);
            let asking = self.asking_card(window, cx);
            return div().relative().w_full().flex().flex_col().gap(px(16.)).child(welcome).children(asking).children(picker).into_any_element();
        }
        let look = Look::of(cx);
        let pane = cx.weak_entity();
        let list = MessageScroller::new("transcript", self.scroller.clone(), move |index, _, cx| {
            pane.update(cx, |pane, cx| pane.row(index, cx)).unwrap_or_else(|_| div().into_any_element())
        })
        // Rows keep their own distance, which depends on what's above them.
        .with_list_style(StyleRefinement::default().pt(px(0.)).pb(px(0.)))
        .with_row_style(StyleRefinement::default().px(px(0.)).pb(px(0.)));
        let copy = self.selection_size().map(|size| {
            let pressed = cx.listener(|pane, _, _, cx| {
                if let Some(text) = pane.selection() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            });
            div().flex().justify_end().px_3().py_1().child(Press::new("copy").small().label(format!("Copy selection ({size} bytes)")).on_click(pressed))
        });
        let tail = self.running.then(|| {
            let fade = linear_gradient(180., linear_color_stop(Hsla { a: 0., ..rgb(THREAD_FROM).into() }, 0.), linear_color_stop(rgb(THREAD_FROM), 1.));
            div().absolute().left(px(RAIL)).bottom_0().w(px(1.5)).h(px(72.)).bg(fade)
        });
        let question = self.question(window, cx);
        let asking = self.asking_card(window, cx);
        let listing = self.listing(cx);
        let waiting = self.waiting(cx);
        let foot = self.foot(window, cx);
        let header = self.header(cx);
        let beside = self.beside(cx);
        let sheet = self.undo_sheet(cx);
        let picker = self.picker_card(cx);
        let sheet_of_the_conversation = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w_0()
            .rounded(px(10.))
            .bg(self.sheet(&look))
            .shadow(if self.looking { vec![halo(look.ring, 1.), lip(look.lip)] } else { vec![halo(look.line, 1.)] })
            // On its way in, before anything inside keeps the press.
            .capture_any_mouse_down(cx.listener(|pane, _: &MouseDownEvent, _, cx| {
                if !pane.looking {
                    cx.emit(Told::Looked);
                }
            }))
            .on_action(cx.listener(|pane, _: &crate::panes::NextMode, _, _| {
                if !pane.running && pane.asking.is_none() {
                    pane.next_mode();
                }
            }))
            .on_action(cx.listener(|pane, _: &crate::panes::Complete, window, cx| pane.tab(window, cx)))
            .on_action(cx.listener(|pane, _: &crate::panes::FindSent, window, cx| pane.find_sent(window, cx)))
            .on_action(cx.listener(|pane, _: &crate::panes::AllOutput, _, cx| pane.all_output(cx)))
            .on_action(cx.listener(|pane, _: &crate::panes::ToBackground, window, cx| pane.send_to_background(window, cx)))
            .on_drop(cx.listener(|pane, paths: &ExternalPaths, window, cx| pane.add_paths(paths.paths().to_vec(), window, cx)))
            .on_key_down(cx.listener(|pane, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && pane.running && pane.asking.is_none() {
                    pane.stop(cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|pane, event: &MouseUpEvent, window, cx| {
                    // Only what began in the list: a press of Copy, or in the
                    // box, selects nothing, and mustn't clear what's selected.
                    if std::mem::take(&mut pane.pressed_in_list) {
                        // After the toolkit has taken the same event in.
                        let shift = event.modifiers.shift;
                        cx.defer_in(window, move |pane, _, cx| pane.note_selection(shift, cx));
                    }
                }),
            )
            .child(header)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(div().absolute().left(px(RAIL)).top_0().bottom_0().w(px(1.)).bg(look.line_2))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .capture_any_mouse_down(cx.listener(|pane, event: &MouseDownEvent, _, _| pane.pressed_in_list |= event.button == MouseButton::Left))
                            .child(list),
                    )
                    .children(tail)
                    .children(copy)
                    .children(question)
                    .children(asking)
                    .children(listing)
                    .children(waiting),
            )
            .child(foot)
            .children(picker)
            .children(sheet);
        div().size_full().min_w_0().flex().gap(px(6.)).child(sheet_of_the_conversation).children(beside).into_any_element()
    }
}

impl Pane {
    /// Whatever follows from how it stands and needs a window to do, which
    /// this is the first to have, and which can't happen as it draws.
    fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((id, asking)) = self.to_ask.take() {
            self.open_asking(id, asking, window, cx);
        }
        self.settle_box(window, cx);
        self.settle_panel(window, cx);
        self.settle_review(window);
        self.settle_undo(window, cx);
        // What held the keys has gone, and took them with it: at a pane
        // you're at, there's always somewhere for what you type to go.
        if self.looking && window.focused(cx).is_none() {
            cx.defer_in(window, |pane, window, cx| {
                if window.focused(cx).is_none() {
                    pane.input.update(cx, |input, cx| input.focus(window, cx));
                }
            });
        }
        let says = if self.agent.is_none() {
            "This agent has ended"
        } else if self.draft && self.asking.is_none() {
            welcome::DESCRIBE
        } else if self.asking.is_some() {
            ASKED_ABOVE
        } else if self.running {
            "Steer it"
        } else {
            WRITE
        };
        if self.box_says != says {
            self.box_says = says;
            cx.defer_in(window, move |pane, window, cx| pane.input.update(cx, |input, cx| input.set_placeholder(says, window, cx)));
        }
        let front = self.asked.front().map(|waiting| waiting.id.clone());
        if front != self.keys_for {
            self.keys_for.clone_from(&front);
            self.asked_at = Instant::now();
            if front.is_some() {
                if self.looking {
                    cx.defer_in(window, |pane, window, cx| pane.take_keys(window, cx));
                }
            } else if self.card.is_focused(window) {
                cx.defer_in(window, |pane, window, cx| pane.input.update(cx, |input, cx| input.focus(window, cx)));
            }
        }
    }

    /// You've come here, or a question has: the keys go to what's waiting,
    /// unless you've something half-written, which they stay with.
    pub(crate) fn take_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.half_written_beside(window, cx) {
            return;
        }
        if self.asking.is_some() {
            self.focus_asking(window, cx);
        } else if self.is_asking() && self.input.read(cx).value().is_empty() {
            self.card.focus(window, cx);
        } else {
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// You pressed here and the keys may not have followed: a conversation
    /// doesn't take them, and they'd stay in the pane you left. What you
    /// typed next would go to the wrong agent.
    pub(crate) fn keys_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input.read(cx).focus_handle(cx).is_focused(window) && !self.card.is_focused(window) {
            self.take_keys(window, cx);
        }
    }

    /// A pane has the colour of paper, a shade brighter when it has your keys.
    const fn sheet(&self, look: &Look) -> Hsla {
        if self.looking { look.bg } else { look.pane_2 }
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        let hide = cx.weak_entity();
        let close = hide.clone();
        let more = Press::new("more").ghost().square().named("More").child(gpui_kit::component::Icon::new(IconName::Ellipsis).size_4()).dropdown_menu(move |menu, _, _| {
            let (hide, close) = (hide.clone(), close.clone());
            menu.item(PopupMenuItem::new("Hide").on_click(move |_, _, cx| {
                let _ = hide.update(cx, |_, cx| cx.emit(Told::Hide));
            }))
            .item(PopupMenuItem::new("Close").on_click(move |_, _, cx| {
                let _ = close.update(cx, |_, cx| cx.emit(Told::Close));
            }))
        });
        let (weight, title) = if self.looking { (FontWeight::SEMIBOLD, look.ink) } else { (FontWeight::MEDIUM, look.ink_2) };
        let changes = self.changes_button(cx);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(43.))
            .pl(px(20.))
            .pr(px(8.))
            .border_b_1()
            .border_color(look.line)
            .child(div().min_w_0().truncate().font_weight(weight).text_color(title).child(self.title()))
            .child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(match &self.branch {
                Some(branch) => format!("{} · {branch}", self.folder()),
                None => self.folder().to_owned(),
            }))
            .child(crate::panes::drag_area("drag-pane").flex_1().h_full())
            .children(changes)
            .child(more)
    }
}

/// What a test of the window reaches for that nothing else needs.
#[cfg(test)]
mod tests {
    use super::{InputEvent, sends, to_send};

    #[test]
    fn enter_sends_and_shift_enter_is_a_new_line() {
        assert!(sends(&InputEvent::PressEnter { secondary: false, shift: false }));
        assert!(!sends(&InputEvent::PressEnter { secondary: false, shift: true }));
    }

    #[test]
    fn what_the_enter_left_isnt_sent_and_lines_you_wrote_are() {
        assert_eq!(to_send("hi\n"), "hi");
        assert_eq!(to_send("one\ntwo\n"), "one\ntwo");
    }
}
