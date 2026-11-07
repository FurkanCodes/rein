//! What's beside a conversation: what's taking room in it, and what's going
//! on behind it. All of it is rein's word, asked for as it's shown, and a
//! panel that's closed costs nothing.

use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::boards::{PlanState, Room, Task as Background, tokens};
use serde_json::Value;

use super::{Pane, Sent};
use crate::look::Look;
use crate::parts::{Press, working};

/// How wide a panel is.
const WIDE: f32 = 400.;
/// How often the commands in the background are asked after while they show.
const TASKS_EVERY: Duration = Duration::from_secs(1);

/// Which part it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Changes,
    Context,
    Tasks,
}

/// A panel that's open.
pub(super) struct Panel {
    tab: Tab,
    room: Option<Room>,
    tasks: Vec<Background>,
    /// What the summary should keep in mind.
    focus: Entity<InputState>,
    /// Asks after the background: stops by being dropped.
    watching: Option<Task<()>>,
}

impl Pane {
    /// Whether `tab` is what's showing beside the conversation.
    pub(super) fn panel_shows(&self, tab: Tab) -> bool {
        self.panel.as_ref().is_some_and(|panel| panel.tab == tab)
    }

    /// Whether you're partway through writing something beside the
    /// conversation. A question that comes up leaves the keys there: a `y`
    /// meant for your comment would say yes to something you never read.
    pub(super) fn half_written_beside(&self, window: &Window, cx: &App) -> bool {
        let underway = |input: &Entity<InputState>| {
            let input = input.read(cx);
            input.focus_handle(cx).is_focused(window) && !input.value().trim().is_empty()
        };
        self.panel.iter().map(|panel| &panel.focus).chain(self.comment_box()).any(underway)
    }

    /// Opens `tab`, or closes it if that's what's showing.
    pub(super) fn open_panel(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.panel.as_ref().is_some_and(|panel| panel.tab == tab) {
            self.panel = None;
            cx.notify();
            return;
        }
        self.to_panel = Some(tab);
        cx.notify();
    }

    /// Opens what was asked for, which needs a window.
    pub(super) fn settle_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.to_panel.take() else { return };
        match &mut self.panel {
            Some(panel) => panel.tab = tab,
            None => {
                let focus = cx.new(|cx| InputState::new(window, cx).placeholder("Keep in mind (optional)"));
                cx.subscribe_in(&focus, window, |pane, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        pane.compact_with_focus(window, cx);
                    }
                })
                .detach();
                self.panel = Some(Panel { tab, room: None, tasks: Vec::new(), focus, watching: None });
            }
        }
        self.ask_room();
        if tab == Tab::Changes {
            self.ask_changes();
        }
        self.keep_watching(cx);
    }

    /// While the tasks show, they're asked after: what a command prints
    /// isn't news the agent sends.
    fn keep_watching(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.panel else { return };
        if panel.tab != Tab::Tasks {
            panel.watching = None;
            return;
        }
        if panel.watching.is_some() {
            return;
        }
        panel.watching = Some(cx.spawn(async move |pane, cx| {
            loop {
                let asked = pane.update(cx, |pane, _| pane.ask_tasks());
                if asked.is_err() {
                    break;
                }
                cx.background_executor().timer(TASKS_EVERY).await;
            }
        }));
    }

    fn ask_tasks(&mut self) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_tasks(self.next_request)) {
            self.sent.insert(self.next_request, Sent::Tasks);
        }
    }

    pub(super) fn tasks_came(&mut self, said: &Value, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.panel {
            panel.tasks = Background::listed(said);
            // One you stopped from here isn't news the agent sends.
            self.background = panel.tasks.iter().filter(|task| task.ended.is_none()).count() as u64;
            cx.notify();
        }
    }

    pub(super) fn room_came(&mut self, said: &Value, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.panel {
            panel.room = Room::of(said);
            cx.notify();
        }
    }

    fn stop_task(&mut self, id: &str) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.stop_task(self.next_request, id)) {
            self.sent.insert(self.next_request, Sent::Stopping);
        }
    }

    /// Makes room, with what you asked the summary to keep in mind.
    fn compact_with_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &self.panel else { return };
        let focus = panel.focus.read(cx).value().to_string();
        panel.focus.update(cx, |input, cx| input.set_value("", window, cx));
        let line = if focus.trim().is_empty() { "/compact".to_owned() } else { format!("/compact {}", focus.trim()) };
        self.run(&line, Vec::new(), cx);
    }

    /// What's beside the conversation, if anything is.
    pub(super) fn beside(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let panel = self.panel.as_ref()?;
        let look = Look::of(cx);
        let tab = |id: &'static str, name: &'static str, which: Tab, count: Option<usize>, cx: &mut Context<Self>| {
            let on = panel.tab == which;
            let count = count.filter(|&count| count > 0).map(|count| div().ml(px(5.)).font_weight(FontWeight::NORMAL).text_color(look.ink_3).child(count.to_string()));
            div()
                .id(id)
                .relative()
                .flex()
                .items_center()
                .h(px(44.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { look.ink } else { look.ink_3 })
                .hover(|style| style.text_color(look.ink))
                .on_click(cx.listener(move |pane, _, _, cx| {
                    if !pane.panel.as_ref().is_some_and(|panel| panel.tab == which) {
                        pane.open_panel(which, cx);
                    }
                }))
                .child(name)
                .children(count)
                .when(on, |it| it.child(div().absolute().left_0().right_0().bottom(px(-1.)).h(px(2.)).bg(look.ink)))
        };
        let close = Press::new("panel-close").ghost().square().named("Close the panel").child("×").on_click(cx.listener(|pane, _, _, cx| {
            pane.panel = None;
            cx.notify();
        }));
        let tabs = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(18.))
            .h(px(44.))
            .pl(px(16.))
            .pr(px(8.))
            .border_b_1()
            .border_color(look.line)
            .child(tab("tab-changes", "Changes", Tab::Changes, Some(self.files_changed()), cx))
            .child(tab("tab-context", "Context", Tab::Context, None, cx))
            .child(tab("tab-tasks", "Tasks", Tab::Tasks, Some(usize::try_from(self.background).unwrap_or(usize::MAX)), cx))
            .child(div().flex_1())
            .child(close);
        let body = match panel.tab {
            Tab::Changes => self.changes_tab(cx).into_any_element(),
            Tab::Context => self.context_tab(panel, cx).into_any_element(),
            Tab::Tasks => self.tasks_tab(panel, cx).into_any_element(),
        };
        Some(
            div()
                .flex_none()
                .w(px(WIDE))
                .h_full()
                .flex()
                .flex_col()
                .rounded(px(10.))
                .overflow_hidden()
                .bg(look.bg)
                .shadow(vec![crate::parts::halo(look.line, 1.)])
                .child(tabs)
                // The lines of a change scroll by themselves.
                .child(div().id("panel-body").flex_1().min_h_0().when(panel.tab != Tab::Changes, |it| it.overflow_y_scroll()).child(body)),
        )
    }

    fn context_tab(&self, panel: &Panel, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let Some(room) = &panel.room else {
            return div().p(px(16.)).text_color(look.ink_3).child("Asking rein…").into_any_element();
        };
        // Each kind a little fainter than the last, as there's one teal.
        let shade = |at: usize| [1., 0.78, 0.6, 0.46, 0.34, 0.24][at.min(5)];
        let bar = room.parts.iter().enumerate().map(|(at, part)| div().h_full().w(relative(room.share_of(part))).bg(look.teal).opacity(shade(at)));
        let rows = room.parts.iter().enumerate().map(|(at, part)| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .h(px(24.))
                .child(div().flex_none().size(px(8.)).rounded(px(2.)).bg(look.teal).opacity(shade(at)))
                .child(div().flex_1().min_w_0().truncate().child(part.name.clone()))
                .child(div().min_w_0().truncate().text_size(px(12.)).text_color(look.ink_3).child(part.about.clone()))
                .child(div().flex_none().w(px(44.)).text_right().child(tokens(part.tokens)))
        });
        let used = sect(&look)
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(6.))
                    .child(div().text_size(px(22.)).line_height(px(28.)).font_weight(FontWeight::SEMIBOLD).child(tokens(room.used)))
                    .child(div().text_color(look.ink_3).child(format!("of {}, when rein makes room", tokens(room.limit)))),
            )
            .child(div().flex().gap(px(2.)).h(px(8.)).rounded(px(4.)).overflow_hidden().bg(look.lit).children(bar))
            .child(div().flex().flex_col().gap(px(1.)).mt(px(2.)).children(rows));
        let big = (!room.biggest.is_empty()).then(|| {
            let rows = room.biggest.iter().map(|big| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(22.))
                    .child(div().flex_1().min_w_0().truncate().font_family(mono.clone()).text_size(px(12.)).child(big.what.clone()))
                    .when(big.stale, |row| row.child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child("stale")))
                    .child(div().flex_none().w(px(44.)).text_right().child(tokens(big.tokens)))
            });
            sect(&look).child(heading("Biggest in the conversation", Some("stale ones are retired first"), &look)).children(rows)
        });
        let cache = room.cache.map(|cache| {
            sect(&look).child(heading("Cache", None, &look)).child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(6.))
                    .child(div().text_size(px(22.)).line_height(px(28.)).font_weight(FontWeight::SEMIBOLD).child(format!("{cache}%")))
                    .child(div().text_color(look.ink_3).child("of the last request came from the cache")),
            )
        });
        let says = if room.retiring > 0 {
            format!("Retiring stale output frees about {}. A summary comes only if that isn't enough.", tokens(room.retiring))
        } else {
            "Nothing's stale, so the model writes a summary.".to_owned()
        };
        let compact = Press::new("compact").label("Compact").on_click(cx.listener(|pane, _, window, cx| pane.compact_with_focus(window, cx)));
        let room_now = sect(&look)
            .child(heading("Make room now", None, &look))
            .child(
                div()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .bg(look.bg)
                    .shadow(vec![crate::parts::hairline(look.ring)])
                    .child(Input::new(&panel.focus).appearance(false).text_size(px(13.))),
            )
            .child(
                div().flex().items_center().gap(px(8.)).child(div().flex_1().min_w_0().text_size(px(12.)).line_height(px(16.)).text_color(look.ink_3).child(says)).child(compact),
            );
        div().flex().flex_col().child(used).children(big).children(cache).child(room_now).into_any_element()
    }

    fn tasks_tab(&self, panel: &Panel, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let plan = self.transcript.plan();
        let done = plan.iter().filter(|step| step.state == PlanState::Done).count();
        let steps = plan.iter().map(|step| {
            let mark = match step.state {
                PlanState::Done => div().flex_none().w(px(15.)).text_color(look.ink_3).child("✓").into_any_element(),
                PlanState::Running => div().flex_none().w(px(15.)).child(working(11., look.teal, self.beat)).into_any_element(),
                PlanState::Waiting => div().flex_none().w(px(15.)).text_color(look.ink_3).child("·").into_any_element(),
            };
            div()
                .flex()
                .items_center()
                .gap(px(7.))
                .min_h(px(22.))
                .text_color(if step.state == PlanState::Waiting { look.ink_3 } else { look.ink_2 })
                .child(mark)
                .child(div().min_w_0().child(step.text.clone()))
        });
        let plan_part = (!plan.is_empty()).then(|| sect(&look).child(heading("Plan", Some(&format!("{done} of {}", plan.len())), &look)).children(steps));
        let tasks: Vec<AnyElement> = panel
            .tasks
            .iter()
            .enumerate()
            .map(|(at, task)| {
                let id = task.id.clone();
                let stop =
                    task.ended.is_none().then(|| Press::new(("stop-task", at)).ghost().small().label("Stop").on_click(cx.listener(move |pane, _, _, _| pane.stop_task(&id))));
                let about = task.ended.clone().unwrap_or_else(|| task.took());
                let mark = if task.ended.is_some() { None } else { Some(working(12., look.teal, self.beat)) };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(10.))
                    .pt(px(8.))
                    .pb(px(10.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(look.line)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .children(mark)
                            .child(div().flex_1().min_w_0().truncate().font_family(mono.clone()).text_size(px(12.)).font_weight(FontWeight::MEDIUM).child(task.command.clone()))
                            .child(div().flex_none().text_size(px(12.)).text_color(if task.failed { look.red } else { look.ink_3 }).child(about))
                            .children(stop),
                    )
                    .children(task.last.clone().map(|last| div().font_family(mono.clone()).text_size(px(11.5)).line_height(px(17.)).text_color(look.ink_3).child(last)))
                    .into_any_element()
            })
            .collect();
        let running = panel.tasks.iter().filter(|task| task.ended.is_none()).count();
        let background = sect(&look).child(heading("Background commands", Some(&running.to_string()), &look));
        let background = if tasks.is_empty() {
            background.child(
                div()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(look.ink_3)
                    .child("Nothing runs in the background. A server or a watcher goes here, and so does a command you press ^B on."),
            )
        } else {
            background.children(tasks).child(div().text_size(px(12.)).text_color(look.ink_3).child("They stop with the conversation."))
        };
        let check = self.check_part(cx);
        div().flex().flex_col().children(plan_part).child(background).children(check).into_any_element()
    }
    /// The check after edits: what it runs, how the last one went, and a
    /// switch, from before it ever runs. A project rein knows no check for
    /// has none to show.
    fn check_part(&self, cx: &mut Context<Self>) -> Option<Div> {
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let setting = self.setting("check")?;
        let on = setting.current == "on";
        let running = self.transcript.last_check().is_some_and(|(_, state)| state == rein_transcript::StepState::Running);
        let checked = self.transcript.last_checked();
        let (said, color) = match checked {
            _ if running => ("running".to_owned(), look.ink_3),
            Some(checked) => (checked.said(), if matches!(checked.verdict, rein_transcript::Verdict::New(_)) { look.red } else { look.ink_3 }),
            None if on => ("hasn't run yet".to_owned(), look.ink_3),
            None => ("off".to_owned(), look.ink_3),
        };
        let flip = cx.listener(move |pane, _: &ClickEvent, _, cx| pane.set_option(rein_transcript::Sets { option: "check", value: if on { "off" } else { "on" } }, cx));
        let before = checked.map_or(0, |checked| checked.before);
        let note = match before {
            0 => "Tells the model only what this turn broke.".to_owned(),
            1 => "Tells the model only what this turn broke. 1 error from before its first change isn't counted.".to_owned(),
            before => format!("Tells the model only what this turn broke. {before} errors from before its first change aren't counted."),
        };
        Some(
            sect(&look)
                .child(heading("Check after edits", None, &look))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().flex_1().min_w_0().truncate().font_family(mono).text_size(px(12.)).child(setting.command.clone().unwrap_or_default()))
                        .child(div().flex_none().text_size(px(12.)).text_color(color).child(said))
                        .child(super::pickers::switch("check-switch", on, &look).on_click(flip)),
                )
                .child(div().text_size(px(12.)).line_height(px(16.)).text_color(look.ink_3).child(note)),
        )
    }
}

/// A part of a panel.
fn sect(look: &Look) -> Div {
    div().flex().flex_col().gap(px(8.)).px(px(16.)).py(px(14.)).border_b_1().border_color(look.line)
}

/// What a part of a panel is called, and what's said to its right.
fn heading(name: &str, beside: Option<&str>, look: &Look) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(11.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(look.ink_3)
        .child(div().flex_1().child(name.to_owned()))
        .children(beside.map(|beside| div().font_weight(FontWeight::NORMAL).child(beside.to_owned())))
}

#[cfg(test)]
impl Pane {
    pub(crate) fn panel_tab_for_test(&self) -> Option<Tab> {
        self.panel.as_ref().map(|panel| panel.tab)
    }

    pub(crate) fn panel_room_for_test(&self) -> Option<Room> {
        self.panel.as_ref().and_then(|panel| panel.room.clone())
    }

    pub(crate) fn panel_tasks_for_test(&self) -> usize {
        self.panel.as_ref().map_or(0, |panel| panel.tasks.len())
    }

    pub(crate) fn open_panel_for_test(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.open_panel(tab, cx);
    }

    pub(crate) fn focus_panel_for_test(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &self.panel {
            panel.focus.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    pub(crate) fn stop_task_for_test(&mut self, id: &str) {
        self.stop_task(id);
    }
}
