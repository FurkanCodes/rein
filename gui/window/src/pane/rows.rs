//! One row of a conversation. Everything hangs on the rail: every step is a
//! node on it, and what's said sits to its right.

use gpui_kit::base::TextView;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Role, StepState};

use super::Pane;
use crate::look::{Look, THREAD_FROM, THREAD_TO};
use crate::parts::{Press, working};

/// Where what's said starts.
const TEXT: f32 = 56.;
/// Where a band or a card starts.
const CARD: f32 = 44.;
/// What's kept clear on the right.
const RIGHT: f32 = 28.;
/// How wide what the model says may run, near 66 characters.
const MEASURE: f32 = 640.;
/// Said under another rein's words.
const NEVER_YOURS: &str = "Another rein's words are never yours, and can't approve anything.";
/// How much of a step's output opens.
const OUTPUT_LINES: usize = 40;

/// A node on the rail: `size` across at `x`, with a ring of the pane's colour
/// that cuts the rail around it.
fn node(x: f32, y: f32, size: f32, pane: Hsla) -> Div {
    div().absolute().left(px(x - 3.)).top(px(y - 3.)).size(px(size + 6.)).rounded_full().border(px(3.)).border_color(pane)
}

/// How many paths of reads in a row are named before the rest are counted.
const PATHS_NAMED: usize = 2;

/// `a.rs, b.rs +1`.
fn paths_shown(paths: &[String]) -> String {
    let named = paths.iter().take(PATHS_NAMED).map(String::as_str).collect::<Vec<_>>().join(", ");
    match paths.len().saturating_sub(PATHS_NAMED) {
        0 => named,
        more => format!("{named} +{more}"),
    }
}

/// `3 files · 412 lines`.
fn reads_said(reads: &rein_transcript::Reads) -> String {
    match reads.lines {
        Some(1) => format!("{} files · 1 line", reads.paths.len()),
        Some(lines) => format!("{} files · {lines} lines", reads.paths.len()),
        None => format!("{} files", reads.paths.len()),
    }
}

/// What a step came to, after what it's about: an edit's lines added and
/// taken away in their colours, a failure in red, the rest quiet.
fn summary_shown(summary: &rein_transcript::Summary, look: &Look) -> AnyElement {
    if summary.added.is_some() || summary.removed.is_some() {
        let added = summary.added.filter(|&added| added > 0).map(|added| div().text_color(look.add).child(format!("+{added}")));
        let removed = summary.removed.filter(|&removed| removed > 0).map(|removed| div().text_color(look.del).child(format!("−{removed}")));
        return div().flex().flex_none().gap(px(5.)).children(added).children(removed).into_any_element();
    }
    let said = if summary.said.is_empty() { summary.status.clone() } else { format!("{} · {}", summary.status, summary.said) };
    div().min_w_0().truncate().when(summary.failed, |it| it.text_color(look.red)).child(said).into_any_element()
}

impl Pane {
    /// Row `index`, which a shift-click takes what's selected to.
    pub(super) fn row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let body = self.row_body(index, cx);
        let selected = self.selected_whole(index).then(|| Look::of(cx).teal_tint);
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

    /// What separates row `index` from the row above it: little between
    /// steps and between a message's blocks, more between messages.
    fn gap_above(&self, index: usize) -> f32 {
        let Some(row) = self.transcript.row(index) else { return 0. };
        let Some(above) = index.checked_sub(1).and_then(|above| self.transcript.row(above)) else { return 22. };
        let role = |at: usize| self.transcript.get(at).map(|message| message.role);
        match (role(above.message), role(row.message)) {
            // What you said is one card, however many paragraphs it runs to.
            (_, Some(Role::You | Role::Peer)) if above.message == row.message => 0.,
            _ if above.message == row.message => 10.,
            (Some(Role::Step), Some(Role::Step)) => 1.,
            _ => 12.,
        }
    }

    fn row_body(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.transcript.row(index) else { return div().into_any_element() };
        let Some(role) = self.transcript.get(row.message).map(|message| message.role) else { return div().into_any_element() };
        let (top, last) = (self.gap_above(index), index + 1 == self.transcript.rows());
        let look = Look::of(cx);
        let pane = self.sheet(&look);
        let body = if role == Role::Step {
            self.step(index, row.message, top, cx)
        } else if role == Role::Turn {
            self.turn_line(row.message, top, cx)
        } else {
            let Some(view) = self.view(index, cx) else { return div().into_any_element() };
            let text = TextView::new(&view).selectable(true);
            let first = row.block == 0;
            match role {
                Role::You => {
                    let dot = first.then(|| node(28., top + 16., 7., pane).child(div().size_full().rounded_full().bg(look.ink)));
                    let end = self.transcript.row(index + 1).is_none_or(|next| next.message != row.message);
                    let card = div()
                        .px(px(12.))
                        .pt(px(if first { 8. } else { 10. }))
                        .pb(px(if end { 9. } else { 0. }))
                        .bg(look.band)
                        .when(first, |it| it.rounded_t(px(8.)))
                        .when(end, |it| it.rounded_b(px(8.)))
                        .text_size(px(15.))
                        .line_height(px(23.))
                        .child(text);
                    div().relative().pt(px(top)).pl(px(CARD)).pr(px(RIGHT)).children(dot).child(card)
                }
                Role::Agent => {
                    let thread = linear_gradient(90., linear_color_stop(rgb(THREAD_FROM), 0.), linear_color_stop(rgb(THREAD_TO), 1.));
                    let dot = first.then(|| node(28.5, top + 9., 6., pane).child(div().size_full().rounded_full().bg(thread)));
                    div().relative().pt(px(top)).pl(px(TEXT)).pr(px(RIGHT)).max_w(px(TEXT + MEASURE)).text_size(px(15.)).line_height(px(24.)).children(dot).child(text)
                }
                Role::Peer => {
                    let from = self.transcript.get(row.message).and_then(|said| said.from.clone()).unwrap_or_else(|| "another rein".to_owned());
                    let end = self.transcript.row(index + 1).is_none_or(|next| next.message != row.message);
                    let who = first.then(|| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .pb(px(4.))
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(look.ink_2)
                            .child(div().size(px(9.)).rounded_full().border_1().border_color(look.teal))
                            .child(format!("{from}, another rein"))
                    });
                    // Said once, under the last of its words.
                    let never_yours = end.then(|| div().pt(px(6.)).text_size(px(12.)).line_height(px(16.)).text_color(look.ink_3).child(NEVER_YOURS));
                    let card = div()
                        .px(px(14.))
                        .pt(px(if first { 9. } else { 10. }))
                        .pb(px(if end { 10. } else { 0. }))
                        .border_l_1()
                        .border_r_1()
                        .border_color(look.line)
                        .when(first, |it| it.border_t_1().rounded_t(px(10.)))
                        .when(end, |it| it.border_b_1().rounded_b(px(10.)))
                        .text_size(px(14.))
                        .line_height(px(22.))
                        .children(who)
                        .child(text)
                        .children(never_yours);
                    div().pt(px(top)).pl(px(CARD)).pr(px(RIGHT)).child(card)
                }
                Role::Thought | Role::Note | Role::Said | Role::Step | Role::Turn => {
                    let dash = (role == Role::Note).then(|| div().absolute().left(px(26.)).top(px(top + 10.)).w(px(11.)).h(px(1.)).bg(look.ink_3));
                    div().relative().pt(px(top)).pl(px(TEXT)).pr(px(RIGHT)).text_size(px(13.)).line_height(px(19.)).text_color(look.ink_3).children(dash).child(text)
                }
            }
        };
        // Above the first thing said there's no rail yet.
        let cover = (index == 0).then(|| div().absolute().left(px(24.)).top_0().w(px(15.)).h(px(top + 13.)).bg(pane));
        div().relative().when(last, |it| it.pb(px(12.))).children(cover).child(body).into_any_element()
    }

    /// What a turn came to, on the rail, and a way back to before it.
    fn turn_line(&self, message: usize, top: f32, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let Some(said) = self.transcript.get(message) else { return div() };
        let undo = self.transcript.undo_point(message).filter(|_| !self.running).map(|turn| {
            let pressed = cx.listener(move |pane, _, _, cx| pane.open_undo(Some(turn), cx));
            Press::new(("undo-to", message)).ghost().small().label("Undo to here").on_click(pressed)
        });
        let dash = div().absolute().left(px(26.)).top(px(top + 12.)).w(px(11.)).h(px(1.)).bg(look.ink_3);
        div()
            .relative()
            .pt(px(top))
            .flex()
            .items_center()
            .gap(px(8.))
            .min_h(px(24.))
            .pl(px(TEXT))
            .pr(px(RIGHT))
            .text_size(px(12.))
            .text_color(look.ink_3)
            .child(dash)
            .child(div().min_w_0().truncate().child(said.text.clone()))
            .children(undo)
    }

    /// A step: its mark on the rail, its verb, what it's about. Pressing it
    /// opens what it printed.
    fn step(&self, index: usize, message: usize, top: f32, cx: &mut Context<Self>) -> Div {
        let (look, mono) = (Look::of(cx), cx.global::<crate::parts::Monospace>().0.clone());
        let pane = self.sheet(&look);
        let Some(said) = self.transcript.get(message) else { return div() };
        // Reads in a row are said on the first one's line, until it's opened.
        let first_read = self.first_of_reads(message);
        if first_read != message && !self.open_steps.contains(&first_read) {
            return div();
        }
        let reads = self.transcript.reads_from(message).filter(|_| !self.open_steps.contains(&message));
        let (verb, what) = said.step_line();
        let what = reads.as_ref().map_or_else(|| what.to_owned(), |reads| paths_shown(&reads.paths));
        let state = said.state();
        let tick = |shown: &'static str, color: Hsla| {
            div().absolute().left(px(24.)).top(px(3.)).w(px(15.)).h(px(14.)).bg(pane).flex().justify_center().text_size(px(11.)).line_height(px(14.)).text_color(color).child(shown)
        };
        let mark = match state {
            StepState::Running => node(26., 4.5, 11., pane).bg(pane).child(working(11., look.teal, self.beat)).into_any_element(),
            StepState::Done => tick("✓", look.ink_3).into_any_element(),
            StepState::Failed => tick("✗", look.red).into_any_element(),
            StepState::Stopped => tick("–", look.ink_3).into_any_element(),
        };
        let summary = match &reads {
            Some(reads) => Some(reads_said(reads)).map(|said| div().flex_none().child(said).into_any_element()),
            None => said.summary.as_ref().map(|summary| summary_shown(summary, &look)),
        };
        // A sub-agent runs in a pane of its own, a press away.
        let open_it = (verb == "Sub-agent").then(|| {
            let role = what.split_once(':').map_or(what.as_str(), |(role, _)| role).to_owned();
            Press::new(("open-sub-agent", message)).ghost().small().label("Open").on_click(cx.listener(move |_, _, _, cx| cx.emit(super::Told::OpenSubAgent(role.clone()))))
        });
        let output = said.step_output();
        let open = self.open_steps.contains(&message) != self.all_open && !output.is_empty() && reads.is_none();
        let shown = open.then(|| {
            let lines: Vec<&str> = output.lines().rev().take(OUTPUT_LINES).collect();
            let tail = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
            div()
                .mt(px(4.))
                .mb(px(4.))
                .ml(px(TEXT))
                .mr(px(RIGHT))
                .px(px(10.))
                .py(px(8.))
                .rounded(px(7.))
                .bg(look.raised)
                .font_family(mono.clone())
                .text_size(px(12.))
                .line_height(px(19.))
                .text_color(look.ink_2)
                .child(tail)
        });
        let line = div()
            .id(("step", index))
            .relative()
            .flex()
            // A button has a height of its own, which text's baseline doesn't.
            .map(|line| if open_it.is_some() { line.items_center() } else { line.items_baseline() })
            .gap(px(7.))
            .min_h(px(20.))
            .pl(px(TEXT))
            .pr(px(RIGHT))
            .text_size(px(12.5))
            .line_height(px(20.))
            .text_color(look.ink_3)
            .on_click(cx.listener(move |pane, _, _, cx| pane.toggle_step(message, index, cx)))
            .child(mark)
            .child(div().flex_none().font_weight(FontWeight::MEDIUM).text_color(look.ink_2).child(verb.to_owned()))
            .child(div().min_w_0().truncate().font_family(mono).text_size(px(12.)).child(what))
            .children(summary)
            .children(open_it);
        div().pt(px(top)).child(line).children(shown)
    }

    /// The first of the reads in a row that `message` is one of, or itself.
    fn first_of_reads(&self, message: usize) -> usize {
        let mut first = message;
        while self.transcript.read_after_read(first) {
            first -= 1;
        }
        first
    }

    fn toggle_step(&mut self, message: usize, index: usize, cx: &mut Context<Self>) {
        if !self.open_steps.remove(&message) {
            self.open_steps.insert(message);
        }
        // Reads in a row open and close together.
        let rows = self.transcript.rows();
        self.scroller.update(cx, |scroller, cx| scroller.remeasure_items(index..rows.max(index + 1), cx));
        cx.notify();
    }
}
