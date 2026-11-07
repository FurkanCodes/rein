//! What a command asks of you, drawn where the terminal's box would show it:
//! a list to narrow and pick from, a line to type, a question. Enter takes it,
//! Esc leaves it, and so does Stop.

use std::time::Instant;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Asking, Menu, Offered};
use serde_json::{Value, json};

use super::{Pane, foot};
use crate::look::Look;
use crate::parts::{Press, set_width};

/// As many rows as a list shows before it scrolls.
const LIST_MOST: f32 = 300.;
/// The fewest it shows, however little room there is.
const LIST_LEAST: f32 = 96.;

/// Who's asking, and so who hears the answer.
enum Asker {
    /// A command, under this number.
    Rein(Value),
    /// The box: what you sent, in the order it's listed.
    Finding(Vec<String>),
}

/// A question on screen.
pub(super) struct Open {
    asker: Asker,
    asking: Asking,
    input: Entity<InputState>,
    /// Which of what fits what's typed is picked.
    at: usize,
    since: Instant,
}

impl Open {
    fn fits(&self, cx: &App) -> Vec<usize> {
        let Asking::Choose(menu) = &self.asking else { return Vec::new() };
        menu.matching(&self.input.read(cx).value())
    }
}

/// What rein is told of a choice, a line or an answer you left.
fn left(asking: &Asking) -> Value {
    match asking {
        Asking::Choose(_) => json!({ "picked": null }),
        Asking::Read { .. } => json!({ "text": null }),
        Asking::Yes { .. } => json!({ "key": null }),
    }
}

/// The most of what a question's about that shows before it scrolls.
const COMMAND_MOST: f32 = 220.;

/// The answer a key gives. Esc goes by its name, as the terminal's does.
fn answer_for<'a>(answers: &'a [Offered], key: &Keystroke) -> Option<&'a Offered> {
    let bare = !key.modifiers.platform && !key.modifiers.control && !key.modifiers.alt;
    let name = if key.key == "escape" { "esc" } else { key.key.as_str() };
    answers.iter().find(|answer| bare && answer.key == name)
}

impl Pane {
    /// Puts up what a command asked, which takes the keys.
    pub(super) fn open_asking(&mut self, id: Value, asking: Asking, window: &mut Window, cx: &mut Context<Self>) {
        self.open(Asker::Rein(id), asking, window, cx);
    }

    /// Puts up what you sent, to find one by its words.
    pub(super) fn open_finding(&mut self, menu: Menu, sent: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.open(Asker::Finding(sent), Asking::Choose(menu), window, cx);
    }

    /// What was asked went with what asked it. What's yours stays.
    pub(super) fn drop_asking(&mut self) {
        if self.asking.as_ref().is_some_and(|open| matches!(open.asker, Asker::Rein(_))) {
            self.asking = None;
        }
        self.to_ask = None;
    }

    fn open(&mut self, asker: Asker, asking: Asking, window: &mut Window, cx: &mut Context<Self>) {
        let (secret, placeholder) = match &asking {
            Asking::Read { secret, .. } => (*secret, ""),
            _ => (false, "Find"),
        };
        let input = cx.new(|cx| InputState::new(window, cx).masked(secret).placeholder(placeholder));
        cx.subscribe_in(&input, window, |pane, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => pane.take_asking(window, cx),
            InputEvent::Change => {
                if let Some(open) = &mut pane.asking {
                    open.at = 0;
                }
                cx.notify();
            }
            _ => {}
        })
        .detach();
        let at = if let Asking::Choose(menu) = &asking { menu.first() } else { 0 };
        let yes = matches!(asking, Asking::Yes { .. });
        // What was up from the bar gives way to it.
        self.picker = None;
        self.asking = Some(Open { asker, asking, input: input.clone(), at, since: Instant::now() });
        cx.defer_in(window, move |pane, window, cx| {
            if pane.half_written_beside(window, cx) {
                return;
            }
            if yes {
                pane.card.focus(window, cx);
            } else {
                input.update(cx, |input, cx| input.focus(window, cx));
            }
        });
    }

    /// Gives what's asking the keys, which it took as it opened.
    pub(super) fn focus_asking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.asking else { return };
        if matches!(open.asking, Asking::Yes { .. }) {
            self.card.focus(window, cx);
        } else {
            open.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Tells rein what you answered, and gives the keys back to the box.
    fn answer_asking(&mut self, result: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.asking.take() else { return };
        match open.asker {
            Asker::Rein(id) => {
                if let Some(agent) = &mut self.agent {
                    agent.reply(&id, result);
                }
                self.watch.answered();
                self.stands_anew(cx);
            }
            Asker::Finding(sent) => {
                let found = result["picked"].as_u64().and_then(|at| usize::try_from(at).ok()).and_then(|at| sent.into_iter().nth(at));
                if let Some(text) = found {
                    self.set_box(text, window, cx);
                }
            }
        }
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// You pressed the answer under `key`: of a double click in a list, the
    /// second lands on the question that replaces it, which nobody has read.
    fn press_asking(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking.as_ref().is_some_and(|open| open.since.elapsed() >= foot::KEYS_AFTER) {
            self.answer_asking(&json!({ "key": key }), window, cx);
        }
    }

    /// You left it, with Esc or by stopping what asked.
    pub(super) fn leave_asking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.asking else { return };
        let result = left(&open.asking);
        self.answer_asking(&result, window, cx);
    }

    /// Enter: what's picked, or what's typed.
    fn take_asking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.asking else { return };
        let result = match &open.asking {
            Asking::Choose(_) => {
                let Some(&picked) = open.fits(cx).get(open.at) else { return };
                json!({ "picked": picked })
            }
            Asking::Read { .. } => json!({ "text": open.input.read(cx).value().to_string() }),
            Asking::Yes { .. } => return,
        };
        self.answer_asking(&result, window, cx);
    }

    /// A key while it's up, before the field has it.
    pub(super) fn key_at_asking(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &mut self.asking else { return };
        let key = &event.keystroke;
        if let Asking::Yes { answers, .. } = &open.asking {
            let waited = !event.is_held && open.since.elapsed() >= foot::KEYS_AFTER;
            if let Some(answer) = answer_for(answers, key).filter(|_| waited).map(|answer| answer.key.clone()) {
                self.answer_asking(&json!({ "key": answer }), window, cx);
                cx.stop_propagation();
            } else if key.key == "escape" {
                self.leave_asking(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        match key.key.as_str() {
            "escape" => self.leave_asking(window, cx),
            "up" | "down" => {
                let count = open.fits(cx).len();
                if count > 0 {
                    open.at = if key.key == "up" { open.at.checked_sub(1).unwrap_or(count - 1) } else { (open.at + 1) % count };
                }
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// What's up: above the box, or under it on the first run, in `room`
    /// when there's a limit to it. Its list gives up what the rest needs.
    pub(super) fn asking_card(&self, room: Option<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.asking.as_ref()?;
        let look = Look::of(cx);
        let card = div()
            .debug_selector(|| "asking".to_owned())
            .relative()
            .flex_none()
            // In from the rail in a conversation; on the first run, as wide as the box.
            .when(!self.draft, |card| card.ml(px(44.)).mr(px(28.)).mb(px(12.)))
            .when_some(room, |card, room| card.max_h(room).overflow_hidden())
            .flex()
            .flex_col()
            .rounded(px(10.))
            .bg(look.float)
            .border_1()
            .border_color(look.line_2)
            .shadow(vec![crate::parts::halo(look.line, 1.)])
            .capture_key_down(cx.listener(Self::key_at_asking));
        let field = |look: &Look, placeholder: bool| {
            div()
                .flex_none()
                .px(px(12.))
                .py(px(8.))
                .when(placeholder, |it| it.border_b_1().border_color(look.line))
                .child(Input::new(&open.input).appearance(false).text_size(px(14.)))
        };
        Some(match &open.asking {
            Asking::Choose(menu) => card
                .child(self.menu_head(menu, &look))
                .child(field(&look, true))
                .child(self.menu_rows(menu, open, cx))
                .child(self.keys_note("↑↓ move · ⏎ pick · esc leave", &look))
                .into_any_element(),
            Asking::Read { prompt, secret } => {
                let says = if *secret { "⏎ takes it · esc leaves · it won't be shown or kept" } else { "⏎ takes it · esc leaves" };
                card.child(div().px(px(12.)).pt(px(10.)).font_weight(FontWeight::SEMIBOLD).child(prompt.trim().to_owned()))
                    .child(field(&look, false))
                    .child(self.keys_note(says, &look))
                    .into_any_element()
            }
            Asking::Yes { title, detail, command, answers } => {
                let mono = cx.global::<crate::parts::Monospace>().0.clone();
                let width = set_width(answers.iter().map(|answer| answer.says.as_str()), true, window);
                let buttons: Vec<Press> = answers
                    .iter()
                    .enumerate()
                    .map(|(index, answer)| {
                        let key = answer.key.clone();
                        let pressed = cx.listener(move |pane, _, window, cx| pane.press_asking(&key, window, cx));
                        let press = Press::new(("answer", index)).label(answer.says.clone()).key(answer.key.clone()).width(width).on_click(pressed);
                        // The yes is the one to press, the no the quietest.
                        match answer.key.as_str() {
                            "y" => press.primary(),
                            "n" => press.ghost(),
                            _ => press,
                        }
                    })
                    .collect();
                card.track_focus(&self.card)
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(12.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title.clone()))
                    .children(detail.clone().map(|detail| div().text_color(look.ink_3).child(detail)))
                    .children(command.clone().map(|command| {
                        // A script shown whole scrolls, rather than pushing its answers away.
                        div()
                            .id("asked-command")
                            .max_h(px(COMMAND_MOST))
                            .overflow_y_scroll()
                            .px(px(10.))
                            .py(px(8.))
                            .rounded(px(7.))
                            .bg(look.bg)
                            .font_family(mono)
                            .text_size(px(12.5))
                            .line_height(px(19.))
                            .child(command)
                    }))
                    .child(div().flex().flex_wrap().gap(px(6.)).children(buttons))
                    .into_any_element()
            }
        })
    }

    fn menu_head(&self, menu: &Menu, look: &Look) -> Div {
        div().px(px(12.)).pt(px(10.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(look.ink_2).child(menu.title.clone())
    }

    fn keys_note(&self, says: &'static str, look: &Look) -> Div {
        div().flex_none().px(px(12.)).py(px(7.)).border_t_1().border_color(look.line).text_size(px(12.)).text_color(look.ink_3).child(says)
    }

    fn menu_rows(&self, menu: &Menu, open: &Open, cx: &mut Context<Self>) -> Stateful<Div> {
        let look = Look::of(cx);
        let fits = open.fits(cx);
        let list = div().id("menu").flex().flex_col().flex_shrink(1.).min_h(px(LIST_LEAST)).max_h(px(LIST_MOST)).overflow_y_scroll().px(px(4.)).py(px(4.));
        if fits.is_empty() {
            return list.child(div().px(px(8.)).py(px(12.)).text_color(look.ink_3).child("Nothing matches every word."));
        }
        let rows: Vec<Stateful<Div>> = fits
            .iter()
            .enumerate()
            .map(|(place, &index)| {
                let item = &menu.items[index];
                let picked = place == open.at;
                let in_use = menu.current == Some(index);
                div()
                    .id(("item", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(6.))
                    .when(picked, |row| row.bg(look.teal_tint))
                    .when(!picked, |row| row.hover(|style| style.bg(look.hover)))
                    .on_click(cx.listener(move |pane, _, window, cx| pane.answer_asking(&json!({ "picked": index }), window, cx)))
                    .child(div().flex_none().w(px(14.)).text_color(look.teal_text).child(if in_use { "✓" } else { "" }))
                    .child(div().flex_none().max_w(px(280.)).truncate().child(item.name.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(if picked { look.ink_2 } else { look.ink_3 }).child(item.about.clone()))
            })
            .collect();
        list.children(rows)
    }
}

#[cfg(test)]
impl Pane {
    pub(crate) fn asking_for_test(&self) -> Option<&Asking> {
        self.asking.as_ref().map(|open| &open.asking)
    }

    /// What's typed in what's asking, if anything's asking.
    pub(crate) fn asking_typed_for_test(&self, cx: &App) -> Option<String> {
        self.asking.as_ref().map(|open| open.input.read(cx).value().to_string())
    }

    pub(crate) fn press_asking_for_test(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.press_asking(key, window, cx);
    }

    pub(crate) fn type_in_asking_for_test(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = &self.asking {
            open.input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
    }

    pub(crate) fn take_asking_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.take_asking(window, cx);
    }

    pub(crate) fn since_for_test(&mut self) {
        if let Some(open) = &mut self.asking {
            open.since = Instant::now().checked_sub(foot::KEYS_AFTER).expect("the clock has run this long");
        }
    }
}
