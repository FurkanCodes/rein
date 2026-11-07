//! What a conversation changed: the count in the header, the Changes tab,
//! and comments, on what rein says.

use gpui_kit::component::diff::DiffFile;
use rein_transcript::changes::{Comment, Side, Span, Totals};

use super::*;

const CHANGES: &str = "_io.github.furkancodes/changes";
const PATCH_A: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
const PATCH_B: &str = "diff --git a/b.rs b/b.rs\nnew file mode 100644\n--- /dev/null\n+++ b/b.rs\n@@ -0,0 +1,2 @@\n+made\n+here\n";

fn files(patches: bool) -> Value {
    let patch = |patch: &str| if patches { json!(patch) } else { Value::Null };
    json!({ "turn": 2, "files": [
        { "path": "a.rs", "became": "changed", "added": 1, "removed": 1, "unshown": null, "since": false, "patch": patch(PATCH_A) },
        { "path": "b.rs", "became": "made", "added": 2, "removed": 0, "unshown": null, "since": false, "patch": patch(PATCH_B) },
    ] })
}

/// What it was asked of what changed, in order.
fn asked_changes(file: &Path) -> Vec<Value> {
    sent_to(file).into_iter().filter(|line| line["method"] == CHANGES).map(|line| line["params"].clone()).collect()
}

/// The number of the `count`th time it was asked what changed.
fn wait_for_changes(file: &Path, count: usize) -> u64 {
    asks_of(file, CHANGES, count);
    asked_for(file, CHANGES)[count - 1]
}

/// A pane with the Changes tab open, told what `span` changed.
fn reviewing(desk: &mut Desk, file: &Path) -> Entity<Pane> {
    let pane = desk.pane(file);
    let first = wait_for_changes(file, 1);
    told(desk, &pane, first, files(false));
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Changes, cx)));
    desk.cx.run_until_parked();
    let shown = wait_for_changes(file, 2);
    told(desk, &pane, shown, files(true));
    pane
}

fn comment(desk: &mut Desk, pane: &Entity<Pane>, path: &'static str, side: Side, line: usize, text: &'static str) {
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.start_comment_for_test(path, side, line, line, window, cx);
            pane.write_comment_for_test(text, window, cx);
            pane.keep_comment_for_test(window, cx);
        });
    });
    desk.cx.run_until_parked();
}

/// New lines measure themselves as they're drawn, and the toolkit asks for
/// the frame after from inside that one, where GPUI drops the ask: Changes
/// opened blank, and stayed so until something else drew.
#[test]
fn lines_new_to_the_panel_ask_for_the_frame_that_shows_them() {
    let mut desk = desk();
    let file = scratch("second-frame");
    let pane = reviewing(&mut desk, &file);
    desk.cx.run_until_parked();
    let frames = |desk: &mut Desk| desk.cx.update(|window, cx| window.simulate_next_frame(cx));
    assert!(frames(&mut desk) > 0, "the frame that shows them was asked for");
    desk.cx.run_until_parked();
    frames(&mut desk);
    desk.cx.update(|_, cx| pane.update(cx, |_, cx| cx.notify()));
    desk.cx.run_until_parked();
    assert_eq!(frames(&mut desk), 0, "once is enough: an idle window draws nothing");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.pick_file_for_test("b.rs", cx)));
    desk.cx.run_until_parked();
    assert!(frames(&mut desk) > 0, "a file's lines are new too");
}

/// Opens a box on `line` of `path`, and leaves `text` in it.
fn half_written(desk: &mut Desk, pane: &Entity<Pane>, path: &'static str, line: usize, text: &'static str) {
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.start_comment_for_test(path, Side::New, line, line, window, cx);
            pane.write_comment_for_test(text, window, cx);
        });
    });
    desk.cx.run_until_parked();
}

fn comment_has_keys(desk: &mut Desk, pane: &Entity<Pane>) -> bool {
    desk.cx.update(|window, cx| pane.read(cx).comment_has_keys_for_test(window, cx))
}

/// Tells the pane, once it asks again, that only a.rs is listed, as `patch`.
fn relisted(desk: &mut Desk, pane: &Entity<Pane>, file: &Path, patch: &str) {
    let asked = asked_changes(file).len();
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.ask_changes_for_test()));
    let id = wait_for_changes(file, asked + 1);
    told(desk, pane, id, json!({ "turn": 2, "files": [{ "path": "a.rs", "became": "changed", "added": 1, "removed": 1, "unshown": null, "since": false, "patch": patch }] }));
}

#[test]
fn a_question_leaves_the_keys_with_a_comment_half_written() {
    let mut desk = desk();
    let file = scratch("comment-keys");
    let pane = reviewing(&mut desk, &file);
    half_written(&mut desk, &pane, "a.rs", 2, "not ");
    desk.asked(&pane, 40, "git push");
    desk.cx.run_until_parked();
    assert!(comment_has_keys(&mut desk, &pane), "they stay with what you're writing");
    desk.key("y");
    desk.cx.run_until_parked();
    assert!(desk.read(&pane, Pane::is_asking), "a y meant for a comment says yes to nothing");
    assert!(!sent_to(&file).iter().any(|line| line["id"] == 40), "nothing was answered");
}

#[test]
fn a_command_asking_leaves_the_keys_with_a_comment_half_written() {
    let mut desk = desk();
    let file = scratch("comment-yes");
    let pane = reviewing(&mut desk, &file);
    half_written(&mut desk, &pane, "a.rs", 2, "not ");
    desk.said(&pane, vec![yes_asked(41)]);
    desk.cx.run_until_parked();
    assert!(comment_has_keys(&mut desk, &pane), "they stay with what you're writing");
    desk.key("y");
    desk.cx.run_until_parked();
    assert!(!sent_to(&file).iter().any(|line| line["id"] == 41), "nothing was answered");
}

#[test]
fn a_question_takes_the_keys_from_a_comment_box_with_nothing_in_it() {
    let mut desk = desk();
    let file = scratch("comment-empty");
    let pane = reviewing(&mut desk, &file);
    half_written(&mut desk, &pane, "a.rs", 2, "");
    desk.asked(&pane, 40, "git push");
    desk.cx.run_until_parked();
    assert!(!comment_has_keys(&mut desk, &pane), "there's nothing there to lose");
}

#[test]
fn a_comment_quotes_the_line_it_was_opened_on_however_the_lines_move() {
    let mut desk = desk();
    let file = scratch("comment-quote");
    let pane = reviewing(&mut desk, &file);
    half_written(&mut desk, &pane, "a.rs", 1, "why this?");
    relisted(&mut desk, &pane, &file, "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n moved in\n-two\n+TWO\n three\n");
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.keep_comment_for_test(window, cx)));
    let quote = desk.read(&pane, Pane::comments_for_test)[0].quote.clone();
    assert_eq!(quote.as_deref(), Some("one"));
}

#[test]
fn comments_stay_yours_once_the_agent_has_ended() {
    let mut desk = desk();
    let file = scratch("comment-ended");
    let pane = reviewing(&mut desk, &file);
    comment(&mut desk, &pane, "a.rs", Side::New, 2, "Why?");
    desk.said(&pane, vec![Said::Ended]);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.send_comments_for_test(cx)));
    assert_eq!(desk.read(&pane, Pane::comments_for_test).len(), 1, "nobody heard it, so it isn't sent");
}

#[test]
fn opening_another_comment_keeps_the_one_half_written() {
    let mut desk = desk();
    let file = scratch("comment-second");
    let pane = reviewing(&mut desk, &file);
    half_written(&mut desk, &pane, "a.rs", 1, "first thought");
    half_written(&mut desk, &pane, "a.rs", 2, "second");
    let said: Vec<String> = desk.read(&pane, Pane::comments_for_test).into_iter().map(|comment| comment.text).collect();
    assert_eq!(said, ["first thought"]);
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).comment_box_for_test(cx)).as_deref(), Some("second"));
}

#[test]
fn a_comment_that_has_lost_its_line_can_still_be_read_and_taken_back() {
    let mut desk = desk();
    let file = scratch("comment-loose");
    let pane = reviewing(&mut desk, &file);
    comment(&mut desk, &pane, "b.rs", Side::New, 1, "Why made?");
    assert_eq!(desk.read(&pane, Pane::loose_for_test), Vec::<String>::new(), "its line is there, in a file you can pick");
    relisted(&mut desk, &pane, &file, PATCH_A);
    assert_eq!(desk.read(&pane, Pane::loose_for_test), ["b.rs"]);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.drop_comment_for_test(0, cx)));
    assert_eq!(desk.read(&pane, Pane::comments_for_test).len(), 0);
}

#[test]
fn a_turn_that_put_everything_back_isnt_called_no_turn_and_offers_no_undo() {
    let mut desk = desk();
    let file = scratch("came-back");
    let pane = reviewing(&mut desk, &file);
    assert_eq!(desk.read(&pane, Pane::turn_to_undo_for_test), Some(2));
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.ask_changes_for_test()));
    let id = wait_for_changes(&file, 3);
    told(&mut desk, &pane, id, json!({ "turn": 2, "files": [] }));
    assert_eq!(desk.read(&pane, Pane::nothing_said_for_test), Some("The last turn left every file as it was."));
    assert_eq!(desk.read(&pane, Pane::turn_to_undo_for_test), None, "there's nothing to put back");
}

#[test]
fn the_open_tab_follows_a_turn_a_look_at_a_time() {
    let mut desk = desk();
    let file = scratch("following");
    let pane = reviewing(&mut desk, &file);
    desk.send(&pane, "go");
    wait_for_ask(&file, "session/prompt");
    let over = |id: &str| Said::Update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed" }));
    desk.said(&pane, vec![over("a")]);
    let look = wait_for_changes(&file, 4);
    assert_eq!(asked_changes(&file)[3]["lines"], true, "{:?}", asked_changes(&file));
    desk.said(&pane, vec![over("b")]);
    // Asked in order, so what's behind this reached it first.
    desk.send(&pane, "first marker");
    asks_of(&file, STEER, 1);
    assert_eq!(asked_changes(&file).len(), 4, "one look at a time");
    told(&mut desk, &pane, look, files(true));
    desk.said(&pane, vec![over("c")]);
    assert_eq!(asks_of(&file, CHANGES, 6), 6, "and the next once that's in");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Changes, cx)));
    desk.cx.run_until_parked();
    let sent = wait_for_changes(&file, 6);
    told(&mut desk, &pane, sent, files(true));
    desk.said(&pane, vec![over("d")]);
    desk.send(&pane, "second marker");
    asks_of(&file, STEER, 2);
    assert_eq!(asked_changes(&file).len(), 6, "with the tab closed nobody's looking");
}

#[test]
fn an_answer_that_has_been_overtaken_is_out_of_date() {
    let mut desk = desk();
    let file = scratch("overtaken");
    let pane = reviewing(&mut desk, &file);
    desk.cx.update(|_, cx| {
        pane.update(cx, |pane, _| {
            pane.ask_changes_for_test();
            pane.ask_changes_for_test();
        });
    });
    let (older, newer) = (wait_for_changes(&file, 3), wait_for_changes(&file, 4));
    let only_a = json!({ "turn": 2, "files": [{ "path": "a.rs", "became": "changed", "added": 1, "removed": 1, "unshown": null, "since": false, "patch": PATCH_A }] });
    told(&mut desk, &pane, newer, only_a);
    told(&mut desk, &pane, older, files(true));
    assert_eq!(desk.read(&pane, Pane::files_listed_for_test), ["a.rs"]);
}

#[test]
fn changes_opens_during_a_turn_with_no_waiting() {
    let mut desk = desk();
    let file = scratch("changes-now");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    wait_for_ask(&file, "session/prompt");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_for_test("/changes", cx)));
    desk.cx.run_until_parked();
    assert!(desk.read(&pane, Pane::shows_changes_for_test), "looking needs no turn to end");
}

#[test]
fn a_new_pane_and_each_turn_ask_how_much_changed_without_the_lines() {
    let mut desk = desk();
    let file = scratch("totals");
    let pane = desk.pane(&file);
    let first = wait_for_changes(&file, 1);
    assert_eq!(asked_changes(&file)[0]["span"], "conversation", "a saved conversation has changed things already");
    assert_eq!(asked_changes(&file)[0]["lines"], false, "the count has no use for them");
    assert_eq!(desk.read(&pane, Pane::totals_for_test).files, 0);
    told(&mut desk, &pane, first, files(false));
    assert_eq!(desk.read(&pane, Pane::totals_for_test), Totals { files: 2, added: 3, removed: 1 });

    desk.send(&pane, "go");
    let turn = wait_for_ask(&file, "session/prompt");
    answers(&mut desk, &pane, turn, CameTo::TurnOver);
    assert_eq!(asks_of(&file, CHANGES, 2), 2, "a turn may have changed more");
}

#[test]
fn nothing_changed_shows_no_count() {
    let mut desk = desk();
    let file = scratch("no-totals");
    let pane = desk.pane(&file);
    let first = wait_for_changes(&file, 1);
    told(&mut desk, &pane, first, json!({ "turn": null, "files": [] }));
    assert_eq!(desk.read(&pane, Pane::totals_for_test).files, 0);
}

#[test]
fn the_tab_asks_for_the_last_turn_and_shows_the_first_files_lines() {
    let mut desk = desk();
    let file = scratch("tab");
    let pane = reviewing(&mut desk, &file);
    assert_eq!(asked_changes(&file)[1]["span"], "turn");
    assert_eq!(asked_changes(&file)[1]["lines"], true);
    assert_eq!(desk.read(&pane, Pane::files_listed_for_test), ["a.rs", "b.rs"]);
    assert_eq!(desk.read(&pane, Pane::picked_for_test).as_deref(), Some("a.rs"));
    let shown = desk.cx.update(|_, cx| pane.read(cx).lines_shown_for_test(cx));
    assert_eq!(shown, Some(("a.rs".to_owned(), 1)));
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.pick_file_for_test("b.rs", cx)));
    let shown = desk.cx.update(|_, cx| pane.read(cx).lines_shown_for_test(cx));
    assert_eq!(shown, Some(("b.rs".to_owned(), 2)));
}

#[test]
fn the_whole_conversation_is_asked_for_and_an_answer_for_the_span_you_left_is_ignored() {
    let mut desk = desk();
    let file = scratch("span");
    let pane = desk.pane(&file);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Changes, cx)));
    desk.cx.run_until_parked();
    let late = wait_for_changes(&file, 2);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.look_at_for_test(Span::Conversation, cx)));
    let asked = wait_for_changes(&file, 3);
    assert_eq!(asked_changes(&file)[2]["span"], "conversation");
    told(&mut desk, &pane, late, files(true));
    assert_eq!(desk.read(&pane, Pane::files_listed_for_test), [""; 0], "that one's for what you've left");
    told(&mut desk, &pane, asked, files(true));
    assert_eq!(desk.read(&pane, Pane::files_listed_for_test), ["a.rs", "b.rs"]);
}

#[test]
fn what_you_have_picked_stays_picked_as_long_as_its_still_there() {
    let mut desk = desk();
    let file = scratch("picked-stays");
    let pane = reviewing(&mut desk, &file);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.pick_file_for_test("b.rs", cx)));
    desk.send(&pane, "go");
    let turn = wait_for_ask(&file, "session/prompt");
    answers(&mut desk, &pane, turn, CameTo::TurnOver);
    let again = wait_for_changes(&file, 4);
    assert_eq!(asked_changes(&file)[3]["lines"], true, "the tab shows, so what's in it is asked for again");
    told(&mut desk, &pane, again, files(true));
    assert_eq!(desk.read(&pane, Pane::picked_for_test).as_deref(), Some("b.rs"));
    let gone = json!({ "turn": 2, "files": [{ "path": "a.rs", "became": "changed", "added": 1, "removed": 1, "unshown": null, "since": false, "patch": PATCH_A }] });
    desk.send(&pane, "again");
    let second = asked_for(&file, "session/prompt")[1];
    answers(&mut desk, &pane, second, CameTo::TurnOver);
    let last = wait_for_changes(&file, 6);
    told(&mut desk, &pane, last, gone);
    assert_eq!(desk.read(&pane, Pane::picked_for_test).as_deref(), Some("a.rs"));
}

#[test]
fn a_file_with_no_lines_to_show_is_listed_and_breaks_nothing() {
    let mut desk = desk();
    let file = scratch("unshown");
    let pane = desk.pane(&file);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Changes, cx)));
    desk.cx.run_until_parked();
    let shown = wait_for_changes(&file, 2);
    told(
        &mut desk,
        &pane,
        shown,
        json!({ "turn": 0, "files": [
        { "path": "logo.png", "became": "changed", "added": 0, "removed": 0, "unshown": "binary", "since": false, "patch": "" },
        { "path": "odd.rs", "became": "changed", "added": 1, "removed": 0, "unshown": null, "since": false, "patch": "not a patch at all" },
    ] }),
    );
    assert_eq!(desk.read(&pane, Pane::files_listed_for_test), ["logo.png", "odd.rs"]);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.pick_file_for_test("odd.rs", cx)));
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).lines_shown_for_test(cx)), None, "what can't be read isn't guessed at");
}

#[test]
fn a_patch_as_rein_writes_it_reads_even_with_a_space_in_the_path() {
    let patch = "diff --git a/src/my file.rs b/src/my file.rs\n--- a/src/my file.rs\n+++ b/src/my file.rs\n@@ -1 +1 @@\n-one\n+two\n\\ No newline at end of file\n";
    let parsed = DiffFile::parse(patch).expect("it reads");
    assert_eq!(parsed[0].path().as_ref(), "src/my file.rs");
    assert_eq!((parsed[0].additions(), parsed[0].deletions()), (1, 1));
    let empty = "diff --git a/empty.txt b/empty.txt\nnew file mode 100644\n--- /dev/null\n+++ b/empty.txt\n";
    assert_eq!(DiffFile::parse(empty).expect("a made file with nothing in it reads").len(), 1);
}

#[test]
fn a_comment_goes_as_your_words_saying_where_and_on_what() {
    let mut desk = desk();
    let file = scratch("comment");
    let pane = reviewing(&mut desk, &file);
    comment(&mut desk, &pane, "a.rs", Side::New, 2, "Why capitals?");
    comment(&mut desk, &pane, "a.rs", Side::Old, 2, "Keep this one.");
    let kept = desk.read(&pane, Pane::comments_for_test);
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0], Comment { path: "a.rs".to_owned(), side: Side::New, first: 2, last: 2, quote: Some("TWO".to_owned()), text: "Why capitals?".to_owned() });
    assert_eq!(kept[1].quote.as_deref(), Some("two"), "the line that was taken out");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.send_comments_for_test(cx)));
    desk.cx.run_until_parked();
    assert!(desk.read(&pane, Pane::comments_for_test).is_empty());
    let said = desk.read(&pane, Pane::conversation_for_test);
    assert!(said.contains("a.rs, line 2\n> TWO\n\nWhy capitals?"), "it's in the conversation as yours: {said}");
    assert!(said.contains("a.rs, removed line 2\n> two\n\nKeep this one."), "{said}");
    assert!(desk.read(&pane, Pane::is_running), "and rein is on it");
}

#[test]
fn a_comment_sent_while_a_turn_runs_goes_into_it() {
    let mut desk = desk();
    let file = scratch("comment-steer");
    let pane = reviewing(&mut desk, &file);
    desk.send(&pane, "go");
    comment(&mut desk, &pane, "b.rs", Side::New, 1, "Not here.");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.send_comments_for_test(cx)));
    desk.cx.run_until_parked();
    assert_eq!(asks_of(&file, "_io.github.furkancodes/steer", 1), 1, "it needn't wait for the turn to end");
}

#[test]
fn an_empty_comment_keeps_nothing_and_esc_leaves_what_you_wrote() {
    let mut desk = desk();
    let file = scratch("comment-empty");
    let pane = reviewing(&mut desk, &file);
    comment(&mut desk, &pane, "a.rs", Side::New, 2, "   ");
    assert!(desk.read(&pane, Pane::comments_for_test).is_empty());
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.start_comment_for_test("a.rs", Side::New, 2, 2, window, cx);
            pane.write_comment_for_test("half a thought", window, cx);
            pane.drop_writing_for_test(window, cx);
            pane.keep_comment_for_test(window, cx);
        });
    });
    assert!(desk.read(&pane, Pane::comments_for_test).is_empty());
}

#[test]
fn comments_outlast_the_panel() {
    let mut desk = desk();
    let file = scratch("comment-stays");
    let pane = reviewing(&mut desk, &file);
    comment(&mut desk, &pane, "a.rs", Side::New, 2, "Mine.");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Changes, cx)));
    assert_eq!(desk.read(&pane, Pane::panel_tab_for_test), None);
    assert_eq!(desk.read(&pane, Pane::comments_for_test).len(), 1);
}

#[test]
fn a_comment_across_lines_says_them_all() {
    let mut desk = desk();
    let file = scratch("comment-range");
    let pane = reviewing(&mut desk, &file);
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.start_comment_for_test("a.rs", Side::New, 1, 3, window, cx);
            pane.write_comment_for_test("All of this.", window, cx);
            pane.keep_comment_for_test(window, cx);
        });
    });
    let kept = desk.read(&pane, Pane::comments_for_test);
    assert_eq!((kept[0].first, kept[0].last, kept[0].quote.as_deref()), (1, 3, Some("three")));
}

#[test]
fn a_comment_can_be_taken_back_and_a_late_press_takes_nothing() {
    let mut desk = desk();
    let file = scratch("comment-back");
    let pane = reviewing(&mut desk, &file);
    comment(&mut desk, &pane, "a.rs", Side::New, 2, "First.");
    comment(&mut desk, &pane, "a.rs", Side::New, 3, "Second.");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.drop_comment_for_test(0, cx)));
    assert_eq!(desk.read(&pane, Pane::comments_for_test)[0].text, "Second.");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.drop_comment_for_test(1, cx)));
    assert_eq!(desk.read(&pane, Pane::comments_for_test).len(), 1, "that was the second press of a double click");
}

#[test]
fn command_enter_in_a_comment_keeps_it_and_sends_them_all() {
    let mut desk = desk();
    let file = scratch("comment-all");
    let pane = reviewing(&mut desk, &file);
    comment(&mut desk, &pane, "a.rs", Side::New, 2, "First.");
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.start_comment_for_test("a.rs", Side::New, 3, 3, window, cx);
            pane.write_comment_for_test("Second.", window, cx);
            pane.press_enter_in_comment_for_test(true, cx);
        });
    });
    desk.cx.run_until_parked();
    assert!(desk.read(&pane, Pane::comments_for_test).is_empty());
    let said = desk.read(&pane, Pane::conversation_for_test);
    assert!(said.contains("First.") && said.contains("Second."), "{said}");
}

#[test]
fn enter_in_a_comment_keeps_it_and_sends_nothing() {
    let mut desk = desk();
    let file = scratch("comment-keep");
    let pane = reviewing(&mut desk, &file);
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.start_comment_for_test("a.rs", Side::New, 2, 2, window, cx);
            pane.write_comment_for_test("Not yet.", window, cx);
            pane.press_enter_in_comment_for_test(false, cx);
        });
    });
    desk.cx.run_until_parked();
    assert_eq!(desk.read(&pane, Pane::comments_for_test).len(), 1);
    assert!(!desk.read(&pane, Pane::is_running));
}

#[test]
fn a_comment_from_one_side_to_the_other_is_on_the_line_it_ends_on() {
    let mut desk = desk();
    let file = scratch("comment-across");
    let pane = reviewing(&mut desk, &file);
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.start_comment_across_for_test("a.rs", window, cx);
            pane.write_comment_for_test("These two.", window, cx);
            pane.keep_comment_for_test(window, cx);
        });
    });
    let kept = desk.read(&pane, Pane::comments_for_test);
    assert_eq!((kept[0].side, kept[0].first, kept[0].last), (Side::New, 3, 3), "line 2 of one side and 3 of the other aren't a range");
}

#[test]
fn a_turn_that_failed_may_have_changed_files_too() {
    let mut desk = desk();
    let file = scratch("failed-totals");
    let pane = desk.pane(&file);
    wait_for_changes(&file, 1);
    desk.send(&pane, "go");
    let turn = wait_for_ask(&file, "session/prompt");
    answers(&mut desk, &pane, turn, CameTo::Refused("the model went away".to_owned()));
    assert_eq!(asks_of(&file, CHANGES, 2), 2);
}

#[test]
fn the_list_of_everything_has_changes() {
    let mut desk = desk();
    let file = scratch("changes-listed");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![Said::Update(json!({ "sessionUpdate": "available_commands_update", "availableCommands": [{ "name": "changes", "description": "what changed" }] }))]);
    desk.key("cmd-k");
    desk.cx.run_until_parked();
    let panes = desk.panes.clone();
    let names = desk.cx.update(|_, cx| panes.read(cx).palette_names_for_test(cx));
    assert!(names.contains(&"Changes".to_owned()), "under the name people look for: {names:?}");
}

#[test]
fn the_box_says_changes_and_the_list_of_everything_has_it() {
    let mut desk = desk();
    let file = scratch("changes-command");
    let pane = desk.pane(&file);
    desk.box_says(&pane, "/changes");
    assert_eq!(desk.read(&pane, Pane::panel_tab_for_test), Some(Tab::Changes), "it opens the tab rather than saying it in the conversation");
    assert_eq!(desk.read(&pane, Pane::rows_for_test), 0);
    let_go(&mut desk, &pane);
    assert!(sent_to(&file).iter().all(|line| line["method"] != "session/prompt"), "the window's own");
}

#[test]
fn undoing_a_turn_asks_how_much_changed_again() {
    let mut desk = desk();
    let file = scratch("undo-totals");
    let pane = reviewing(&mut desk, &file);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_undo_for_test(Some(2), cx)));
    desk.cx.run_until_parked();
    let turns = wait_for_ask(&file, UNDO_TURNS);
    told(&mut desk, &pane, turns, json!({ "turns": [{ "turn": 2, "said": "go", "at": 0, "changed": 1, "made": 1, "removed": 0 }] }));
    let plan = wait_for_ask(&file, UNDO_PLAN);
    told(&mut desk, &pane, plan, json!({ "plan": { "putBack": ["a.rs"], "takeAway": ["b.rs"], "left": [], "lost": [], "said": "go", "turns": 1 } }));
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.undo_now_for_test()));
    let undo = wait_for_ask(&file, "_io.github.furkancodes/undo");
    answers(&mut desk, &pane, undo, CameTo::Done);
    assert_eq!(asks_of(&file, CHANGES, 4), 4, "the count, and the tab that's showing");
}
