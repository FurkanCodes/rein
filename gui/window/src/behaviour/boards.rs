//! The sheets, panels and lists, on what rein says.

use super::*;

#[test]
fn a_turn_is_undone_from_a_sheet_that_says_what_it_will_do_first() {
    let mut desk = desk();
    let file = scratch("undo");
    let pane = desk.pane(&file);
    turn_line(&mut desk, &pane, "2 steps · changed a.rs · 4s", Some(7));
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_for_test("/undo", cx)));
    let id = wait_for_ask(&file, UNDO_TURNS);
    told(&mut desk, &pane, id, json!({ "turns": [{ "turn": 8, "said": "later", "at": 2, "changed": 1 }, { "turn": 7, "said": "the one", "at": 1, "changed": 1 }] }));
    assert_eq!(desk.read(&pane, Pane::undo_picked_for_test), Some(8), "the latest, unless it's asked for one");
    let plan = wait_for_ask(&file, UNDO_PLAN);
    assert_eq!(desk.read(&pane, Pane::undo_plan_for_test), Some(None), "nothing's offered until rein says what it would do");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.undo_now_for_test()));
    told(&mut desk, &pane, plan, json!({ "plan": { "putBack": ["a.rs"], "takeAway": [], "left": [], "lost": [], "turns": 1 } }));
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.undo_now_for_test()));
    let id = wait_for_ask(&file, "_io.github.furkancodes/undo");
    assert_eq!(asks_of(&file, "_io.github.furkancodes/undo", 1), 1, "the press before rein said what it does did nothing");
    assert_eq!(sent_to(&file).into_iter().find(|line| line["id"] == id).expect("sent")["params"]["turn"], 8);
    answers(&mut desk, &pane, id, CameTo::Told(json!({ "plan": {} })));
    assert_eq!(desk.read(&pane, Pane::undo_picked_for_test), None, "the sheet goes when it's done");
}

#[test]
fn nothing_to_undo_is_said_and_no_sheet_comes_up() {
    let mut desk = desk();
    let file = scratch("undo-none");
    let pane = desk.pane(&file);
    let rows = desk.read(&pane, Pane::rows_for_test);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_for_test("/undo", cx)));
    let id = wait_for_ask(&file, UNDO_TURNS);
    told(&mut desk, &pane, id, json!({ "turns": [] }));
    assert_eq!(desk.read(&pane, Pane::undo_picked_for_test), None);
    assert_eq!(desk.read(&pane, Pane::rows_for_test), rows + 1);
}

#[test]
fn a_panel_shows_whats_taking_room_and_closes_as_it_opened() {
    let mut desk = desk();
    let file = scratch("panel");
    let pane = desk.pane(&file);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Context, cx)));
    desk.cx.run_until_parked();
    assert_eq!(desk.read(&pane, Pane::panel_tab_for_test), Some(Tab::Context));
    let id = wait_for_ask(&file, "_io.github.furkancodes/room");
    told(&mut desk, &pane, id, json!({ "used": 42_000, "limit": 150_000, "parts": [{ "name": "Tools", "about": "17", "tokens": 6200 }], "biggest": [] }));
    assert_eq!(desk.read(&pane, Pane::panel_room_for_test).map(|room| room.used), Some(42_000));
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Context, cx)));
    assert_eq!(desk.read(&pane, Pane::panel_tab_for_test), None);
}

#[test]
fn the_tasks_panel_asks_after_the_background_and_stops_what_you_press() {
    let mut desk = desk();
    let file = scratch("tasks");
    let pane = desk.pane(&file);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_for_test("/tasks", cx)));
    desk.cx.run_until_parked();
    assert_eq!(desk.read(&pane, Pane::panel_tab_for_test), Some(Tab::Tasks));
    let id = wait_for_ask(&file, "_io.github.furkancodes/tasks");
    told(&mut desk, &pane, id, json!({ "tasks": [{ "id": "b1", "command": "pnpm dev", "seconds": 5 }] }));
    assert_eq!(desk.read(&pane, Pane::panel_tasks_for_test), 1);
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.stop_task_for_test("b1")));
    let id = wait_for_ask(&file, "_io.github.furkancodes/stop_task");
    assert_eq!(sent_to(&file).into_iter().find(|line| line["id"] == id).expect("sent")["params"]["id"], "b1");
}

#[test]
fn what_runs_in_the_background_is_counted_and_follows_the_tasks_asked_after() {
    let mut desk = desk();
    let file = scratch("tasks-count");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![Said::State { branch: None, meters: Vec::new(), background: 2 }]);
    assert_eq!(desk.read(&pane, Pane::background_for_test), 2);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Tasks, cx)));
    desk.cx.run_until_parked();
    let id = wait_for_ask(&file, "_io.github.furkancodes/tasks");
    told(&mut desk, &pane, id, json!({ "tasks": [{ "id": "b1", "command": "pnpm dev", "seconds": 5 }, { "id": "b2", "command": "npm test", "seconds": 9, "ended": "stopped" }] }));
    assert_eq!(desk.read(&pane, Pane::background_for_test), 1, "one you stopped here is news nobody sends");
}

const MODELS: &str = "_io.github.furkancodes/models";
const SET: &str = "session/set_config_option";

/// What the pane set on its agent, as (option, value).
fn options_set(file: &Path) -> Vec<(String, String)> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    sent_to(file).into_iter().filter(|line| line["method"] == SET).map(|line| (text(&line["params"]["configId"]), text(&line["params"]["value"]))).collect()
}

fn set(option: &str, value: &str) -> (String, String) {
    (option.to_owned(), value.to_owned())
}

/// What the pane sent as prompts, in order.
fn prompted(file: &Path) -> Vec<String> {
    sent_to(file).into_iter().filter(|line| line["method"] == "session/prompt").filter_map(|line| line["params"]["prompt"][0]["text"].as_str().map(str::to_owned)).collect()
}

fn mode_setting() -> Said {
    Said::Settings(Setting::listed(&json!([{ "id": "mode", "currentValue": "trust", "options": [{ "value": "trust", "name": "⚠ trust" }, { "value": "ask", "name": "ask" }] }])))
}

/// Narrows the model list to Groq's and takes what's left.
fn pick_groq(desk: &mut Desk, pane: &Entity<Pane>) {
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.open_picker_for_test(Picking::Model, window, cx)));
    desk.cx.run_until_parked();
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.type_in_picker_for_test("groq", window, cx)));
    desk.cx.run_until_parked();
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.take_pick_for_test(window, cx)));
    desk.cx.run_until_parked();
}

fn run(desk: &mut Desk, pane: &Entity<Pane>, line: &'static str) {
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_for_test(line, cx)));
    desk.cx.run_until_parked();
}

#[test]
fn the_model_cell_opens_a_list_to_narrow_and_a_pick_writes_nothing() {
    let mut desk = desk();
    let file = scratch("picker");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    pick_groq(&mut desk, &pane);
    wait_for_ask(&file, MODELS);
    assert_eq!(desk.read(&pane, Pane::picking_for_test), None);
    assert_eq!(desk.read(&pane, Pane::rows_for_test), 0, "you wrote nothing, and the cell shows what it comes to");
    assert!(!desk.read(&pane, Pane::is_running), "changing a setting isn't a turn");
    let_go(&mut desk, &pane);
    assert_eq!(options_set(&file), [set("model", "groq/llama")]);
    assert_eq!(prompted(&file), [""; 0]);
}

#[test]
fn while_a_turn_runs_a_pick_waits_for_its_end_and_writes_nothing() {
    let mut desk = desk();
    let file = scratch("picker-busy");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    desk.send(&pane, "go");
    pick_groq(&mut desk, &pane);
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["/model groq/llama"], "the list opens at once, and rein takes it after");
    let turn = wait_for_ask(&file, "session/prompt");
    answers(&mut desk, &pane, turn, CameTo::TurnOver);
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), [""; 0]);
    let_go(&mut desk, &pane);
    assert_eq!(options_set(&file), [set("model", "groq/llama")]);
    assert_eq!(prompted(&file), ["go"]);
    assert_eq!(asked_for(&file, MODELS), [0_u64; 0], "rein would turn the lookup down, and say so in the conversation");
}

#[test]
fn the_mode_list_says_what_each_lets_through_and_a_pick_writes_nothing() {
    let mut desk = desk();
    let file = scratch("mode-picker");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![mode_setting()]);
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.open_picker_for_test(Picking::Mode, window, cx)));
    desk.cx.run_until_parked();
    desk.key("down");
    desk.cx.run_until_parked();
    desk.key("enter");
    assert_eq!(desk.read(&pane, Pane::rows_for_test), 0);
    let_go(&mut desk, &pane);
    assert_eq!(options_set(&file), [set("mode", "ask")]);
    assert_eq!(prompted(&file), [""; 0]);
}

#[test]
fn an_effort_pressed_writes_nothing_and_waits_out_a_turn() {
    let mut desk = desk();
    let file = scratch("effort");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.set_effort_for_test("high", cx)));
    desk.cx.run_until_parked();
    assert_eq!(desk.read(&pane, Pane::rows_for_test), 0);
    let first = wait_for_ask(&file, SET);
    answers(&mut desk, &pane, first, CameTo::Done);
    desk.send(&pane, "go");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.set_effort_for_test("low", cx)));
    desk.cx.run_until_parked();
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["/effort low"], "rein would turn it down");
    let_go(&mut desk, &pane);
    assert_eq!(options_set(&file), [set("effort", "high")]);
}

#[test]
fn a_setting_that_fails_says_why_and_nothing_else_changes() {
    let mut desk = desk();
    let file = scratch("setting-fails");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    run(&mut desk, &pane, "/model groq/llama");
    let id = wait_for_ask(&file, SET);
    answers(&mut desk, &pane, id, CameTo::Refused("Groq isn't signed in".to_owned()));
    assert!(desk.read(&pane, Pane::conversation_for_test).contains("Groq isn't signed in"));
    assert!(!desk.read(&pane, Pane::is_running));
    assert_eq!(desk.read(&pane, Pane::standing), Standing::Idle, "a turn didn't fail: nothing's the matter with the conversation");
}

#[test]
fn a_setting_typed_goes_as_a_pick_does_and_the_word_alone_opens_its_list() {
    let mut desk = desk();
    let file = scratch("typed-setting");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    desk.box_says(&pane, "/model groq/llama");
    assert_eq!(desk.read(&pane, Pane::rows_for_test), 0);
    let id = wait_for_ask(&file, SET);
    answers(&mut desk, &pane, id, CameTo::Done);
    for line in ["/model", "/effort"] {
        desk.box_says(&pane, line);
        assert_eq!(desk.read(&pane, Pane::picking_for_test), Some(Picking::Model), "{line}");
        desk.key("escape");
    }
    desk.said(&pane, vec![mode_setting()]);
    for line in ["/mode", "/sandbox"] {
        desk.box_says(&pane, line);
        assert_eq!(desk.read(&pane, Pane::picking_for_test), Some(Picking::Mode), "{line}");
        desk.key("escape");
    }
    let_go(&mut desk, &pane);
    assert_eq!(prompted(&file), [""; 0]);
}

#[test]
fn a_setting_its_agent_doesnt_offer_goes_as_the_line_it_is() {
    let mut desk = desk();
    let file = scratch("no-setting");
    let pane = desk.pane(&file);
    desk.box_says(&pane, "/model groq/llama");
    let_go(&mut desk, &pane);
    assert_eq!(options_set(&file), []);
    assert_eq!(prompted(&file), ["/model groq/llama"]);
}

#[test]
fn what_you_send_while_a_setting_goes_through_waits_for_it() {
    let mut desk = desk();
    let file = scratch("setting-race");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    run(&mut desk, &pane, "/effort high");
    run(&mut desk, &pane, "go");
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["go"], "rein says it's busy until then, and a turn that never started would run for good");
    assert!(!desk.read(&pane, Pane::is_running));
    let id = wait_for_ask(&file, SET);
    answers(&mut desk, &pane, id, CameTo::Done);
    assert!(desk.read(&pane, Pane::is_running), "it goes as the setting's done");
    let_go(&mut desk, &pane);
    assert_eq!(prompted(&file), ["go"]);
}

#[test]
fn what_waited_for_a_turn_goes_one_at_a_time_past_a_setting() {
    let mut desk = desk();
    let file = scratch("setting-queue");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    desk.send(&pane, "go");
    run(&mut desk, &pane, "/effort high");
    run(&mut desk, &pane, "/model groq/llama");
    run(&mut desk, &pane, "/compact");
    let turn = wait_for_ask(&file, "session/prompt");
    answers(&mut desk, &pane, turn, CameTo::TurnOver);
    let rest = ["/model groq/llama", "/compact"];
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), rest, "rein is busy with the first, and may be asking about it");
    desk.said(&pane, vec![model_setting()]);
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), rest, "and what's said meanwhile leaves them in the order you sent them");
    let first = wait_for_ask(&file, SET);
    answers(&mut desk, &pane, first, CameTo::Done);
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["/compact"]);
    let_go(&mut desk, &pane);
    assert_eq!(options_set(&file), [set("effort", "high"), set("model", "groq/llama")]);
}

#[test]
fn a_setting_sent_with_an_image_goes_as_the_message_it_is() {
    let mut desk = desk();
    let file = scratch("setting-image");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![model_setting()]);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_with_image_for_test("/model groq/llama", cx)));
    desk.cx.run_until_parked();
    let_go(&mut desk, &pane);
    assert_eq!(options_set(&file), []);
    let sent = sent_to(&file).into_iter().find(|line| line["method"] == "session/prompt").expect("it went");
    assert!(sent["params"]["prompt"].as_array().is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "image")), "the image isn't dropped: {sent}");
}

#[test]
fn the_palette_lists_agents_and_commands_and_runs_what_you_pick() {
    let mut desk = desk();
    let (first, second) = (scratch("palette-1"), scratch("palette-2"));
    let (a, b) = (desk.pane(&first), desk.pane(&second));
    let listed = json!({ "sessionUpdate": "available_commands_update", "availableCommands": [{ "name": "compact", "description": "make room" }, { "name": "new-one", "description": "something rein grew" }] });
    desk.said(&b, vec![Said::Update(listed)]);
    desk.key("cmd-k");
    desk.cx.run_until_parked();
    let panes = desk.panes.clone();
    assert!(desk.cx.update(|_, cx| panes.read(cx).palette_open_for_test()));
    let names = desk.cx.update(|_, cx| panes.read(cx).palette_names_for_test(cx));
    assert!(names.contains(&"Make room".to_owned()) && names.contains(&"something rein grew".to_owned()), "{names:?}");
    desk.cx.update(|window, cx| panes.update(cx, |panes, cx| panes.type_in_palette_for_test("make room", window, cx)));
    desk.cx.run_until_parked();
    desk.cx.update(|window, cx| panes.update(cx, |panes, cx| panes.take_palette_for_test(window, cx)));
    assert!(!desk.cx.update(|_, cx| panes.read(cx).palette_open_for_test()), "it goes as it runs");
    let_go(&mut desk, &b);
    let ran = sent_to(&second).into_iter().find(|line| line["method"] == "session/prompt").expect("it ran");
    assert_eq!(ran["params"]["prompt"][0]["text"], "/compact", "in the pane you're at");
    drop(a);
}

#[test]
fn the_palette_goes_to_an_agent() {
    let mut desk = desk();
    let (first, second) = (scratch("go-1"), scratch("go-2"));
    let (a, _b) = (desk.pane(&first), desk.pane(&second));
    desk.send(&a, "the one to find");
    desk.key("cmd-k");
    desk.cx.run_until_parked();
    let panes = desk.panes.clone();
    desk.cx.update(|window, cx| panes.update(cx, |panes, cx| panes.type_in_palette_for_test("find", window, cx)));
    desk.cx.run_until_parked();
    assert_eq!(desk.cx.update(|_, cx| panes.read(cx).palette_names_for_test(cx)), ["the one to find"]);
    desk.cx.update(|window, cx| panes.update(cx, |panes, cx| panes.take_palette_for_test(window, cx)));
    assert!(desk.read(&a, Pane::is_looked_at));
}

#[test]
fn esc_puts_the_palette_away_and_the_keys_go_back_to_the_box() {
    let mut desk = desk();
    let file = scratch("palette-esc");
    let pane = desk.pane(&file);
    typed(&mut desk, &pane, "words");
    desk.key("cmd-k");
    desk.cx.run_until_parked();
    desk.key("escape");
    desk.cx.run_until_parked();
    let panes = desk.panes.clone();
    assert!(!desk.cx.update(|_, cx| panes.read(cx).palette_open_for_test()));
    desk.key("!");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "words!");
}

#[test]
fn when_a_sheet_closes_the_keys_go_back_to_the_box() {
    let mut desk = desk();
    let file = scratch("sheet-keys");
    let pane = desk.pane(&file);
    typed(&mut desk, &pane, "");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_for_test("/undo", cx)));
    let id = wait_for_ask(&file, UNDO_TURNS);
    told(&mut desk, &pane, id, json!({ "turns": [{ "turn": 8, "said": "later", "at": 2, "changed": 1 }] }));
    desk.cx.run_until_parked();
    desk.key("escape");
    desk.cx.run_until_parked();
    assert_eq!(desk.read(&pane, Pane::undo_picked_for_test), None);
    desk.key("a");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "a", "there's somewhere for what you type to go");
}

#[test]
fn when_the_panel_closes_with_the_keys_in_it_they_go_back_to_the_box() {
    let mut desk = desk();
    let file = scratch("panel-keys");
    let pane = desk.pane(&file);
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Context, cx)));
    desk.cx.run_until_parked();
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.focus_panel_for_test(window, cx)));
    desk.cx.run_until_parked();
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.open_panel_for_test(Tab::Context, cx)));
    desk.cx.run_until_parked();
    desk.key("a");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "a");
}
