//! What a conversation changed, beside it, as undoing it would see it: file
//! by file, with comments on lines that go to rein as your words. Asked for
//! as it's shown, and when a turn ends for the count in the header.

use gpui_kit::component::diff::{Diff, DiffAnnotation, DiffChangeIndicator, DiffFile, DiffHoverHighlight, DiffHunkSeparator, DiffLinePosition, DiffLineRange, DiffSide, DiffState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::changes::{ChangedFile, Changes, Comment, Side, Span, Totals, comments_said};
use serde_json::Value;

use super::panel::Tab;
use super::{Pane, Sent};
use crate::look::Look;
use crate::parts::{Press, hairline, lip};

/// As many files as show before the list scrolls.
const FILES_HIGH: f32 = 180.;

/// What's beside a conversation of what it changed. It outlives the panel:
/// a comment is yours until you send it.
pub(super) struct Review {
    span: Span,
    changes: Option<Changes>,
    picked: Option<String>,
    /// The picked file's lines, and the patch they're from.
    diff: Option<Entity<DiffState>>,
    shown: String,
    comments: Vec<Comment>,
    writing: Option<Writing>,
    /// How much the conversation has changed, for the header.
    totals: Totals,
    /// Lines that haven't had the frame after their first.
    unmeasured: bool,
    /// The newest answers taken: one overtaken since is out of date.
    counted: u64,
    listed: u64,
}

impl Default for Review {
    fn default() -> Review {
        Review {
            span: Span::Turn,
            changes: None,
            picked: None,
            diff: None,
            shown: String::new(),
            comments: Vec::new(),
            writing: None,
            totals: Totals::default(),
            unmeasured: false,
            counted: 0,
            listed: 0,
        }
    }
}

/// A comment being written.
struct Writing {
    range: DiffLineRange,
    input: Entity<InputState>,
    /// The line as it was when you opened this: by the time you're done, a
    /// turn may have moved another into its place.
    quote: Option<String>,
}

const fn side_of(side: DiffSide) -> Side {
    match side {
        DiffSide::Original => Side::Old,
        DiffSide::Modified => Side::New,
    }
}

const fn drawn_side(side: Side) -> DiffSide {
    match side {
        Side::Old => DiffSide::Original,
        Side::New => DiffSide::Modified,
    }
}

/// Where `comment` shows.
fn place(comment: &Comment) -> DiffLinePosition {
    DiffLinePosition::new(comment.path.clone(), drawn_side(comment.side), comment.last)
}

impl Pane {
    /// Asks how much the conversation has changed.
    pub(super) fn ask_totals(&mut self) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_changes(self.next_request, Span::Conversation, false)) {
            self.sent.insert(self.next_request, Sent::Totals);
        }
    }

    /// Asks what the span in view changed, with its lines.
    pub(super) fn ask_changes(&mut self) {
        let span = self.review.span;
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_changes(self.next_request, span, true)) {
            self.sent.insert(self.next_request, Sent::Changes(span));
        }
    }

    /// A turn ended or was undone, so what's changed may have too.
    pub(super) fn files_may_have_changed(&mut self) {
        self.ask_totals();
        if self.panel_shows(Tab::Changes) {
            self.ask_changes();
        }
    }

    /// A step ended, so an open tab moves on with the turn, a look at a
    /// time. The turn's end looks again whatever this missed.
    pub(super) fn step_over(&mut self) {
        if self.panel_shows(Tab::Changes) && !self.sent.values().any(|sent| matches!(sent, Sent::Changes(_))) {
            self.files_may_have_changed();
        }
    }

    pub(super) fn totals_came(&mut self, id: u64, said: &Value, cx: &mut Context<Self>) {
        if id < self.review.counted {
            return;
        }
        self.review.counted = id;
        self.review.totals = Changes::of(said).totals();
        cx.notify();
    }

    /// What was asked for has come. An answer for the span you've left is
    /// nothing you're looking at.
    pub(super) fn changes_came(&mut self, id: u64, span: Span, said: &Value, cx: &mut Context<Self>) {
        if span != self.review.span || id < self.review.listed {
            return;
        }
        self.review.listed = id;
        let changes = Changes::of(said);
        if !self.review.picked.as_ref().is_some_and(|path| changes.file(path).is_some()) {
            self.review.picked = changes.files.first().map(|file| file.path.clone());
        }
        self.review.changes = Some(changes);
        self.show_picked(cx);
        cx.notify();
    }

    /// The file that's picked, as it was last said.
    fn picked_file(&self) -> Option<&ChangedFile> {
        self.review.changes.as_ref()?.file(self.review.picked.as_ref()?)
    }

    /// Shows the picked file's lines, if they've changed: setting them
    /// again would lose your place in them.
    fn show_picked(&mut self, cx: &mut Context<Self>) {
        let patch = self.picked_file().and_then(|file| file.patch.clone()).unwrap_or_default();
        if self.review.diff.is_some() && self.review.shown == patch {
            return;
        }
        let files = DiffFile::parse(&patch).unwrap_or_default();
        match &self.review.diff {
            Some(diff) => diff.update(cx, |diff, cx| diff.set_files(files, cx)),
            None => self.review.diff = Some(cx.new(|cx| DiffState::new(files, cx))),
        }
        self.review.shown = patch;
        self.review.unmeasured = true;
    }

    /// New lines take a second frame: they measure themselves as they're
    /// drawn, and the toolkit asks for the next from inside that one, where
    /// GPUI drops the ask. They'd stay blank until something else drew, and
    /// a press on the header's count leaves nothing that does.
    pub(super) fn settle_review(&mut self, window: &Window) {
        if self.review.unmeasured && self.panel_shows(Tab::Changes) {
            self.review.unmeasured = false;
            window.request_animation_frame();
        }
    }

    fn pick_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.review.picked = Some(path);
        self.show_picked(cx);
        cx.notify();
    }

    fn look_at(&mut self, span: Span, cx: &mut Context<Self>) {
        if self.review.span != span {
            self.review.span = span;
            self.review.changes = None;
            self.ask_changes();
            cx.notify();
        }
    }

    /// Opens a box under `range` for what you have to say.
    fn start_comment(&mut self, range: DiffLineRange, window: &mut Window, cx: &mut Context<Self>) {
        // What's half-written isn't thrown away for another.
        self.keep_comment(window, cx);
        let side = side_of(range.end_side());
        let quote = self.review.changes.as_ref().and_then(|changes| changes.file(range.path().as_ref())?.line(side, range.end())).map(str::to_owned);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Say what to change"));
        cx.subscribe_in(&input, window, |pane, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary, .. } = event {
                pane.keep_comment(window, cx);
                if *secondary {
                    pane.send_comments(cx);
                }
            }
        })
        .detach();
        input.update(cx, |input, cx| input.focus(window, cx));
        self.review.writing = Some(Writing { range, input, quote });
        cx.notify();
    }

    /// Keeps what's in the box, if anything is.
    fn keep_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Writing { range, input, quote }) = self.review.writing.take() else { return };
        let text = input.read(cx).value().to_string();
        if !text.trim().is_empty() {
            self.comment_on(&range, quote, text);
        }
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn drop_writing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review.writing = None;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn comment_on(&mut self, range: &DiffLineRange, quote: Option<String>, text: String) {
        let side = side_of(range.end_side());
        let (path, last) = (range.path().to_string(), range.end());
        let first = if range.side() == range.end_side() { range.start() } else { last };
        self.review.comments.push(Comment { path, side, first, last, quote, text });
    }

    /// Takes back what you said. A second press, as the first takes it
    /// away, finds nothing there.
    fn drop_comment(&mut self, at: usize, cx: &mut Context<Self>) {
        if at < self.review.comments.len() {
            self.review.comments.remove(at);
            cx.notify();
        }
    }

    /// The box a comment is being written in.
    pub(super) fn comment_box(&self) -> Option<&Entity<InputState>> {
        self.review.writing.as_ref().map(|writing| &writing.input)
    }

    /// Comments with nowhere to show in what's listed: their line or their
    /// file has gone from it. They'd still count, and still be sent.
    fn loose_comments(&self) -> Vec<(usize, &Comment)> {
        let Some(changes) = &self.review.changes else { return Vec::new() };
        self.review.comments.iter().enumerate().filter(|(_, comment)| changes.file(&comment.path).and_then(|file| file.line(comment.side, comment.last)).is_none()).collect()
    }

    /// What's said where there's nothing to list.
    fn nothing_said(&self) -> Option<&'static str> {
        let changes = self.review.changes.as_ref().filter(|changes| changes.files.is_empty())?;
        Some(match (self.review.span, changes.turn) {
            (Span::Turn, Some(_)) => "The last turn left every file as it was.",
            (Span::Turn, None) => "No turn has changed any files yet.",
            (Span::Conversation, _) => "Nothing's changed in this conversation yet.",
        })
    }

    /// The turn on offer to undo: none with nothing to put back.
    fn turn_to_undo(&self) -> Option<usize> {
        let changes = self.review.changes.as_ref().filter(|changes| !changes.files.is_empty())?;
        changes.turn.filter(|_| !self.running)
    }

    /// Sends what you said as your own message: it starts a turn, or goes
    /// in at this one's next step, as anything you write does. With no
    /// agent left to hear it, it stays yours.
    fn send_comments(&mut self, cx: &mut Context<Self>) {
        if self.agent.is_none() {
            return;
        }
        if let Some(said) = comments_said(&self.review.comments) {
            self.review.comments.clear();
            self.run(&said, Vec::new(), cx);
        }
    }

    /// How many files the conversation has changed.
    pub(super) const fn files_changed(&self) -> usize {
        self.review.totals.files
    }

    /// How much the conversation changed, in the pane's header.
    pub(super) fn changes_button(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let totals = Some(self.review.totals).filter(|totals| totals.files > 0)?;
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let count = |text: String, color: Hsla| div().font_family(mono.clone()).text_size(px(12.)).text_color(color).child(text);
        let name = format!("Changes: {}, {} lines added, {} taken away", totals.files_said(), totals.added, totals.removed);
        Some(
            Press::new("changes")
                .ghost()
                .small()
                .lit(self.panel_shows(Tab::Changes))
                .named(name)
                .child(totals.files_said())
                .child(count(format!("+{}", totals.added), look.add))
                .when(totals.removed > 0, |it| it.child(count(format!("−{}", totals.removed), look.del)))
                .on_click(cx.listener(|pane, _, _, cx| pane.open_panel(Tab::Changes, cx))),
        )
    }

    /// A row of the list of files.
    fn file_row(&self, at: usize, file: &ChangedFile, look: &Look, mono: &SharedString, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let on = self.review.picked.as_ref() == Some(&file.path);
        let path = file.path.clone();
        let count = |text: String, color: Hsla| div().flex_none().font_family(mono.clone()).text_size(px(12.)).text_color(color).child(text);
        div()
            .id(("changed-file", at))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(8.))
            .rounded(px(7.))
            .when(on, |it| it.bg(look.teal_tint))
            .when(!on, |it| it.hover(|style| style.bg(look.hover)))
            .on_click(cx.listener(move |pane, _, _, cx| pane.pick_file(path.clone(), cx)))
            .child(div().flex_1().min_w_0().truncate().font_family(mono.clone()).text_size(px(12.)).child(file.path.clone()))
            .when(!file.tag().is_empty(), |it| it.child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(file.tag())))
            .when(file.added > 0, |it| it.child(count(format!("+{}", file.added), look.add)))
            .when(file.removed > 0, |it| it.child(count(format!("−{}", file.removed), look.del)))
    }

    /// One of the two spans to look at.
    fn span_button(&self, span: Span, name: &'static str, look: &Look, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let on = self.review.span == span;
        div()
            .id(name)
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .h(px(24.))
            .rounded(px(6.))
            .text_size(px(12.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(if on { look.ink } else { look.ink_2 })
            .when(on, |it| it.bg(look.bg).shadow(vec![hairline(look.ring), lip(look.lip)]))
            .when(!on, |it| it.hover(|style| style.bg(look.hover)))
            .on_click(cx.listener(move |pane, _, _, cx| pane.look_at(span, cx)))
            .child(name)
    }

    /// The lines of the picked file, and what's been said of them.
    fn lines(&self, look: &Look, cx: &mut Context<Self>) -> AnyElement {
        let quiet = |words: &'static str| div().p(px(16.)).text_size(px(13.)).text_color(look.ink_3).child(words).into_any_element();
        let Some(file) = self.picked_file() else { return div().into_any_element() };
        if let Some(why) = file.unshown {
            return quiet(why.says());
        }
        let Some(diff) = self.review.diff.as_ref().filter(|diff| !diff.read(cx).files().is_empty()) else {
            return quiet("These lines can't be shown.");
        };
        let path = file.path.clone();
        let on_this: Vec<(usize, &Comment)> = self.review.comments.iter().enumerate().filter(|(_, comment)| comment.path == path).collect();
        let mut notes: Vec<DiffAnnotation> = on_this.iter().map(|(at, comment)| DiffAnnotation::line(("comment", *at), place(comment))).collect();
        let writing = self.review.writing.as_ref().filter(|writing| *writing.range.path() == *path).map(|writing| {
            let position = DiffLinePosition::new(path.clone(), writing.range.end_side(), writing.range.end());
            notes.push(DiffAnnotation::line("writing", position));
            writing.input.clone()
        });
        let said: Vec<(usize, String)> = on_this.iter().map(|(at, comment)| (*at, comment.text.clone())).collect();
        let (pane, card_look) = (cx.weak_entity(), *look);
        let adds = cx.weak_entity();
        Diff::new(diff)
            .size_full()
            // It sits flush in the panel, whose lines are its edges.
            .border_0()
            .header_visible(false)
            .soft_wrap(true)
            .hunk_separator(DiffHunkSeparator::Metadata)
            .change_indicator(DiffChangeIndicator::None)
            .hover_highlight(DiffHoverHighlight::Line)
            .annotations(notes)
            .on_add_annotation(move |range, window, cx| {
                let _ = adds.update(cx, |pane, cx| pane.start_comment(range.clone(), window, cx));
            })
            .render_annotation(move |note, _, _| note_card(note, &said, writing.as_ref(), &pane, &card_look))
            .into_any_element()
    }

    /// The Changes tab.
    pub(super) fn changes_tab(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let files = self.review.changes.as_ref().map_or(&[][..], |changes| &changes.files);
        let rows: Vec<_> = files.iter().enumerate().map(|(at, file)| self.file_row(at, file, &look, &mono, cx)).collect();
        let spans = div().flex().gap(px(2.)).p(px(2.)).rounded(px(8.)).bg(look.lit).child(self.span_button(Span::Turn, "This turn", &look, cx)).child(self.span_button(
            Span::Conversation,
            "Whole conversation",
            &look,
            cx,
        ));
        let top = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(8.))
            .pt(px(10.))
            .px(px(12.))
            .pb(px(8.))
            .border_b_1()
            .border_color(look.line)
            .child(spans)
            .child(div().id("changed-files").max_h(px(FILES_HIGH)).overflow_y_scroll().flex().flex_col().children(rows));
        let nothing = self.nothing_said().map(|words| div().p(px(16.)).text_size(px(13.)).text_color(look.ink_3).child(words));
        let loose: Vec<_> = self.loose_comments().into_iter().map(|(at, comment)| Self::loose_row(at, comment, &look, cx)).collect();
        let elsewhere = (!loose.is_empty()).then(|| {
            div()
                .id("loose-comments")
                .flex_none()
                .flex()
                .flex_col()
                .max_h(px(FILES_HIGH))
                .overflow_y_scroll()
                .py(px(4.))
                .pl(px(20.))
                .pr(px(8.))
                .border_t_1()
                .border_color(look.line)
                .children(loose)
        });
        let undo = self.turn_to_undo().map(|turn| {
            let pressed = cx.listener(move |pane, _, _, cx| pane.open_undo(Some(turn), cx));
            Press::new("undo-this-turn").ghost().small().label("Undo this turn").on_click(pressed)
        });
        let count = self.review.comments.len();
        let said = (count > 0).then(|| div().text_size(px(12.)).text_color(look.ink_3).child(if count == 1 { "1 comment".to_owned() } else { format!("{count} comments") }));
        let send = (count > 0 && self.agent.is_some())
            .then(|| Press::new("send-comments").primary().small().label("Send to rein").on_click(cx.listener(|pane, _, _, cx| pane.send_comments(cx))));
        let foot = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .py(px(10.))
            .px(px(12.))
            .border_t_1()
            .border_color(look.line)
            .children(undo)
            .child(div().flex_1())
            .children(said)
            .children(send);
        div().size_full().flex().flex_col().child(top).child(div().flex_1().min_h_0().children(nothing).child(self.lines(&look, cx))).children(elsewhere).child(foot)
    }

    /// A comment with nowhere to show in the lines: you can still read it,
    /// and take it back.
    fn loose_row(at: usize, comment: &Comment, look: &Look, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let remove =
            Press::new(("loose-remove", at)).ghost().square().named("Remove this comment").child("×").on_click(cx.listener(move |pane, _, _, cx| pane.drop_comment(at, cx)));
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .min_h(px(30.))
            .text_size(px(12.))
            .child(div().flex_none().text_color(look.ink_3).child(comment.place()))
            .child(div().flex_1().min_w_0().truncate().child(comment.text.clone()))
            .child(remove)
    }
}

/// What `note` shows: something you said, or the box to say it in.
fn note_card(note: &DiffAnnotation, said: &[(usize, String)], writing: Option<&Entity<InputState>>, pane: &WeakEntity<Pane>, look: &Look) -> AnyElement {
    let card = div()
        .mt(px(6.))
        .mr(px(12.))
        .mb(px(8.))
        .ml(px(34.))
        .py(px(8.))
        .px(px(10.))
        .rounded(px(8.))
        .bg(look.float)
        .shadow(vec![hairline(look.ring), lip(look.lip)])
        .text_size(px(13.));
    if let ElementId::NamedInteger(_, at) = note.id()
        && let Some((at, text)) = said.iter().find(|(index, _)| *index as u64 == *at)
    {
        let (at, pane) = (*at, pane.clone());
        let remove = Press::new(("comment-remove", at)).ghost().square().named("Remove this comment").child("×").on_click(move |_, _, cx| {
            let _ = pane.update(cx, |pane, cx| pane.drop_comment(at, cx));
        });
        let head = div().flex().items_center().justify_between().mb(px(2.)).text_size(px(12.)).text_color(look.ink_3).child("You, on this line").child(remove);
        return card.child(head).child(text.clone()).into_any_element();
    }
    let Some(input) = writing else { return div().into_any_element() };
    let pane = pane.clone();
    card.on_key_down(move |event, window, cx| {
        if event.keystroke.key == "escape" {
            cx.stop_propagation();
            let _ = pane.update(cx, |pane, cx| pane.drop_writing(window, cx));
        }
    })
    .child(Input::new(input).appearance(false).text_size(px(13.)).px(px(0.)))
    .child(div().mt(px(4.)).text_size(px(12.)).text_color(look.ink_3).child("⏎ keep · ⌘⏎ send all · esc leave"))
    .into_any_element()
}

#[cfg(test)]
impl Pane {
    pub(crate) const fn totals_for_test(&self) -> Totals {
        self.review.totals
    }

    pub(crate) fn picked_for_test(&self) -> Option<String> {
        self.review.picked.clone()
    }

    pub(crate) fn files_listed_for_test(&self) -> Vec<String> {
        self.review.changes.iter().flat_map(|changes| &changes.files).map(|file| file.path.clone()).collect()
    }

    pub(crate) fn lines_shown_for_test(&self, cx: &App) -> Option<(String, usize)> {
        let diff = self.review.diff.as_ref()?.read(cx);
        diff.files().first().map(|file| (file.path().to_string(), file.additions()))
    }

    pub(crate) fn pick_file_for_test(&mut self, path: &str, cx: &mut Context<Self>) {
        self.pick_file(path.to_owned(), cx);
    }

    pub(crate) fn look_at_for_test(&mut self, span: Span, cx: &mut Context<Self>) {
        self.look_at(span, cx);
    }

    pub(crate) fn start_comment_for_test(&mut self, path: &str, side: Side, first: usize, last: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.start_comment(DiffLineRange::new(path.to_owned(), drawn_side(side), first, last), window, cx);
    }

    pub(crate) fn write_comment_for_test(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(writing) = &self.review.writing {
            writing.input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
    }

    pub(crate) fn keep_comment_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.keep_comment(window, cx);
    }

    pub(crate) fn drop_comment_for_test(&mut self, at: usize, cx: &mut Context<Self>) {
        self.drop_comment(at, cx);
    }

    /// As if Enter were pressed in the box, with `⌘` or without.
    pub(crate) fn press_enter_in_comment_for_test(&mut self, secondary: bool, cx: &mut Context<Self>) {
        if let Some(writing) = &self.review.writing {
            writing.input.update(cx, |_, cx| cx.emit(InputEvent::PressEnter { secondary, shift: false }));
        }
    }

    /// A comment from a line on one side to a line on the other.
    pub(crate) fn start_comment_across_for_test(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let range = DiffLineRange::new(path.to_owned(), DiffSide::Original, 2, 3).with_end_side(DiffSide::Modified);
        self.start_comment(range, window, cx);
    }

    pub(crate) fn drop_writing_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drop_writing(window, cx);
    }

    pub(crate) fn comment_box_for_test(&self, cx: &App) -> Option<String> {
        self.comment_box().map(|input| input.read(cx).value().to_string())
    }

    pub(crate) fn comment_has_keys_for_test(&self, window: &Window, cx: &App) -> bool {
        self.comment_box().is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    pub(crate) fn loose_for_test(&self) -> Vec<String> {
        self.loose_comments().into_iter().map(|(_, comment)| comment.path.clone()).collect()
    }

    pub(crate) fn nothing_said_for_test(&self) -> Option<&'static str> {
        self.nothing_said()
    }

    pub(crate) fn turn_to_undo_for_test(&self) -> Option<usize> {
        self.turn_to_undo()
    }

    pub(crate) fn ask_changes_for_test(&mut self) {
        self.ask_changes();
    }

    pub(crate) fn shows_changes_for_test(&self) -> bool {
        self.panel_shows(Tab::Changes)
    }

    pub(crate) fn comments_for_test(&self) -> Vec<Comment> {
        self.review.comments.clone()
    }

    pub(crate) fn send_comments_for_test(&mut self, cx: &mut Context<Self>) {
        self.send_comments(cx);
    }
}
