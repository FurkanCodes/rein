//! The model and the mode, picked over the box from the bar. What's in them
//! is what rein says can be set, narrowed by the terminal's rule; a choice
//! goes through the command it would be typed as, so it's asked and kept the
//! same way wherever it's made.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Item, Menu, Setting};

use super::{Pane, Sent};
use crate::look::Look;
use crate::parts::hairline;

/// How wide each is, and how far from the box it sits.
const MODEL_WIDE: f32 = 420.;
const MODE_WIDE: f32 = 380.;
const FROM_BOX: f32 = 8.;
/// As many rows as show before the list scrolls, and the fewest: what's
/// around them, in each card, takes the rest of the room there is.
const LIST_MOST: f32 = 360.;
const LIST_LEAST: f32 = 96.;
const MODEL_AROUND: f32 = 250.;
const MODE_AROUND: f32 = 150.;

/// Where a list opens from the box it's for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Opens {
    /// Over it, from the top of a conversation's foot: anchored there, so a
    /// box that grows doesn't cover it.
    Above,
    /// Under it, on the first run, where the box sits near the top and a
    /// list over it would run off the window.
    Below,
}

/// Which list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Picking {
    Model,
    Mode,
}

/// A list on screen.
pub(super) struct Picker {
    pub(super) picking: Picking,
    /// What's typed to narrow the models. The modes are few enough.
    input: Entity<InputState>,
    /// Which of what fits is picked.
    at: usize,
    keys: FocusHandle,
}

/// One row of a list: what it runs, and what it shows.
struct Choice {
    run: String,
    name: String,
    about: String,
    group: Option<String>,
    current: bool,
}

/// How much of your limits is used, in a few words: `5h 28% · 7d 75%`.
fn meters_said(meters: &[rein_transcript::Meter]) -> Option<String> {
    let said: Vec<String> = meters.iter().filter_map(|meter| meter.used.map(|used| format!("{} {used}%", meter.label))).collect();
    (!said.is_empty()).then(|| said.join(" · "))
}

impl Pane {
    /// What the list holds, all of it, in order.
    fn choices(&self, picking: Picking) -> Vec<Choice> {
        match picking {
            Picking::Model => self.setting("model").map_or_else(Vec::new, |model| {
                let choices = model.choices.iter();
                choices
                    .map(|choice| Choice {
                        run: format!("/model {}", choice.value),
                        name: choice.name.clone(),
                        about: choice.about.clone(),
                        group: choice.group.clone(),
                        current: choice.value == model.current,
                    })
                    .collect()
            }),
            Picking::Mode => self.modes.as_ref().map_or_else(Vec::new, |modes| {
                let modes_now = modes.available.iter();
                modes_now
                    .map(|mode| Choice { run: format!("/mode {}", mode.id), name: mode.name.clone(), about: mode.about.clone(), group: None, current: mode.id == modes.current })
                    .collect()
            }),
        }
    }

    /// Those of `choices` that fit what's typed, in their order.
    fn fitting(picker: &Picker, choices: &[Choice], cx: &App) -> Vec<usize> {
        if picker.picking == Picking::Mode {
            return (0..choices.len()).collect();
        }
        let items = choices.iter().map(|choice| Item { name: choice.name.clone(), about: format!("{} {}", choice.group.as_deref().unwrap_or_default(), choice.about) }).collect();
        let menu = Menu { title: String::new(), items, current: None, at: None };
        let mut fits = menu.matching(&picker.input.read(cx).value());
        fits.sort_unstable();
        fits
    }

    /// Opens `picking`, or closes it. While a turn runs what you pick waits
    /// for its end, and the models are those known so far: rein takes no
    /// lookup until then.
    pub(super) fn open_picker(&mut self, picking: Picking, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker_by(picking, None, window, cx);
    }

    /// The same, by a press that went down `at`: one that closed this list
    /// on its way down, from its own cell, has done all it was for.
    pub(super) fn open_picker_by(&mut self, picking: Picking, at: Option<Point<Pixels>>, window: &mut Window, cx: &mut Context<Self>) {
        let shut = self.picker_shut.take();
        if at.is_some() && shut == at.map(|at| (picking, at)) {
            return;
        }
        if self.picker.as_ref().is_some_and(|picker| picker.picking == picking) {
            self.close_picker(window, cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find a model"));
        cx.subscribe_in(&input, window, |pane, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => pane.take_pick(window, cx),
            InputEvent::Change => {
                if let Some(picker) = &mut pane.picker {
                    picker.at = 0;
                }
                cx.notify();
            }
            _ => {}
        })
        .detach();
        let keys = cx.focus_handle();
        if picking == Picking::Model {
            input.update(cx, |input, cx| input.focus(window, cx));
            self.next_request += 1;
            if let Some(agent) = &mut self.agent {
                agent.ask_ways(self.next_request);
                self.sent.insert(self.next_request, Sent::Ways);
            }
            // The other providers' are looked up as the list opens.
            if !self.running {
                self.ask_models();
            }
        } else {
            keys.focus(window, cx);
        }
        let mut picker = Picker { picking, input, at: 0, keys };
        let choices = self.choices(picking);
        picker.at = Self::fitting(&picker, &choices, cx).iter().position(|&at| choices[at].current).unwrap_or(0);
        self.picker = Some(picker);
        cx.notify();
    }

    /// Asks every provider you're signed in to for its models.
    pub(super) fn ask_models(&mut self) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_models(self.next_request)) {
            self.sent.insert(self.next_request, Sent::Models);
        }
    }

    /// Signs in to more with a key, for a provider you choose: what it asks
    /// comes up as any question does, and nothing goes in the conversation.
    /// `/login` typed was written there as yours, and on the first run it
    /// started a conversation.
    fn sign_in_more(&mut self, cx: &mut Context<Self>) {
        let Some(agent) = &mut self.agent else { return };
        self.next_request += 1;
        agent.sign_in(self.next_request, rein_transcript::signing::Way::Key);
        self.sent.insert(self.next_request, Sent::SignInMore);
        cx.notify();
    }

    /// The keys go to the list that's up, if one is: the models' to what
    /// finds one, the modes' to the list itself.
    pub(super) fn picker_takes_keys(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(picker) = &self.picker else { return false };
        match picker.picking {
            Picking::Model => picker.input.clone().update(cx, |input, cx| input.focus(window, cx)),
            Picking::Mode => picker.keys.focus(window, cx),
        }
        true
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Takes what's picked: it's the line it would be typed as.
    fn take_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = &self.picker else { return };
        let choices = self.choices(picker.picking);
        let fits = Self::fitting(picker, &choices, cx);
        let Some(choice) = fits.get(picker.at).and_then(|&at| choices.get(at)) else { return };
        let line = choice.run.clone();
        self.close_picker(window, cx);
        self.run(&line, Vec::new(), cx);
    }

    fn picker_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else { return };
        let key = &event.keystroke;
        match key.key.as_str() {
            "escape" => self.close_picker(window, cx),
            "up" | "down" => {
                let picking = picker.picking;
                let choices = self.choices(picking);
                let Some(picker) = &self.picker else { return };
                let count = Self::fitting(picker, &choices, cx).len();
                let at = picker.at;
                if let Some(picker) = self.picker.as_mut().filter(|_| count > 0) {
                    picker.at = if key.key == "up" { at.checked_sub(1).unwrap_or(count - 1) } else { (at + 1) % count };
                }
                cx.notify();
            }
            "enter" if picker.picking == Picking::Mode => self.take_pick(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn pick_row(&mut self, place: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.picker {
            picker.at = place;
        }
        self.take_pick(window, cx);
    }

    /// The list that's up, by the box, in `room`: the height there is for
    /// it, which its rows fit in.
    pub(super) fn picker_card(&self, opens: Opens, room: Pixels, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.picker.as_ref()?;
        let around = if picker.picking == Picking::Model { MODEL_AROUND } else { MODE_AROUND };
        let list_most = (f32::from(room) - around).clamp(LIST_LEAST, LIST_MOST);
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let choices = self.choices(picker.picking);
        let fits = Self::fitting(picker, &choices, cx);
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut group: Option<&str> = None;
        // How much of your limits is used is said of the provider in use,
        // beside its models, as it says it: a provider that says nothing
        // shows nothing.
        let in_use = choices.iter().find(|choice| choice.current).and_then(|choice| choice.group.clone());
        let used = meters_said(&self.meters);
        for (place, &at) in fits.iter().enumerate() {
            let choice = &choices[at];
            if choice.group.as_deref() != group {
                group = choice.group.as_deref();
                let used = used.clone().filter(|_| group.is_some() && group == in_use.as_deref());
                rows.push(
                    div()
                        .flex()
                        .justify_between()
                        .gap(px(8.))
                        .px(px(8.))
                        .pt(px(10.))
                        .pb(px(4.))
                        .text_size(px(11.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(look.ink_3)
                        .child(div().min_w_0().truncate().children(group.map(str::to_owned)))
                        .children(used.map(|used| div().flex_none().font_weight(FontWeight::NORMAL).child(used)))
                        .into_any_element(),
                );
            }
            let picked = place == picker.at;
            let row = div()
                .id(("pick", place))
                .flex()
                .gap(px(8.))
                .min_h(px(30.))
                .px(px(8.))
                .py(px(5.))
                .rounded(px(7.))
                .when(picked, |row| row.bg(look.teal_tint))
                .when(!picked, |row| row.hover(|style| style.bg(look.hover)))
                .on_click(cx.listener(move |pane, _, window, cx| pane.pick_row(place, window, cx)));
            let tick = div().flex_none().w(px(14.)).text_center().text_color(look.teal_text).child(if choice.current { "✓" } else { "" });
            rows.push(match picker.picking {
                Picking::Model => row
                    .items_center()
                    .child(div().flex_1().min_w_0().truncate().child(choice.name.clone()))
                    .child(div().flex_none().font_family(mono.clone()).text_size(px(12.)).text_color(look.ink_3).child(choice.about.clone()))
                    .child(tick)
                    .into_any_element(),
                Picking::Mode => row
                    .child(tick)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_weight(FontWeight::MEDIUM).child(choice.name.clone()))
                            .child(div().text_size(px(12.)).line_height(px(16.)).text_color(if picked { look.ink_2 } else { look.ink_3 }).child(choice.about.clone())),
                    )
                    .into_any_element(),
            });
        }
        if fits.is_empty() {
            rows.push(div().px(px(8.)).py(px(16.)).text_color(look.ink_3).child("Nothing matches every word.").into_any_element());
        }
        let list = div().id("picker-list").flex().flex_col().max_h(px(list_most)).overflow_y_scroll().px(px(6.)).pt(px(4.)).pb(px(8.)).children(rows);
        let keys = cx.listener(Self::picker_key);
        let card = div()
            .id("picker")
            .debug_selector(|| "picker".to_owned())
            .track_focus(&picker.keys)
            .capture_key_down(keys)
            .occlude()
            // Once the press has done what it will: a press on what takes no
            // keys gave them to the card, where only Esc and ↑↓ are heard.
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, window, cx| cx.defer_in(window, |pane, window, cx| pane.take_keys(window, cx))))
            // A press anywhere else closes it, as a menu does; one on the
            // cell that opened it is that cell's own, and closes it once.
            .on_mouse_down_out(cx.listener(|pane, event: &MouseDownEvent, window, cx| {
                pane.picker_shut = pane.picker.as_ref().map(|picker| (picker.picking, event.position));
                pane.close_picker(window, cx);
            }))
            .absolute()
            .left(px(8.))
            .map(|card| match opens {
                Opens::Above => card.bottom(px(FROM_BOX)),
                Opens::Below => card.top(relative(1.)).mt(px(FROM_BOX)),
            })
            .flex()
            .flex_col()
            .max_w(relative(0.94))
            .rounded(px(12.))
            .overflow_hidden()
            .bg(look.float)
            .shadow(vec![crate::parts::halo(look.line, 1.)]);
        Some(match picker.picking {
            Picking::Model => {
                let typed = !picker.input.read(cx).value().is_empty();
                let count = typed.then(|| div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(format!("{} of {}", fits.len(), choices.len())));
                let sign_in = cx.listener(|pane, _, window, cx| {
                    pane.close_picker(window, cx);
                    pane.sign_in_more(cx);
                });
                let heading = div().px(px(8.)).pt(px(10.)).pb(px(4.)).text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(look.ink_3).child("Sign in to more");
                let providers = self.more_ways.clone().unwrap_or_else(|| "Another provider".to_owned());
                let row = div()
                    .id("sign-in-more")
                    .debug_selector(|| "sign-in-more".to_owned())
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(30.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .hover(|style| style.bg(look.hover))
                    .on_click(sign_in)
                    .child(div().flex_1().min_w_0().truncate().child(providers))
                    .child(div().flex_none().text_color(look.ink_3).child("›"));
                let more = div().child(heading).child(row);
                let effort = self.setting("effort").map(|effort| effort_levels(effort, cx));
                card.w(px(MODEL_WIDE))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(44.))
                            .px(px(14.))
                            .border_b_1()
                            .border_color(look.line)
                            .child(div().flex_1().child(Input::new(&picker.input).appearance(false).text_size(px(14.))))
                            .children(count),
                    )
                    .child(list)
                    .child(div().flex_none().px(px(6.)).pb(px(6.)).child(more))
                    .children(effort)
                    .child(
                        div()
                            .flex_none()
                            .px(px(14.))
                            .pt(px(8.))
                            .pb(px(10.))
                            .border_t_1()
                            .border_color(look.line)
                            .text_size(px(12.))
                            .line_height(px(17.))
                            .text_color(look.ink_3)
                            .child("The conversation carries over. Reasoning goes back only to the provider that wrote it."),
                    )
                    .into_any_element()
            }
            Picking::Mode => {
                let walled = self.setting("sandbox").is_some_and(|sandbox| sandbox.current == "on");
                let walls = if walled { "Commands run inside walls. A bad instruction stops there." } else { "Off, nothing holds a command to what it says it needs." };
                let flip = cx.listener(move |pane, _: &ClickEvent, window, cx| {
                    pane.close_picker(window, cx);
                    pane.run(if walled { "/sandbox off" } else { "/sandbox on" }, Vec::new(), cx);
                });
                card.w(px(MODE_WIDE))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .justify_between()
                            .gap(px(8.))
                            .px(px(14.))
                            .pt(px(12.))
                            .pb(px(4.))
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(look.ink_3)
                            .child("What runs without asking")
                            .child(div().font_weight(FontWeight::NORMAL).child("⇧⇥")),
                    )
                    .child(list)
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .min_h(px(48.))
                            .px(px(14.))
                            .py(px(4.))
                            .border_t_1()
                            .border_color(look.line)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().font_weight(FontWeight::MEDIUM).child("Sandbox"))
                                    .child(div().text_size(px(12.)).line_height(px(16.)).text_color(look.ink_3).child(walls)),
                            )
                            .child(switch("sandbox-switch", walled, &look).on_click(flip)),
                    )
                    .children(self.setting("keep").map(|keep| {
                        let kept = keep.current == "on";
                        let flip =
                            cx.listener(move |pane, _: &ClickEvent, _, cx| pane.set_option(rein_transcript::Sets { option: "keep", value: if kept { "off" } else { "on" } }, cx));
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .min_h(px(36.))
                            .px(px(14.))
                            .border_t_1()
                            .border_color(look.line)
                            .child(div().flex_1().min_w_0().child("Keep for new conversations"))
                            .child(switch("keep-switch", kept, &look).on_click(flip))
                    }))
                    .into_any_element()
            }
        })
    }
}

/// The levels the model takes, one of them lit.
fn effort_levels(effort: &Setting, cx: &mut Context<Pane>) -> impl IntoElement + use<> {
    let look = Look::of(cx);
    let (now, choices) = (effort.current.clone(), effort.choices.clone());
    let levels: Vec<Stateful<Div>> = choices
        .into_iter()
        .enumerate()
        .map(|(at, level)| {
            let on = level.value == now;
            let value = level.value.clone();
            div()
                .id(("effort-level", at))
                .debug_selector(move || format!("effort-{at}"))
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .justify_center()
                .h(px(24.))
                .rounded(px(6.))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { look.ink } else { look.ink_2 })
                .when(on, |it| it.bg(look.bg).shadow(vec![hairline(look.ring), crate::parts::lip(look.lip)]))
                .when(!on, |it| it.hover(|style| style.bg(look.hover)))
                .on_click(cx.listener(move |pane, _, _, cx| pane.set_effort(&value, cx)))
                .child(level.name)
        })
        .collect();
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(6.))
        .px(px(14.))
        .pt(px(10.))
        .pb(px(12.))
        .border_t_1()
        .border_color(look.line)
        .child(
            div()
                .flex()
                .justify_between()
                .gap(px(8.))
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(look.ink_3)
                .child("Effort")
                .child(div().font_weight(FontWeight::NORMAL).child("the levels this model takes")),
        )
        .child(div().flex().gap(px(2.)).p(px(2.)).rounded(px(8.)).bg(look.lit).children(levels))
}

/// A switch: a knob in a track, teal when it's on.
pub(super) fn switch(id: &'static str, on: bool, look: &Look) -> Stateful<Div> {
    let knob = div()
        .absolute()
        .top(px(2.))
        .left(px(if on { 14. } else { 2. }))
        .size(px(14.))
        .rounded_full()
        .bg(look.knob)
        .when(!on, |it| it.shadow(vec![crate::parts::halo(look.ring, 1.), crate::parts::lip(look.lip)]));
    div()
        .id(id)
        .relative()
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .rounded(px(9.))
        .bg(if on { look.teal_fill } else { look.lit })
        .when(!on, |it| it.shadow(vec![hairline(look.ring)]))
        .cursor_pointer()
        .child(knob)
}

impl Pane {
    /// Sets the effort, as any setting goes.
    fn set_effort(&mut self, level: &str, cx: &mut Context<Self>) {
        self.run(&format!("/effort {level}"), Vec::new(), cx);
    }
}

#[cfg(test)]
impl Pane {
    pub(crate) fn picking_for_test(&self) -> Option<Picking> {
        self.picker.as_ref().map(|picker| picker.picking)
    }

    pub(crate) fn open_picker_for_test(&mut self, picking: Picking, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(picking, window, cx);
    }

    /// What's typed to find a model, if that list is up.
    pub(crate) fn picker_typed_for_test(&self, cx: &App) -> Option<String> {
        self.picker.as_ref().filter(|picker| picker.picking == Picking::Model).map(|picker| picker.input.read(cx).value().to_string())
    }

    pub(crate) fn type_in_picker_for_test(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &self.picker {
            picker.input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
    }

    pub(crate) fn take_pick_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.take_pick(window, cx);
    }

    pub(crate) fn set_effort_for_test(&mut self, level: &str, cx: &mut Context<Self>) {
        self.set_effort(level, cx);
    }
}
