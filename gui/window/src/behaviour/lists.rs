//! The lists that open from the box and the bar: the model and the mode,
//! what `/` and `@` list, and ⌘K. Each is pressed as you would, where it's
//! drawn, on the first run and in a conversation.

use gpui_kit::{Bounds, Modifiers, MouseButton, Pixels, Point, point, px};

use super::*;

const SIGN_IN: &str = "_io.github.furkancodes/sign_in";

fn commands() -> Said {
    Said::Update(
        json!({ "sessionUpdate": "available_commands_update", "availableCommands": [{ "name": "model", "description": "list the models" }, { "name": "mode", "description": "what runs" }, { "name": "skills", "description": "what it can do" }] }),
    )
}

fn modes() -> Said {
    let modes = Modes::of(&json!({ "currentModeId": "trust", "availableModes": [{ "id": "trust", "name": "trust" }, { "id": "ask", "name": "ask" }] }));
    Said::Modes(modes.expect("they read"))
}

/// The pane to start with, told what a conversation would be.
fn first_run(file: &Path) -> (Desk, Entity<Pane>) {
    first_run_saved(file, "[]")
}

/// The same, with `sessions` saved to go on with under its box.
fn first_run_saved(file: &Path, sessions: &str) -> (Desk, Entity<Pane>) {
    let (mut desk, draft) = desk_with_saved(file, sessions);
    desk.said(&draft, vec![Said::Started("session".to_owned()), model_setting(), modes(), commands()]);
    (desk, draft)
}

/// Four saved conversations, as many as the first run shows.
const FOUR_SAVED: &str = r#"[{"sessionId":"one","cwd":"/work","title":"a"},{"sessionId":"two","cwd":"/work","title":"b"},{"sessionId":"three","cwd":"/work","title":"c"},{"sessionId":"four","cwd":"/work","title":"d"}]"#;

/// The list `opening` opens on the first run: a cell's, or what `/` lists.
fn open_on_first_run(desk: &mut Desk, draft: &Entity<Pane>, opening: &'static str) -> &'static str {
    if opening == "/" {
        typed(desk, draft, "/");
        "listing"
    } else {
        press_on(desk, opening);
        "picker"
    }
}

/// A conversation that can set its model and mode.
fn conversation(desk: &mut Desk, file: &Path) -> Entity<Pane> {
    let pane = desk.pane(file);
    desk.said(&pane, vec![model_setting(), commands()]);
    pane
}

/// Draws the window again, and where `selector` was drawn.
fn drawn(desk: &mut Desk, selector: &'static str) -> Option<Bounds<Pixels>> {
    desk.cx.update(|window, _| window.refresh());
    desk.cx.run_until_parked();
    desk.cx.debug_bounds(selector)
}

/// A press and its release at `at`, a frame between, as your mouse does.
fn press(desk: &mut Desk, at: Point<Pixels>) {
    desk.cx.simulate_mouse_move(at, None, Modifiers::default());
    desk.cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::default());
    desk.cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
    desk.cx.run_until_parked();
}

fn press_on(desk: &mut Desk, selector: &'static str) {
    let bounds = drawn(desk, selector).unwrap_or_else(|| panic!("{selector} isn't drawn"));
    press(desk, bounds.center());
}

fn picking(desk: &mut Desk, pane: &Entity<Pane>) -> Option<Picking> {
    desk.read(pane, Pane::picking_for_test)
}

#[test]
fn the_first_runs_lists_open_under_its_box_and_inside_the_window() {
    let file = scratch("first-run-lists");
    let (mut desk, draft) = first_run(&file);
    let height = desk.cx.update(|window, _| window.viewport_size().height);
    for (cell, wanted) in [("model-cell", Picking::Model), ("mode-cell", Picking::Mode)] {
        let box_foot = drawn(&mut desk, cell).expect("the bar's drawn").bottom();
        press_on(&mut desk, cell);
        assert_eq!(picking(&mut desk, &draft), Some(wanted));
        let list = drawn(&mut desk, "picker").expect("drawn, not only known");
        assert!(list.top() >= box_foot, "under the box, over nothing it's for: {list:?}, the box ends at {box_foot:?}");
        assert!(list.bottom() <= height, "inside the window: {list:?}, {height:?} tall");
        desk.key("escape");
    }
}

#[test]
fn a_press_outside_a_list_closes_it_and_one_on_its_cell_doesnt_open_it_again() {
    let mut desk = desk();
    let file = scratch("press-out");
    let pane = conversation(&mut desk, &file);
    press_on(&mut desk, "model-cell");
    assert_eq!(picking(&mut desk, &pane), Some(Picking::Model));
    press_on(&mut desk, "model-cell");
    assert_eq!(picking(&mut desk, &pane), None, "the press that closed it is the cell's, and doesn't open it again");
    press_on(&mut desk, "model-cell");
    assert_eq!(picking(&mut desk, &pane), Some(Picking::Model), "the next one does");
    press(&mut desk, point(px(600.), px(120.)));
    assert_eq!(picking(&mut desk, &pane), None, "a press anywhere else closes it, as a menu does");
    let_go(&mut desk, &pane);
}

#[test]
fn a_press_inside_a_list_leaves_your_keys_in_it() {
    let file = scratch("press-in-first");
    let (mut desk, draft) = first_run(&file);
    press_on(&mut desk, "model-cell");
    press_on(&mut desk, "effort-1");
    desk.key("g");
    let typed = desk.cx.update(|_, cx| draft.read(cx).picker_typed_for_test(cx));
    assert_eq!((typed.as_deref(), in_box(&mut desk, &draft).as_str()), (Some("g"), ""), "on the first run, what you type next finds a model");
    let mut desk = self::desk();
    let file = scratch("press-in-conversation");
    let pane = conversation(&mut desk, &file);
    press_on(&mut desk, "model-cell");
    press_on(&mut desk, "effort-1");
    desk.key("g");
    let typed = desk.cx.update(|_, cx| pane.read(cx).picker_typed_for_test(cx));
    assert_eq!((typed.as_deref(), in_box(&mut desk, &pane).as_str()), (Some("g"), ""), "and in a conversation, not in the box behind it");
    let_go(&mut desk, &pane);
}

#[test]
fn signing_in_to_more_asks_rein_and_says_nothing_in_the_conversation() {
    for first in [true, false] {
        let file = scratch(if first { "more-first" } else { "more-conversation" });
        let (mut desk, pane) = if first {
            first_run(&file)
        } else {
            let mut desk = desk();
            let pane = conversation(&mut desk, &file);
            (desk, pane)
        };
        press_on(&mut desk, "model-cell");
        press_on(&mut desk, "sign-in-more");
        assert_eq!(picking(&mut desk, &pane), None);
        let sent = sent_in_time(&file, |line| line["method"] == SIGN_IN);
        let signing = sent.iter().find(|line| line["method"] == SIGN_IN).expect("it asked");
        assert_eq!(signing["params"]["way"], "key", "the row names those a key signs in to");
        assert!(!sent.iter().any(|line| line["method"] == "session/prompt"), "nothing went as a message: {sent:?}");
        if first {
            assert!(desk.cx.update(|_, cx| desk.panes.read(cx).draft_for_test()).is_some(), "the first run is still the first run");
        } else {
            let_go(&mut desk, &pane);
        }
    }
}

#[test]
fn the_first_runs_box_lists_commands_under_it_and_a_setting_opens_its_list() {
    let file = scratch("first-run-slash");
    let (mut desk, draft) = first_run(&file);
    typed(&mut desk, &draft, "/mod");
    let box_foot = drawn(&mut desk, "model-cell").expect("the bar's drawn").bottom();
    let listing = drawn(&mut desk, "listing").expect("what `/` lists is drawn on the first run too");
    assert!(listing.top() >= box_foot, "under the box: {listing:?}");
    desk.key("enter");
    assert_eq!(picking(&mut desk, &draft), Some(Picking::Model), "`/model`, listed first, opens the list its cell does");
    assert!(desk.cx.update(|_, cx| desk.panes.read(cx).draft_for_test()).is_some(), "rather than starting a conversation that says /model");
    assert!(!sent_to(&file).iter().any(|line| line["method"] == "session/prompt"));
}

#[test]
fn command_k_on_the_first_run_lists_what_it_can_run() {
    let file = scratch("first-run-palette");
    let (mut desk, _draft) = first_run(&file);
    desk.key("cmd-k");
    let runs = desk.cx.update(|_, cx| desk.panes.read(cx).palette_runs_for_test(cx));
    assert!(runs.contains(&"/model".to_owned()), "the pane to start with is the one you're at: {runs:?}");
}

#[test]
fn the_first_screen_has_no_list_beside_it_and_its_count_opens_the_saved_ones() {
    let file = scratch("bare-first-run");
    let (mut desk, _draft) = desk_with_saved(&file, r#"[{"sessionId":"one","cwd":"/work","title":"fix the flaky test"}]"#);
    assert!(drawn(&mut desk, "sidebar").is_none(), "nothing's on the desk, so nothing's listed beside it");
    press_on(&mut desk, "all-saved");
    assert!(drawn(&mut desk, "sidebar").is_some(), "the count opens every saved conversation beside the box");
    press_on(&mut desk, "back");
    assert!(drawn(&mut desk, "sidebar").is_none(), "and Back is the first screen again");
}

#[test]
fn the_first_runs_lists_are_over_the_saved_conversations_and_take_the_press() {
    for (n, opening) in ["model-cell", "mode-cell", "/"].into_iter().enumerate() {
        let file = scratch(&format!("over-saved-{n}"));
        let (mut desk, draft) = first_run_saved(&file, FOUR_SAVED);
        let card = open_on_first_run(&mut desk, &draft, opening);
        let list = drawn(&mut desk, card).unwrap_or_else(|| panic!("{opening} opens its list"));
        let row = drawn(&mut desk, "recent-0").expect("a saved conversation is under the box");
        assert!(list.contains(&row.center()), "{opening}: the list lies over a saved conversation here, {list:?} over {row:?}");
        press(&mut desk, row.center());
        let opened: Vec<String> = desk.cx.update(|_, cx| desk.panes.read(cx).panes().iter().filter_map(|pane| pane.read(cx).session().map(str::to_owned)).collect());
        assert!(!opened.contains(&"one".to_owned()), "{opening}: the press was the list's, not the saved conversation's drawn under it: {opened:?}");
    }
}

#[test]
fn the_first_runs_lists_stay_inside_a_short_window() {
    for (n, opening) in ["model-cell", "mode-cell", "/"].into_iter().enumerate() {
        let file = scratch(&format!("short-{n}"));
        let (mut desk, draft) = first_run_saved(&file, FOUR_SAVED);
        desk.cx.simulate_resize(gpui_kit::size(px(900.), px(560.)));
        let card = open_on_first_run(&mut desk, &draft, opening);
        let list = drawn(&mut desk, card).unwrap_or_else(|| panic!("{opening} opens its list"));
        let height = desk.cx.update(|window, _| window.viewport_size().height);
        assert!(list.bottom() <= height, "{opening}: its list ends inside the window, {list:?}, {height:?} tall");
    }
}

#[test]
fn every_effort_level_has_room_around_its_word_inside_the_card() {
    const LEVELS: [&str; 8] = ["default", "off", "minimal", "low", "medium", "high", "xhigh", "max"];
    const PILLS: [&str; 8] = ["effort-0", "effort-1", "effort-2", "effort-3", "effort-4", "effort-5", "effort-6", "effort-7"];
    let mut desk = desk();
    let file = scratch("effort-room");
    let pane = desk.pane(&file);
    let levels: Vec<Value> = LEVELS.iter().map(|level| json!({ "value": level, "name": level })).collect();
    let settings = Setting::listed(&json!([
        { "id": "model", "currentValue": "openai/gpt-5", "options": [{ "group": "openai", "name": "OpenAI", "options": [{ "value": "openai/gpt-5", "name": "gpt-5" }] }] },
        { "id": "effort", "currentValue": "medium", "options": levels },
    ]));
    desk.said(&pane, vec![Said::Settings(settings), commands()]);
    press_on(&mut desk, "model-cell");
    let card = drawn(&mut desk, "picker").expect("the model's list is open");
    for (word, selector) in LEVELS.into_iter().zip(PILLS) {
        let pill = drawn(&mut desk, selector).unwrap_or_else(|| panic!("{word} is drawn"));
        let text = desk.cx.update(|window, _| {
            let mut style = window.text_style();
            style.font_weight = gpui_kit::FontWeight::MEDIUM;
            window.text_system().shape_line(word.into(), px(12.), &[style.to_run(word.len())], None).width
        });
        let room = px(2. * crate::pane::LEVEL_ROOM_FOR_TEST);
        assert!(pill.size.width >= text + room, "{word}: its pill has room around the word, {pill:?} for a word {text:?} wide");
        assert!(pill.left() >= card.left() && pill.right() <= card.right(), "{word}: inside the card, {pill:?} in {card:?}");
    }
    let_go(&mut desk, &pane);
}

/// A question as "Sign in to more" asks it: a long list to choose from.
fn add_a_key_for() -> Said {
    let items = (0..13).map(|n| rein_transcript::Item { name: format!("Provider {n}"), about: "no key yet".to_owned() }).collect();
    let menu = rein_transcript::Menu { title: "add a key for".to_owned(), items, current: None, at: None };
    Said::Asking { id: json!(9), asking: Asking::Choose(menu) }
}

#[test]
fn a_question_on_the_first_run_is_under_the_box_in_the_window_and_no_list_opens_over_it() {
    let file = scratch("first-run-question");
    let (mut desk, draft) = first_run_saved(&file, FOUR_SAVED);
    desk.cx.simulate_resize(gpui_kit::size(px(900.), px(640.)));
    press_on(&mut desk, "model-cell");
    assert_eq!(picking(&mut desk, &draft), Some(Picking::Model));
    desk.said(&draft, vec![add_a_key_for()]);
    assert_eq!(picking(&mut desk, &draft), None, "the list from the bar gives way to the question");
    let bar = drawn(&mut desk, "model-cell").expect("the box's bar is drawn");
    let asking = drawn(&mut desk, "asking").expect("the question is drawn");
    let height = desk.cx.update(|window, _| window.viewport_size().height);
    assert!(bar.top() >= px(0.) && asking.top() >= bar.bottom(), "under the box, with the box in view: {asking:?} under {bar:?}");
    // The sheet's foot is 26 high: past it, the first run scrolls.
    assert!(asking.bottom() <= height - px(26.), "inside the window, and nothing scrolls: {asking:?}, {height:?} tall");
    assert!(drawn(&mut desk, "recent-0").is_none(), "it takes the saved conversations' place");
    press_on(&mut desk, "model-cell");
    assert_eq!(picking(&mut desk, &draft), None, "no list opens while the question waits for its answer");
}
