//! One agent: its transcript, scrolled by message, and a box to write in.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::base::{TextView, TextViewState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Question, Role, Spot, Transcript, Update, extended};

use crate::agent::{Agent, Said};
use crate::probe::Probe;

/// How often a pane takes what its agent said: every chunk would be a
/// frame, and a frame shows them all anyway. VS Code's chat waits 50 ms.
const TAKE_EVERY: Duration = Duration::from_millis(33);
/// Text views kept: more can be made again, from the transcript.
const VIEWS_KEPT: usize = 256;

/// A row's text view, and what it holds now.
struct Kept {
    view: Entity<TextViewState>,
    shown: String,
}

pub(crate) struct Pane {
    transcript: Transcript,
    agent: Option<Agent>,
    scroller: Entity<MessageScrollerState>,
    pub(crate) input: Entity<InputState>,
    /// A view for each row that's been on screen lately.
    views: HashMap<usize, Kept>,
    newest: VecDeque<usize>,
    next_prompt: u64,
    /// How many messages make this conversation loaded, for a run to time.
    settles_at: Option<(usize, Probe)>,
    /// What's selected, held as places in the conversation: a row that has
    /// scrolled away takes the toolkit's selection with it, but not this.
    selected: Option<(Spot, Spot)>,
    /// A press began in the list, so its release may have selected something.
    pressed_in_list: bool,
    /// A turn is running, so what you write steers it.
    running: bool,
    /// What the agent asks, waiting for your answer. Nothing answers but you,
    /// and a step nobody can describe can't be allowed.
    asked: VecDeque<(serde_json::Value, Option<Question>)>,
}

impl Pane {
    pub(crate) fn new(mut agent: Option<Agent>, settles_at: Option<(usize, Probe)>, window: &mut Window, cx: &mut Context<Self>) -> Pane {
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Write to this agent"));
        cx.subscribe_in(&input, window, |pane, input, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let text = input.read(cx).value().to_string();
                input.update(cx, |input, cx| input.set_value("", window, cx));
                pane.send(&text, cx);
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
            next_prompt: 100,
            settles_at,
            selected: None,
            pressed_in_list: false,
            running: false,
            asked: VecDeque::new(),
        }
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
        self.next_prompt += 1;
        if std::mem::replace(&mut self.running, true) {
            // It says a steer back, once that reaches the turn.
            agent.steer(self.next_prompt, text);
        } else {
            agent.prompt(self.next_prompt, text);
            let update = self.transcript.you(text);
            self.show(update.added, update.touched, cx);
        }
    }

    /// Whether a question is waiting for you.
    pub(crate) fn is_asking(&self) -> bool {
        !self.asked.is_empty()
    }

    /// Answers the oldest question, as you pressed.
    fn answer(&mut self, allow: bool, cx: &mut Context<Self>) {
        if let (Some((id, _)), Some(agent)) = (self.asked.pop_front(), &mut self.agent) {
            agent.answer(&id, allow);
            cx.notify();
        }
    }

    /// Takes in what the agent said since the last time.
    fn take(&mut self, said: Vec<Said>, cx: &mut Context<Self>) {
        let (mut added, mut first, mut end) = (0, usize::MAX, 0);
        for one in said {
            let update = match one {
                Said::Update(update) => {
                    if let Some((_, probe)) = &self.settles_at {
                        probe.first_taken();
                    }
                    self.transcript.apply(&update)
                }
                Said::Heard { from, text } => self.transcript.heard(&from, &text),
                // Said in order, so the step it's about is in by now.
                Said::Asked { id, call } => {
                    self.asked.push_back((id, self.transcript.question(&call).cloned()));
                    Update::default()
                }
                Said::Started(session) => {
                    if let Some(agent) = &mut self.agent {
                        agent.started(session);
                    }
                    Update::default()
                }
                Said::TurnOver => {
                    self.running = false;
                    Update::default()
                }
                Said::Failed(why) => {
                    self.running = false;
                    self.transcript.note(&why)
                }
                // Nothing's left to answer what's asked.
                Said::Ended => {
                    self.running = false;
                    self.asked.clear();
                    self.transcript.note("This agent has ended.")
                }
            };
            added += update.added;
            if !update.touched.is_empty() {
                first = first.min(update.touched.start);
                end = end.max(update.touched.end);
            }
        }
        self.show(added, first..end, cx);
        if let Some((count, probe)) = &self.settles_at
            && self.transcript.len() >= *count
        {
            probe.settle_on_next_frame();
            self.settles_at = None;
        }
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

    /// Row `index`, which a shift-click takes what's selected to.
    fn row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let body = self.row_body(index, cx);
        let selected = self.selected_whole(index).then(|| cx.theme().selection);
        div()
            .id(("row", index))
            .when_some(selected, |row, color| row.bg(color))
            // On its way in: the text view keeps a press for itself.
            .capture_any_mouse_down(cx.listener(move |pane, event: &MouseDownEvent, _, cx| {
                if event.button == MouseButton::Left && event.modifiers.shift {
                    pane.extend_selection(index, cx);
                }
            }))
            .child(body)
            .into_any_element()
    }

    fn row_body(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.transcript.row(index) else { return div().into_any_element() };
        let Some(message) = self.transcript.get(row.message) else { return div().into_any_element() };
        let (role, status) = (message.role, message.status.clone());
        if role == Role::Step {
            let (title, output) = message.text.split_once('\n').unwrap_or((&message.text, ""));
            let tail: Vec<&str> = output.lines().rev().take(8).collect();
            let shown: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
            let line = format!("{title} · {}", status.unwrap_or_default());
            return div().px_3().py_1().text_sm().child(line).child(div().text_xs().child(shown)).into_any_element();
        }
        let Some(view) = self.view(index, cx) else { return div().into_any_element() };
        let text = TextView::new(&view).selectable(true);
        if row.block > 0 {
            return div().px_3().child(text).into_any_element();
        }
        let name = match role {
            Role::You => "You",
            Role::Agent => "rein",
            Role::Thought => "thinking",
            Role::Peer => "another rein",
            Role::Note => "this window",
            Role::Step => "",
        };
        div().px_3().pt_2().child(div().text_xs().child(name)).child(text).into_any_element()
    }
}

impl Render for Pane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = cx.weak_entity();
        let copy = self.selection_size().map(|size| {
            Button::new("copy").small().primary().label(format!("Copy selection ({size} bytes)")).on_click(cx.listener(|pane, _, _, cx| {
                if let Some(text) = pane.selection() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }))
        });
        let question = self.asked.front().map(|(_, question)| {
            let (title, detail) = question.as_ref().map_or(("a step it hasn't shown", None), |question| (question.title.as_str(), question.detail.clone()));
            let allow = question.is_some().then(|| Button::new("allow").small().primary().label("Allow").on_click(cx.listener(|pane, _, _, cx| pane.answer(true, cx))));
            let reject = Button::new("reject").small().label("Reject").on_click(cx.listener(|pane, _, _, cx| pane.answer(false, cx)));
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_1()
                .text_sm()
                .child(format!("rein asks to: {title}"))
                // However long: it scrolls, and never cuts what you'd allow.
                .when_some(detail, |asked, detail| asked.child(div().id("asked").max_h(px(180.)).overflow_y_scroll().text_xs().child(detail)))
                .child(div().flex().gap_2().children(allow).child(reject))
        });
        let list = MessageScroller::new("transcript", self.scroller.clone(), move |index, _, cx| {
            pane.update(cx, |pane, cx| pane.row(index, cx)).unwrap_or_else(|_| div().into_any_element())
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .size_full()
            .border_1()
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
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .capture_any_mouse_down(cx.listener(|pane, event: &MouseDownEvent, _, _| pane.pressed_in_list |= event.button == MouseButton::Left))
                    .child(list),
            )
            .children(copy)
            .children(question)
            .child(div().p_1().child(Input::new(&self.input)))
    }
}
