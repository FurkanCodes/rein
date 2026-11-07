//! Putting the project back, by turn: the turns there are, and what undoing
//! back to before the one you pick does, as a sheet over the conversation.
//! rein does it and says what it did: the sheet only shows what it will.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::Update;
use rein_transcript::boards::{UndoPlan, UndoTurn};
use serde_json::Value;

use super::{Pane, Sent};
use crate::look::Look;
use crate::parts::Press;

/// How wide the sheet is, and how far down the pane it sits.
const WIDE: f32 = 640.;
const DOWN: f32 = 96.;

/// A sheet on screen.
pub(super) struct Sheet {
    turns: Vec<UndoTurn>,
    /// Which is picked.
    at: usize,
    /// What undoing it does, once rein has said.
    plan: Option<UndoPlan>,
    focus: FocusHandle,
}

impl Pane {
    /// Asks which turns there are to undo, to show them with `wanted` picked.
    pub(super) fn open_undo(&mut self, wanted: Option<usize>, cx: &mut Context<Self>) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_undo_turns(self.next_request)) {
            self.sent.insert(self.next_request, Sent::UndoTurns(wanted));
        }
        cx.notify();
    }

    /// What there is to undo has come.
    pub(super) fn undo_turns_came(&mut self, wanted: Option<usize>, said: &Value, cx: &mut Context<Self>) -> Update {
        let turns = UndoTurn::listed(said);
        if turns.is_empty() {
            return self.transcript.note("Nothing to undo: rein hasn't changed any files in this conversation.");
        }
        let at = wanted.and_then(|wanted| turns.iter().position(|turn| turn.turn == wanted)).unwrap_or(0);
        self.undoing = Some(Sheet { turns, at, plan: None, focus: cx.focus_handle() });
        self.ask_undo_plan();
        cx.notify();
        Update::default()
    }

    /// Asks what undoing the turn picked does.
    fn ask_undo_plan(&mut self) {
        let Some(turn) = self.undoing.as_ref().and_then(|sheet| sheet.turns.get(sheet.at)).map(|turn| turn.turn) else { return };
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_undo_plan(self.next_request, turn)) {
            self.sent.insert(self.next_request, Sent::UndoPlan);
        }
    }

    pub(super) fn undo_plan_came(&mut self, said: &Value, cx: &mut Context<Self>) {
        if let Some(sheet) = &mut self.undoing {
            sheet.plan = UndoPlan::of(said);
            cx.notify();
        }
    }

    /// `at` is picked, and what it does is asked again.
    fn pick_undo(&mut self, at: usize, cx: &mut Context<Self>) {
        if let Some(sheet) = &mut self.undoing {
            sheet.at = at;
            sheet.plan = None;
        }
        self.ask_undo_plan();
        cx.notify();
    }

    /// Does what the sheet says it will.
    fn undo_now(&mut self) {
        let Some(sheet) = &self.undoing else { return };
        let (Some(turn), true) = (sheet.turns.get(sheet.at).map(|turn| turn.turn), sheet.plan.as_ref().is_some_and(UndoPlan::does_anything)) else { return };
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.undo(self.next_request, turn)) {
            self.sent.insert(self.next_request, Sent::Undoing);
        }
    }

    fn undo_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(sheet) = &self.undoing else { return };
        let count = sheet.turns.len();
        match event.keystroke.key.as_str() {
            "escape" => {
                self.undoing = None;
                cx.notify();
            }
            "up" => self.pick_undo(sheet.at.checked_sub(1).unwrap_or(count - 1), cx),
            "down" => self.pick_undo((sheet.at + 1) % count, cx),
            "enter" => self.undo_now(),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The sheet, if it's up.
    pub(super) fn undo_sheet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.undoing.as_ref()?;
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let rows: Vec<Stateful<Div>> = sheet
            .turns
            .iter()
            .enumerate()
            .map(|(at, turn)| {
                let picked = at == sheet.at;
                div()
                    .id(("undo-turn", at))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(30.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(7.))
                    .when(picked, |row| row.bg(look.teal_tint))
                    .when(!picked, |row| row.hover(|style| style.bg(look.hover)))
                    .on_click(cx.listener(move |pane, _, _, cx| pane.pick_undo(at, cx)))
                    .child(div().flex_1().min_w_0().truncate().child(turn.said.clone()))
                    .child(div().flex_none().text_size(px(12.)).text_color(if picked { look.ink_2 } else { look.ink_3 }).child(turn.counts()))
                    .child(
                        div()
                            .flex_none()
                            .w(px(34.))
                            .text_right()
                            .text_size(px(12.))
                            .text_color(if picked { look.ink_2 } else { look.ink_3 })
                            .child(crate::panes::age_since(turn.at)),
                    )
            })
            .collect();
        let part = |name: &'static str, files: &[String], why: Option<&'static str>| {
            (!files.is_empty()).then(|| {
                let head = div()
                    .flex()
                    .gap(px(8.))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(look.ink_3)
                    .child(name)
                    .child(div().font_weight(FontWeight::NORMAL).child(files.len().to_string()));
                let shown = files.iter().map(|file| div().font_family(mono.clone()).text_size(px(12.)).line_height(px(19.)).text_color(look.ink_2).child(file.clone()));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(head)
                    .children(shown)
                    .children(why.map(|why| div().text_size(px(12.)).line_height(px(16.)).text_color(look.ink_3).child(why)))
            })
        };
        let plan = sheet.plan.as_ref();
        let parts = plan.map(|plan| {
            vec![
                part("Put back", &plan.put_back, None),
                part("Take away", &plan.take_away, Some("rein made it.")),
                part("Left as it is", &plan.left, Some("It has changed since rein did, so it stays yours.")),
                part("Can't come back", &plan.lost, Some("Too big to keep a copy of.")),
            ]
        });
        let over = plan.map_or(1, |plan| plan.turns);
        let note = (over > 1).then_some("Undoing a turn undoes the ones after it.");
        let action = if over > 1 { format!("Undo {over} turns") } else { "Undo this turn".to_owned() };
        let can = plan.is_some_and(UndoPlan::does_anything);
        let cancel = Press::new("undo-cancel").label("Cancel").key("esc").on_click(cx.listener(|pane, _, _, cx| {
            pane.undoing = None;
            cx.notify();
        }));
        let undo = can.then(|| Press::new("undo-now").primary().label(action).key("⏎").on_click(cx.listener(|pane, _, _, _| pane.undo_now())));
        let card = div()
            .id("undo-sheet")
            .track_focus(&sheet.focus)
            .on_key_down(cx.listener(|pane, event: &KeyDownEvent, _, cx| pane.undo_key(event, cx)))
            .occlude()
            .w(px(WIDE))
            .max_w(relative(0.92))
            .max_h(relative(0.8))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .overflow_hidden()
            .bg(look.float)
            .shadow(vec![crate::parts::halo(look.line, 1.)])
            .child(
                div()
                    .flex_none()
                    .px(px(16.))
                    .pt(px(16.))
                    .pb(px(10.))
                    .child(div().text_size(px(15.)).line_height(px(20.)).font_weight(FontWeight::SEMIBOLD).child("Put the project back"))
                    .child(
                        div()
                            .mt(px(2.))
                            .text_size(px(13.))
                            .line_height(px(18.))
                            .text_color(look.ink_3)
                            .child("Pick a turn. rein puts back what it did from then on, commands' changes too."),
                    ),
            )
            .child(div().id("undo-turns").flex_none().max_h(px(180.)).overflow_y_scroll().border_t_1().border_b_1().border_color(look.line).p(px(6.)).children(rows))
            .child(
                div().id("undo-plan").flex_1().min_h_0().overflow_y_scroll().px(px(16.)).py(px(12.)).flex().flex_col().gap(px(12.)).children(parts.into_iter().flatten().flatten()),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(look.line)
                    .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(look.ink_3).children(note))
                    .child(cancel)
                    .children(undo),
            );
        let close = cx.listener(|pane, _: &ClickEvent, _, cx| {
            pane.undoing = None;
            cx.notify();
        });
        Some(
            div()
                .id("undo-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(DOWN))
                .rounded(px(10.))
                .bg(look.scrim)
                .occlude()
                .on_click(close)
                .child(card)
                .into_any_element(),
        )
    }

    /// The sheet takes the keys as it comes up.
    pub(super) fn settle_undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sheet) = &self.undoing
            && !sheet.focus.contains_focused(window, cx)
        {
            sheet.focus.focus(window, cx);
        }
    }
}

#[cfg(test)]
impl Pane {
    pub(crate) fn undo_plan_for_test(&self) -> Option<Option<UndoPlan>> {
        self.undoing.as_ref().map(|sheet| sheet.plan.clone())
    }

    pub(crate) fn undo_picked_for_test(&self) -> Option<usize> {
        self.undoing.as_ref().and_then(|sheet| sheet.turns.get(sheet.at)).map(|turn| turn.turn)
    }

    pub(crate) fn open_undo_for_test(&mut self, wanted: Option<usize>, cx: &mut Context<Self>) {
        self.open_undo(wanted, cx);
    }

    pub(crate) fn undo_now_for_test(&mut self) {
        self.undo_now();
    }
}
