//! One agent: its transcript, scrolled by message, on a rail that runs down
//! into a box to write in.

mod foot;
mod rows;

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
use rein_transcript::{Choice, Modes, Question, Spot, Standing, Tick, Transcript, Update, Watch, extended, folder_name};
use serde_json::Value;

use crate::agent::{Agent, CameTo, Said, Stopping, SubAgent};
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
    Prompt(String),
    Steer,
    /// The mode it asks for: it's in that one only once it says so.
    Mode(String),
}

/// A question waiting for your answer.
struct Waiting {
    id: Value,
    /// What it's about: `None` for a step nobody can describe.
    question: Option<Question>,
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
        Pane {
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
            failure: None,
            since: Instant::now(),
            open_steps: HashSet::new(),
            beat: 0,
            beating: None,
            box_says: WRITE,
            card,
            keys_for: None,
            asked_at: Instant::now(),
        }
    }

    /// Sends what's in the box. With no agent to take it, it stays there.
    fn send_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent.is_none() {
            return;
        }
        let value = self.input.read(cx).value();
        let text = to_send(&value).to_owned();
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        self.send(&text, cx);
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
        let Some(agent) = self.agent.as_mut().filter(|_| !text.trim().is_empty()) else { return };
        self.next_request += 1;
        if std::mem::replace(&mut self.running, true) {
            // It says a steer back, once that reaches the turn.
            agent.steer(self.next_request, text);
            self.sent.insert(self.next_request, Sent::Steer);
        } else {
            agent.prompt(self.next_request, text);
            self.sent.insert(self.next_request, Sent::Prompt(text.to_owned()));
            self.watch.started();
            self.failure = None;
            let update = self.transcript.you(text);
            self.show(update.added, update.touched, cx);
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
        !self.asked.is_empty()
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

    /// A turn has ended, as it should.
    fn turn_ended(&mut self) -> Update {
        self.running = false;
        // What it was asking went with the turn.
        self.asked.clear();
        self.watch.turn_over(self.looking);
        self.transcript.turn_over()
    }

    /// A turn has failed, and this is why.
    fn turn_failed(&mut self, why: String, cx: &mut Context<Self>) -> Update {
        self.running = false;
        self.asked.clear();
        self.watch.failed();
        self.tell_parent(Err(&why), cx);
        let update = self.transcript.turn_over().and(self.transcript.note(&why));
        self.failure = Some(why);
        update
    }

    /// What came of the request sent under `id`.
    fn came_to(&mut self, id: u64, came_to: CameTo, cx: &mut Context<Self>) -> Update {
        match (self.sent.remove(&id), came_to) {
            // A turn nobody here prompted got there first: what you wrote
            // goes into it, as it would have if you'd seen it start.
            (Some(Sent::Prompt(text)), CameTo::Busy(_)) => {
                self.next_request += 1;
                if let Some(agent) = &mut self.agent {
                    agent.steer(self.next_request, &text);
                    self.sent.insert(self.next_request, Sent::Steer);
                }
                Update::default()
            }
            (Some(Sent::Prompt(_)), CameTo::Refused(why)) => self.turn_failed(why, cx),
            (Some(Sent::Prompt(_)), CameTo::TurnOver) => self.turn_ended(),
            (Some(Sent::Mode(mode)), CameTo::Done) => {
                if let Some(modes) = &mut self.modes {
                    modes.current = mode;
                }
                Update::default()
            }
            (Some(Sent::Mode(_)), CameTo::Busy(why) | CameTo::Refused(why)) => self.transcript.note(&format!("The mode stays as it was: {why}")),
            (Some(Sent::Steer), CameTo::Busy(why) | CameTo::Refused(why)) => self.transcript.note(&format!("That didn't reach it: {why}")),
            _ => Update::default(),
        }
    }

    /// Takes in what the agent said since the last time.
    pub(crate) fn take(&mut self, said: Vec<Said>, cx: &mut Context<Self>) {
        let (mut added, mut first, mut end) = (0, usize::MAX, 0);
        let before = self.listed();
        for one in said {
            let update = match one {
                Said::Update(update) => {
                    if let Some((_, probe)) = &self.about.settles_at {
                        probe.first_taken();
                    }
                    self.transcript.apply(&update)
                }
                Said::Heard { from, text } => self.transcript.heard(&from, &text),
                // Said in order, so the step it's about is in by now.
                Said::Asked { id, call, options } => {
                    let question = self.transcript.question(&call).cloned();
                    let choices = Choice::offered(&options, question.is_some());
                    if self.asked.is_empty() {
                        self.asked_at = Instant::now();
                    }
                    self.asked.push_back(Waiting { id, question, choices });
                    self.watch.asked();
                    Update::default()
                }
                Said::SubAgent { id, asked } => {
                    cx.emit(Told::SubAgent { id, asked });
                    Update::default()
                }
                Said::Modes(modes) => {
                    self.modes = Some(modes);
                    Update::default()
                }
                Said::Started(session) => {
                    self.about.session = Some(session.clone());
                    if let Some(agent) = &mut self.agent {
                        agent.started(session);
                    }
                    // What a saved conversation did isn't this turn's.
                    self.transcript.turn_started();
                    self.tell_parent(Ok(()), cx);
                    Update::default()
                }
                Said::TurnStarted => {
                    self.running = true;
                    self.failure = None;
                    self.transcript.turn_started();
                    self.watch.started();
                    Update::default()
                }
                Said::TurnOver => self.turn_ended(),
                Said::Failed(why) => self.turn_failed(why, cx),
                Said::Answer { id, came_to } => self.came_to(id, came_to, cx),
                // Nothing's left to answer what's asked, or to take what you
                // write: kept, it would read as working for good.
                Said::Ended => {
                    self.agent = None;
                    self.running = false;
                    self.asked.clear();
                    self.watch.gone();
                    self.tell_parent(Err("the sub-agent ended before it started"), cx);
                    self.transcript.turn_over().and(self.transcript.note("This agent has ended."))
                }
            };
            added += update.added;
            if !update.touched.is_empty() {
                first = first.min(update.touched.start);
                end = end.max(update.touched.end);
            }
        }
        self.show(added, first..end, cx);
        if let Some((count, probe)) = &self.about.settles_at
            && self.transcript.len() >= *count
        {
            probe.settle_on_next_frame();
            self.about.settles_at = None;
        }
        // The list only hears of what it shows. The window draws again with
        // any pane, but it's told to look again only when this has changed.
        let after = self.listed();
        if after.0 != before.0 {
            self.stands_anew(cx);
        } else if after != before {
            cx.emit(Told::Changed);
        }
    }

    /// What the list of agents shows of this one.
    fn listed(&self) -> (Standing, Vec<Tick>, String) {
        (self.standing(), self.ticks(), self.title())
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
        let foot = self.foot(window, cx);
        let header = self.header(cx);
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
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
            .on_key_down(cx.listener(|pane, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && pane.running {
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
                    .children(question),
            )
            .child(foot)
    }
}

impl Pane {
    /// Whatever follows from how it stands and needs a window to do, which
    /// this is the first to have, and which can't happen as it draws.
    fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let says = if self.agent.is_none() {
            "This agent has ended"
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
        if self.is_asking() && self.input.read(cx).value().is_empty() {
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
            .child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(self.folder().to_owned()))
            .child(crate::panes::drag_area("drag-pane").flex_1().h_full())
            .child(more)
    }
}

/// What a test of the window reaches for that nothing else needs.
#[cfg(test)]
impl Pane {
    pub(crate) const fn is_running(&self) -> bool {
        self.running
    }

    pub(crate) fn mode_for_test(&self) -> Option<String> {
        self.modes.as_ref().map(|modes| modes.current.clone())
    }

    /// Asks for `mode`, as the bar's menu does, and says under what number.
    pub(crate) fn ask_mode_for_test(&mut self, mode: &str) -> u64 {
        self.set_mode(mode);
        self.next_request
    }

    pub(crate) const fn last_request_for_test(&self) -> u64 {
        self.next_request
    }

    /// As if the question at the front had been up for long enough.
    pub(crate) fn pretend_waited_for_test(&mut self) {
        self.asked_at = Instant::now().checked_sub(foot::KEYS_AFTER).expect("the clock has run this long");
    }

    pub(crate) fn press_for_test(&mut self, drawn_for: &Value, chosen: &str, cx: &mut Context<Self>) {
        self.answer(drawn_for, chosen, cx);
    }

    pub(crate) fn question_count_for_test(&self) -> usize {
        self.asked.len()
    }

    /// Ends its agent, so a test can read all it was sent.
    pub(crate) fn finish_agent_for_test(&mut self) {
        if let Some(agent) = self.agent.take() {
            agent.finish();
        }
    }

    pub(crate) fn send_box_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send_box(window, cx);
    }

    pub(crate) const fn is_beating_for_test(&self) -> bool {
        self.beating.is_some()
    }
}

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
