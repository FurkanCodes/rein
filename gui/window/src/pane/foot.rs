//! What's under the conversation: a question waiting for you, and the box,
//! with the terminal's status bar under it as an instrument. The bar shows
//! only what the agent has said: a cell with nothing behind it isn't drawn.

use std::time::Duration;

use gpui_kit::component::input::{Enter, MoveDown, MoveUp, Textarea};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Answering, ChangedLine, Choice, FileChange, Setting};

use super::panel::Tab;
use super::pickers::Picking;
use super::{Pane, RAIL, Sent};
use crate::look::{Look, THREAD_FROM, THREAD_TO};
use crate::parts::{Press, gauge, hairline, halo, lip, pips, set_width};

/// Where a pane's foot starts the box, and how far the rail runs into it.
const OVER_BOX: f32 = 12.;
/// A question shows this much of what it would do, and scrolls the rest.
const QUESTION_MOST: f32 = 180.;

/// How long a question waits before it takes an answer: a key or a click
/// as it appears was meant for something else, and this one can send a
/// `git push`.
pub(super) const KEYS_AFTER: Duration = Duration::from_millis(400);

/// The name on a choice's button, and the key that presses it. The agent's
/// own word for it, with its exact reach, is what a screen reader says.
fn choice_label(choice: &Choice) -> (&'static str, &'static str) {
    match choice.kind {
        Answering::AllowAlways => ("Always", "a"),
        Answering::AllowOnce => ("Allow", "y"),
        Answering::RejectOnce | Answering::RejectAlways => ("Reject", "n"),
        Answering::Instead => ("Tell rein instead", "esc"),
    }
}

/// The choice `key` presses, in time. Only a bare key does: `⌘A` is a
/// shortcut, not an answer.
fn pressed<'a>(choices: &'a [Choice], key: &Keystroke, waited: Duration) -> Option<&'a Choice> {
    let bare = !key.modifiers.platform && !key.modifiers.control && !key.modifiers.alt;
    let name = if key.key == "escape" { "esc" } else { key.key.as_str() };
    choices.iter().find(|choice| bare && waited >= KEYS_AFTER && choice_label(choice).1 == name)
}

/// What `key` types, if it does: a shortcut doesn't, and nor does Enter,
/// which would send what you can't see it send.
fn types(key: &Keystroke) -> Option<String> {
    key.key_char.clone().filter(|typed| !key.modifiers.platform && !key.modifiers.control && !typed.chars().any(char::is_control))
}

/// A line of a file change, on its tint, as the design's diffs are.
fn changed_line(line: &ChangedLine, look: &Look) -> impl IntoElement + use<> {
    let number = line.number.map(|number| number.to_string()).unwrap_or_default();
    // An empty line still takes its height.
    let text = if line.text.is_empty() { " ".to_owned() } else { line.text.clone() };
    div()
        .flex()
        .w_full()
        .bg(if line.added { look.add_tint } else { look.del_tint })
        .child(div().flex_none().w(px(44.)).pr(px(10.)).text_right().text_color(look.ink_3).child(number))
        .child(div().flex_1().min_w_0().pr(px(10.)).child(text))
}

/// What a step would change, under its path.
fn file_change(path: &str, change: &FileChange, look: &Look) -> impl IntoElement + use<> {
    let path = div().px(px(10.)).py(px(6.)).border_b_1().border_color(look.line).text_color(look.ink_2).child(path.to_owned());
    let every = change.everywhere.then(|| div().px(px(10.)).py(px(4.)).text_color(look.ink_3).child("at every place it's found"));
    div().flex().flex_col().child(path).children(change.lines.iter().map(|line| changed_line(line, look))).children(every)
}

/// A model as the bar has room for it: with no provider in front.
fn short_model(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The breathing of the line above the box, which takes a beat in three.
fn breath(beat: u8) -> f32 {
    const STEPS: [f32; 12] = [1., 0.89, 0.78, 0.67, 0.56, 0.45, 0.45, 0.56, 0.67, 0.78, 0.89, 1.];
    STEPS[usize::from(beat / 3) % STEPS.len()]
}

impl Pane {
    /// The oldest question, on the rail, however long it is: what you'd
    /// allow scrolls, and is never cut.
    pub(super) fn question(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let waiting = self.asked.front()?;
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let (verb, what) = waiting.question.as_ref().map_or(("do", "a step it hasn't shown"), |question| question.title.split_once(' ').unwrap_or((&question.title, "")));
        let heading = waiting.words.asks.clone().unwrap_or_else(|| format!("rein asks to {}", verb.to_lowercase()));
        let change = waiting.question.as_ref().and_then(|question| question.change.as_ref());
        let whole = waiting.question.as_ref().and_then(|question| question.detail.clone()).unwrap_or_else(|| what.to_owned());
        let plain = |choice: &&Choice| choice.kind != Answering::Instead;
        let width = set_width(waiting.choices.iter().filter(plain).map(|choice| choice_label(choice).0), true, window);
        // Shown only while they work: with your keys in the box they'd type.
        let keyed = self.card.is_focused(window);
        let buttons: Vec<Press> = waiting
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let (chosen, drawn_for) = (choice.id.clone(), waiting.id.clone());
                let (label, key) = choice_label(choice);
                let pressed = cx.listener(move |pane, _, _, cx| pane.answer(&drawn_for, &chosen, cx));
                let button = Press::new(("choice", index)).label(label).named(choice.name.clone()).on_click(pressed);
                let button = if plain(&choice) { button.width(width) } else { button };
                let button = if choice.kind == Answering::AllowAlways { button.tip(choice.name.clone()) } else { button };
                let button = if keyed { button.key(key) } else { button };
                if index == 0 && choice.allows() { button.primary() } else { button }
            })
            .collect();
        let pane = self.sheet(&look);
        Some(
            div()
                .track_focus(&self.card)
                .on_key_down(cx.listener(|pane, event: &KeyDownEvent, window, cx| pane.key_at_question(event, window, cx)))
                .relative()
                .flex_none()
                .ml(px(44.))
                .mr(px(28.))
                .mb(px(12.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .px(px(14.))
                .py(px(12.))
                .rounded(px(10.))
                .bg(look.amber_tint)
                .border_1()
                .border_color(look.amber_line)
                .child(
                    div()
                        .absolute()
                        .left(px(-20.))
                        .top(px(13.))
                        .size(px(15.))
                        .rounded_full()
                        .border(px(3.))
                        .border_color(pane)
                        .child(div().size_full().rounded_full().bg(look.amber)),
                )
                .child(div().font_weight(FontWeight::SEMIBOLD).child(heading))
                .child(
                    div()
                        .id("asked")
                        .max_h(px(QUESTION_MOST))
                        .overflow_y_scroll()
                        .when(change.is_none(), |it| it.px(px(10.)).py(px(8.)))
                        .rounded(px(7.))
                        .bg(look.bg)
                        .border_1()
                        .border_color(look.line)
                        .font_family(mono)
                        .text_size(px(12.5))
                        .line_height(px(19.))
                        .map(|it| match change {
                            Some(change) => it.child(file_change(&whole, change, &look)),
                            None => it.child(whole.clone()),
                        }),
                )
                .children(waiting.words.why.clone().map(|why| div().text_size(px(12.)).text_color(look.ink_2).child(why)))
                .child(div().flex().flex_wrap().gap(px(6.)).children(buttons)),
        )
    }

    /// The box, and the bar under it.
    pub(super) fn foot(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        let has_keys = self.input.read(cx).focus_handle(cx).is_focused(window);
        // A command waiting for your answer isn't working.
        let working = self.running && self.asking.is_none();
        let thread = working.then(|| {
            let line = linear_gradient(90., linear_color_stop(rgb(THREAD_FROM), 0.), linear_color_stop(rgb(THREAD_TO), 1.));
            div().absolute().left(px(23.)).right(px(18.)).top_0().h(px(1.5)).bg(line).opacity(breath(self.beat))
        });
        let (rail_width, rail) = if working { (1.5, Hsla::from(rgb(THREAD_FROM))) } else { (1., look.line_2) };
        let pane = cx.weak_entity();
        let paste = move |item: &ClipboardItem, window: &mut Window, cx: &mut App| pane.update(cx, |pane, cx| pane.pasted(item, window, cx)).unwrap_or(false);
        let hint = (self.running && self.asking.is_none())
            .then(|| div().absolute().right(px(12.)).top(px(9.)).text_size(px(12.)).text_color(look.ink_3).child("⏎ now · ⇥ after this turn"));
        div()
            .relative()
            .flex_none()
            .pt(px(OVER_BOX))
            .px(px(8.))
            .pb(px(8.))
            .capture_action(cx.listener(|pane, _: &MoveUp, window, cx| pane.arrow(true, window, cx)))
            .capture_action(cx.listener(|pane, _: &MoveDown, window, cx| pane.arrow(false, window, cx)))
            .capture_action(cx.listener(|pane, action: &Enter, window, cx| pane.enter(action, window, cx)))
            .capture_action(cx.listener(Self::escape))
            .child(div().absolute().left(px(RAIL)).top_0().w(px(rail_width)).h(px(OVER_BOX)).bg(rail))
            .child(
                div()
                    .relative()
                    .rounded(px(10.))
                    .bg(look.bg)
                    .shadow(if has_keys { vec![hairline(look.focus_line), lip(look.lip), halo(look.focus_halo, 3.)] } else { vec![hairline(look.ring), lip(look.lip)] })
                    .children(thread)
                    .children(hint)
                    .child(div().min_h(px(62.)).pt(px(6.)).pb(px(4.)).pl(px(36.)).pr(px(6.)).child(Textarea::new(&self.input).appearance(false).text_size(px(15.)).on_paste(paste)))
                    .child(self.bar(cx)),
            )
    }

    /// What a press opens.
    fn picks(picking: Picking, cx: &mut Context<Self>) -> impl Fn(&ClickEvent, &mut Window, &mut App) + use<> {
        let pane = cx.weak_entity();
        move |_, window, cx| {
            let _ = pane.update(cx, |pane, cx| pane.open_picker(picking, window, cx));
        }
    }

    /// The instrument under the box.
    fn bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        // With no agent there's nothing to give it.
        let add = self
            .agent
            .is_some()
            .then(|| Press::new("add").cell().named("Add a file or an image").child("+").on_click(cx.listener(|pane, _, window, cx| pane.pick_files(window, cx))));
        // A cell runs the command for what it shows, and shows its list.
        let model = self
            .setting("model")
            .map(|model| Press::new("model").cell().yields().child(div().truncate().child(short_model(model.current_name()).to_owned())).on_click(Self::picks(Picking::Model, cx)));
        let effort = self
            .setting("effort")
            .and_then(Setting::pips)
            .map(|(level, levels)| Press::new("effort").cell().named("Effort").child(pips(level, levels, &look)).on_click(Self::picks(Picking::Model, cx)));
        let mode = self.modes.as_ref().map(|modes| {
            let name = modes.current_name().to_owned();
            let (warning, name) = name.strip_prefix("⚠ ").map_or((None, name.as_str()), |name| (Some(div().text_color(look.amber).child("⚠")), name));
            let walled = self.setting("sandbox").is_some_and(|sandbox| sandbox.current == "on");
            let shown = if walled { format!("{name} · sandbox") } else { name.to_owned() };
            if !self.transcript.commands().is_empty() {
                return Press::new("mode").cell().yields().children(warning).child(div().truncate().child(shown)).on_click(Self::picks(Picking::Mode, cx)).into_any_element();
            }
            let available = modes.available.clone();
            let pane = cx.weak_entity();
            Press::new("mode")
                .cell()
                .children(warning)
                .child(shown)
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                    available.iter().cloned().fold(menu, |menu, mode| {
                        let pane = pane.clone();
                        menu.item(PopupMenuItem::new(mode.name.clone()).on_click(move |_, _, cx| {
                            let _ = pane.update(cx, |pane, _| pane.set_mode(&mode.id));
                        }))
                    })
                })
                .into_any_element()
        });
        let context = self.transcript.context().map(|context| {
            let fill = if context.share() >= 0.8 { look.amber } else { look.ink_3 };
            let pressed = cx.listener(|pane, _, _, cx| pane.open_panel(Tab::Context, cx));
            Press::new("context")
                .cell_at_end()
                .named(format!("Context: {}", context.shown()))
                .child(gauge(context.share(), Some(44.), fill, &look))
                .child(context.shown())
                .on_click(pressed)
        });
        // What's left running would otherwise show only in a panel you opened.
        let tasks = (self.background > 0).then(|| {
            let running = format!("{} running", self.background);
            let pressed = cx.listener(|pane, _, _, cx| pane.open_panel(Tab::Tasks, cx));
            Press::new("tasks")
                .cell_at_end()
                .named(format!("Background commands: {running}"))
                .child(div().size(px(6.)).rounded_full().bg(look.teal))
                .child(running)
                .on_click(pressed)
        });
        // With no agent, there's nothing to send to or stop.
        let act = self.agent.is_some().then(|| {
            let act = if self.running {
                Press::new("stop").small().label("Stop").key("esc").on_click(cx.listener(|pane, _, _, cx| pane.stop(cx)))
            } else {
                Press::new("send").small().label("Send").key("⏎").on_click(cx.listener(|pane, _, window, cx| pane.send_box(window, cx)))
            };
            div().flex_none().mx(px(6.)).child(act)
        });
        div()
            .flex()
            .items_center()
            .h(px(34.))
            .border_t_1()
            .border_color(look.line)
            .children(add)
            .children(model)
            .children(effort)
            .children(mode)
            .child(div().flex_1().min_w_0())
            .children(tasks)
            .children(context)
            .children(act)
    }
}

impl Pane {
    /// A key while a question holds them: its answer, in time, or else
    /// something meant for the box, which gets it, and the keys back.
    fn key_at_question(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A key held down isn't an answer: its repeats would go on to the
        // next question, which you haven't read.
        let key = &event.keystroke;
        let Some(waiting) = self.asked.front().filter(|_| !event.is_held) else { return };
        if let Some(chosen) = pressed(&waiting.choices, key, self.asked_at.elapsed()).map(|choice| choice.id.clone()) {
            let drawn_for = waiting.id.clone();
            self.answer(&drawn_for, &chosen, cx);
            // Esc is an answer here, and the pane would stop the turn.
            cx.stop_propagation();
            return;
        }
        if self.type_in_box(key, window, cx) {
            cx.stop_propagation();
        }
    }

    /// `key` goes in the box, and your keys with it, if it's one that types.
    fn type_in_box(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(typed) = types(key) else { return false };
        self.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.insert(typed, window, cx);
        });
        true
    }

    /// You typed where nothing types, like the conversation's text, which
    /// keeps the keys for `⌘C`. In a terminal what you type goes in the box
    /// wherever you last pressed, so it does here, unless a list or a sheet
    /// that's up has them.
    pub(crate) fn typed_elsewhere(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking.is_none() && self.picker.is_none() && self.undoing.is_none() {
            self.type_in_box(key, window, cx);
        }
    }

    /// Shift-Tab: on to the next mode, as in the terminal.
    pub(super) fn next_mode(&mut self) {
        let Some(modes) = &self.modes else { return };
        let at = modes.available.iter().position(|mode| mode.id == modes.current).unwrap_or(0);
        if let Some(next) = modes.available.get((at + 1) % modes.available.len().max(1)).map(|mode| mode.id.clone()) {
            self.set_mode(&next);
        }
    }

    /// Asks for the mode `id`. The bar says so once the agent has: until then
    /// it's in the mode it was in, and a bar that said otherwise could show
    /// `ask` over a turn running in `trust`.
    pub(super) fn set_mode(&mut self, id: &str) {
        let Some(agent) = &mut self.agent else { return };
        self.next_request += 1;
        agent.set_mode(self.next_request, id);
        self.sent.insert(self.next_request, Sent::Mode(id.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Choice, Duration, KEYS_AFTER, Keystroke, pressed, types};

    fn choices() -> Vec<Choice> {
        let options = json!([{ "optionId": "yes", "name": "Allow", "kind": "allow_once" }, { "optionId": "keep", "name": "Don't ask again", "kind": "allow_always" }, { "optionId": "no", "name": "Reject", "kind": "reject_once" }]);
        Choice::offered(&options, true)
    }

    fn key(key: &'static str) -> Keystroke {
        Keystroke::parse(key).unwrap()
    }

    #[test]
    fn y_a_and_n_answer_a_question_that_has_waited() {
        let (choices, waited) = (choices(), KEYS_AFTER);
        let chosen = |typed: &'static str| pressed(&choices, &key(typed), waited).map(|choice| choice.id.as_str());
        assert_eq!([chosen("y"), chosen("a"), chosen("n")], [Some("yes"), Some("keep"), Some("no")]);
        assert_eq!(chosen("w"), None, "any other key is meant for the box");
    }

    #[test]
    fn esc_is_rein_s_own_no_where_it_s_offered() {
        let options = json!([{ "optionId": "no", "name": "No", "kind": "reject_once" }, { "optionId": "stop", "name": "No, and say", "kind": "reject_once", "_meta": { "io.github.furkancodes/instead": true } }]);
        let offered = Choice::offered(&options, true);
        assert_eq!(pressed(&offered, &key("escape"), KEYS_AFTER).map(|choice| choice.id.as_str()), Some("stop"));
        assert_eq!(pressed(&offered, &key("n"), KEYS_AFTER).map(|choice| choice.id.as_str()), Some("no"), "n is the plain one");
        assert_eq!(pressed(&choices(), &key("escape"), KEYS_AFTER), None, "where it isn't, esc is the pane's");
    }

    #[test]
    fn only_a_key_that_writes_something_types() {
        let with = |key: &str, typed: &str| Keystroke { key_char: Some(typed.to_owned()), ..self::key(key.to_owned().leak()) };
        assert_eq!(types(&with("h", "h")), Some("h".to_owned()));
        assert_eq!(types(&with("cmd-c", "c")), None, "a platform may say what a shortcut would have typed");
        assert_eq!(types(&with("ctrl-a", "a")), None);
        assert_eq!(types(&with("enter", "\r")), None);
    }

    #[test]
    fn a_key_pressed_as_a_question_appears_answers_nothing() {
        assert_eq!(pressed(&choices(), &key("y"), KEYS_AFTER - Duration::from_millis(1)), None);
    }

    #[test]
    fn a_shortcut_is_no_answer() {
        assert_eq!(pressed(&choices(), &key("cmd-a"), KEYS_AFTER), None);
        assert_eq!(pressed(&choices(), &key("ctrl-n"), KEYS_AFTER), None);
    }

    #[test]
    fn a_step_that_cant_be_allowed_has_no_key_for_yes() {
        let options = json!([{ "optionId": "yes", "name": "Allow", "kind": "allow_once" }]);
        let unknown = Choice::offered(&options, false);
        assert_eq!(pressed(&unknown, &key("y"), KEYS_AFTER), None);
        assert_eq!(pressed(&unknown, &key("n"), KEYS_AFTER).map(|choice| choice.id.as_str()), Some("reject"));
    }
}
