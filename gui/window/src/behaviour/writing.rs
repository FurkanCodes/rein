//! The box, under real keys.

use super::*;

#[test]
fn the_list_of_commands_follows_what_you_type_and_enter_runs_the_one_picked() {
    let mut desk = desk();
    let file = scratch("list");
    let pane = desk.pane(&file);
    // Of those the window has no home of its own for: `/status` opens the
    // Context panel, as it does sent.
    let listed = json!({ "sessionUpdate": "available_commands_update", "availableCommands": [{ "name": "compact", "description": "make room" }, { "name": "skills", "description": "what it can do" }] });
    desk.said(&pane, vec![Said::Update(listed)]);
    typed(&mut desk, &pane, "/ski");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("skills".to_owned()));
    desk.key("enter");
    let in_box = desk.cx.update(|_, cx| pane.read(cx).input.read(cx).value().to_string());
    assert_eq!(in_box, "");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.finish_agent_for_test()));
    let ran = sent_to(&file).into_iter().find(|line| line["method"] == "session/prompt").expect("it ran");
    assert_eq!(ran["params"]["prompt"][0]["text"], "/skills");
}

#[test]
fn the_arrows_move_through_the_list_of_commands() {
    let mut desk = desk();
    let file = scratch("arrows");
    let pane = desk.pane(&file);
    let listed = json!({ "sessionUpdate": "available_commands_update", "availableCommands": [{ "name": "compact", "description": "make room" }, { "name": "status", "description": "how it stands" }] });
    desk.said(&pane, vec![Said::Update(listed)]);
    typed(&mut desk, &pane, "/");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("compact".to_owned()));
    desk.key("down");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("status".to_owned()));
    desk.key("up");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("compact".to_owned()));
}

#[test]
fn a_command_waits_for_the_end_of_the_turn_and_then_runs() {
    let mut desk = desk();
    let file = scratch("waits");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    // `/status` alone opens the Context panel, which needn't wait.
    desk.box_says(&pane, "/skills");
    assert_eq!(in_box(&mut desk, &pane), "", "it's on its way");
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["/skills"]);
    desk.said(&pane, vec![Said::TurnOver]);
    assert!(desk.read(&pane, Pane::waiting_for_test).is_empty());
    let_go(&mut desk, &pane);
    let prompts: Vec<Value> = sent_to(&file).into_iter().filter(|line| line["method"] == "session/prompt").map(|line| line["params"]["prompt"][0]["text"].clone()).collect();
    assert_eq!(prompts, [json!("go"), json!("/skills")]);
}

#[test]
fn tab_sends_it_after_the_turn_and_enter_steers() {
    let mut desk = desk();
    let file = scratch("tab-later");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "and then this");
    desk.key("tab");
    assert_eq!(in_box(&mut desk, &pane), "");
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["and then this"]);
    typed(&mut desk, &pane, "right now");
    desk.key("enter");
    assert_eq!(desk.read(&pane, Pane::steering_for_test), ["right now"], "it shows until it reaches the turn");
    let reached = json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "right now" } });
    desk.said(&pane, vec![Said::Update(reached)]);
    assert!(desk.read(&pane, Pane::steering_for_test).is_empty());
    assert_eq!(asks_of(&file, "session/prompt", 1), 1, "what waits hasn't gone");
    desk.said(&pane, vec![Said::TurnOver]);
    let_go(&mut desk, &pane);
    let prompts: Vec<Value> = sent_to(&file).into_iter().filter(|line| line["method"] == "session/prompt").map(|line| line["params"]["prompt"][0]["text"].clone()).collect();
    assert_eq!(prompts, [json!("go"), json!("and then this")]);
}

#[test]
fn stopping_brings_what_was_waiting_back_to_the_box() {
    let mut desk = desk();
    let file = scratch("stop-back");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "next");
    desk.key("tab");
    typed(&mut desk, &pane, "");
    desk.key("escape");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "next");
    assert!(desk.read(&pane, Pane::waiting_for_test).is_empty());
    desk.said(&pane, vec![Said::TurnOver]);
    assert_eq!(asks_of(&file, "session/prompt", 1), 1, "and nothing went by itself");
}

#[test]
fn a_turn_that_fails_leaves_what_was_waiting_to_you() {
    let mut desk = desk();
    let file = scratch("failed-back");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "next");
    desk.key("tab");
    desk.said(&pane, vec![Said::Failed("the key was refused".to_owned())]);
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "next");
    let_go(&mut desk, &pane);
    assert_eq!(asks_of(&file, "session/prompt", 1), 1);
}

#[test]
fn up_brings_back_what_you_sent_and_down_what_you_were_writing() {
    let mut desk = desk();
    let file = scratch("recall");
    let pane = with_sent(&mut desk, &file, &["first", "second"]);
    typed(&mut desk, &pane, "draft");
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "second");
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "first");
    desk.key("down");
    desk.cx.run_until_parked();
    desk.key("down");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "draft");
}

#[test]
fn up_moves_within_what_you_wrote_before_it_goes_back() {
    let mut desk = desk();
    let file = scratch("recall-lines");
    let pane = with_sent(&mut desk, &file, &["older"]);
    typed(&mut desk, &pane, "one\ntwo");
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "one\ntwo", "it's the box's own, on the second line");
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "older");
}

#[test]
fn a_message_that_comes_back_has_its_caret_at_its_end() {
    let mut desk = desk();
    let file = scratch("recall-caret");
    let pane = with_sent(&mut desk, &file, &["older", "one\ntwo"]);
    typed(&mut desk, &pane, "");
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "one\ntwo");
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "one\ntwo", "the caret came back on its last line, so this moves up it");
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "older");
}

#[test]
fn a_turn_ending_takes_what_never_reached_it_off_the_way_to_it() {
    let mut desk = desk();
    let file = scratch("steering-ends");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "too late");
    desk.key("enter");
    assert_eq!(desk.read(&pane, Pane::steering_for_test), ["too late"]);
    desk.said(&pane, vec![Said::TurnOver]);
    assert!(desk.read(&pane, Pane::steering_for_test).is_empty(), "it won't reach anything now");
}

#[test]
fn at_asks_for_the_files_again_after_a_turn_that_may_have_changed_them() {
    let mut desk = desk();
    let file = scratch("at-again");
    let pane = with_paths(&mut desk, &file, "@sr");
    let asked = asked_for(&file, PATHS).len();
    desk.key("backspace");
    assert_eq!(asked_for(&file, PATHS).len(), asked, "while nothing's happened it has them");
    desk.said(&pane, vec![Said::TurnOver]);
    desk.key("r");
    desk.cx.run_until_parked();
    assert_eq!(asks_of(&file, PATHS, asked + 1), asked + 1, "a turn may have made more");
}

#[test]
fn what_you_send_comes_back_with_up_but_a_commands_line_only_if_rein_says() {
    let mut desk = desk();
    let file = scratch("recall-keep");
    let pane = with_sent(&mut desk, &file, &[]);
    desk.box_says(&pane, "/login groq a-secret-key");
    desk.said(&pane, vec![Said::TurnOver]);
    desk.key("up");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "", "it may hold a key, and only rein can tell");
}

#[test]
fn control_r_finds_what_you_sent() {
    let mut desk = desk();
    let file = scratch("find");
    let pane = with_sent(&mut desk, &file, &["fix the tests", "write the docs\nall of them"]);
    typed(&mut desk, &pane, "");
    desk.key("ctrl-r");
    desk.cx.run_until_parked();
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.type_in_asking_for_test("docs", window, cx)));
    desk.cx.run_until_parked();
    desk.key("enter");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "write the docs\nall of them", "whole, as it was sent");
    assert!(desk.read(&pane, |pane| pane.asking_for_test().is_none()));
}

#[test]
fn at_lists_the_projects_files_and_tab_puts_one_in() {
    let mut desk = desk();
    let file = scratch("at-tab");
    let pane = with_paths(&mut desk, &file, "read @mai");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("src/main.rs".to_owned()));
    desk.key("tab");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "read @src/main.rs ", "with a space, to go on writing");
}

#[test]
fn a_folder_is_left_open_to_go_on_into_it() {
    let mut desk = desk();
    let file = scratch("at-folder");
    let pane = with_paths(&mut desk, &file, "@sr");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("src/".to_owned()));
    desk.key("enter");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "@src/");
}

#[test]
fn enter_sends_a_path_typed_out_in_full() {
    let mut desk = desk();
    let file = scratch("at-full");
    let pane = with_paths(&mut desk, &file, "@README.md");
    desk.key("enter");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "");
    let_go(&mut desk, &pane);
    let ran = sent_to(&file).into_iter().find(|line| line["method"] == "session/prompt").expect("it went");
    assert_eq!(ran["params"]["prompt"][0]["text"], "@README.md");
}

#[test]
fn esc_puts_the_list_away_for_that_word() {
    let mut desk = desk();
    let file = scratch("at-esc");
    let pane = with_paths(&mut desk, &file, "@sr");
    desk.key("escape");
    desk.cx.run_until_parked();
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), None);
    assert_eq!(in_box(&mut desk, &pane), "@sr", "and what you wrote stays");
    desk.key("a");
    desk.cx.run_until_parked();
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("src/main.rs".to_owned()), "another word lists again");
}

#[test]
fn a_skill_is_listed_for_a_slash_word_later_in_a_message() {
    let mut desk = desk();
    let file = scratch("later-skill");
    let pane = desk.pane(&file);
    let listed = json!({ "sessionUpdate": "available_commands_update", "availableCommands": [{ "name": "status", "description": "how it stands" }, { "name": "stats", "description": "numbers", "_meta": { "io.github.furkancodes/skill": true } }] });
    desk.said(&pane, vec![Said::Update(listed)]);
    typed(&mut desk, &pane, "use /sta");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).listed_for_test(cx)), Some("stats".to_owned()), "a command runs alone, and a skill loads with the message");
    desk.key("tab");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "use /stats ");
}

#[test]
fn an_image_goes_with_its_message_and_one_taken_out_doesnt() {
    let mut desk = desk();
    let file = scratch("image");
    let pane = desk.pane(&file);
    typed(&mut desk, &pane, "");
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.attach_for_test(b"one", window, cx);
            pane.attach_for_test(b"two", window, cx);
        });
    });
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "[Image #1] [Image #2] ");
    typed(&mut desk, &pane, "what's [Image #2]?");
    desk.key("enter");
    let_go(&mut desk, &pane);
    let ran = sent_to(&file).into_iter().find(|line| line["method"] == "session/prompt").expect("it went");
    let prompt = ran["params"]["prompt"].as_array().expect("blocks");
    assert_eq!(prompt.len(), 2, "{prompt:?}");
    assert_eq!(prompt[1]["data"], "dHdv", "the one still named");
    assert_eq!(prompt[1]["_meta"]["io.github.furkancodes/label"], "[Image #2]");
    assert_eq!(prompt[1]["mimeType"], "image/png");
}

#[test]
fn a_message_with_an_image_waits_for_the_turn_as_a_steer_cant_carry_one() {
    let mut desk = desk();
    let file = scratch("image-later");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "");
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.attach_for_test(b"one", window, cx)));
    desk.cx.run_until_parked();
    desk.key("enter");
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["[Image #1] "]);
    desk.said(&pane, vec![Said::TurnOver]);
    let_go(&mut desk, &pane);
    let prompts: Vec<Value> = sent_to(&file).into_iter().filter(|line| line["method"] == "session/prompt").collect();
    assert_eq!(prompts[1]["params"]["prompt"][1]["data"], "b25l", "and it goes with it");
    assert!(!sent_to(&file).iter().any(|line| line["method"] == "_io.github.furkancodes/steer"));
}

#[test]
fn control_o_opens_every_steps_output_and_closes_it() {
    let mut desk = desk();
    let file = scratch("all-output");
    let pane = desk.pane(&file);
    typed(&mut desk, &pane, "");
    desk.key("ctrl-o");
    assert!(desk.read(&pane, Pane::all_open_for_test));
    desk.key("ctrl-o");
    assert!(!desk.read(&pane, Pane::all_open_for_test));
}

#[test]
fn control_b_sends_the_running_command_to_the_background_with_nothing_typed() {
    let mut desk = desk();
    let file = scratch("ctrl-b");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "words");
    desk.key("ctrl-b");
    typed(&mut desk, &pane, "");
    desk.key("ctrl-b");
    let_go(&mut desk, &pane);
    assert_eq!(asks_of(&file, "_io.github.furkancodes/background", 1), 1, "with words there it's the caret, a letter left");
}

#[test]
fn tab_puts_the_command_in_the_box_to_add_to_it() {
    let mut desk = desk();
    let file = scratch("tab");
    let pane = desk.pane(&file);
    let listed = json!({ "sessionUpdate": "available_commands_update", "availableCommands": [{ "name": "compact", "description": "make room" }] });
    desk.said(&pane, vec![Said::Update(listed)]);
    typed(&mut desk, &pane, "/com");
    desk.key("tab");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).input.read(cx).value().to_string()), "/compact ");
}

#[test]
fn a_commands_line_that_cant_run_is_named_and_nothing_more() {
    let mut desk = desk();
    let file = scratch("busy-key");
    let pane = desk.pane(&file);
    desk.send(&pane, "/login groq sk-secret");
    let id = desk.read(&pane, Pane::last_request_for_test);
    answers(&mut desk, &pane, id, CameTo::Busy("a turn is running".to_owned()));
    let said = desk.read(&pane, Pane::conversation_for_test);
    assert!(said.contains("/login didn't run") && !said.contains("sk-secret"), "{said}");
}

#[test]
fn a_message_a_turn_got_ahead_of_goes_into_it_and_shows_once() {
    let mut desk = desk();
    let file = scratch("busy-steer");
    let pane = desk.pane(&file);
    desk.send(&pane, "go on");
    let id = desk.read(&pane, Pane::last_request_for_test);
    answers(&mut desk, &pane, id, CameTo::Busy("a turn is running".to_owned()));
    let reached = json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "go on" } });
    desk.said(&pane, vec![Said::Update(reached.clone())]);
    assert_eq!(desk.read(&pane, Pane::conversation_for_test).matches("go on").count(), 1);
    desk.said(&pane, vec![Said::Update(reached)]);
    assert_eq!(desk.read(&pane, Pane::conversation_for_test).matches("go on").count(), 2, "only that once: you may say it again");
    assert_eq!(asks_of(&file, STEER, 1), 1);
}

#[test]
fn a_message_with_an_image_a_turn_got_ahead_of_comes_back_with_it() {
    let mut desk = desk();
    let file = scratch("busy-image");
    let pane = desk.pane(&file);
    typed(&mut desk, &pane, "");
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.attach_for_test(b"one", window, cx)));
    desk.cx.run_until_parked();
    desk.key("enter");
    let id = desk.read(&pane, Pane::last_request_for_test);
    answers(&mut desk, &pane, id, CameTo::Busy("a turn is running".to_owned()));
    desk.cx.run_until_parked();
    assert!(in_box(&mut desk, &pane).contains("[Image #1]"), "{}", in_box(&mut desk, &pane));
    assert_eq!(desk.read(&pane, Pane::attached_for_test), 1, "and the image with it");
    assert!(asked_for(&file, STEER).is_empty(), "no words go without what they're about");
}

#[test]
fn what_a_command_cant_take_as_it_runs_goes_after_it() {
    let mut desk = desk();
    let file = scratch("steer-busy");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "and this");
    desk.key("enter");
    let id = desk.read(&pane, Pane::last_request_for_test);
    answers(&mut desk, &pane, id, CameTo::Busy("a turn is running".to_owned()));
    assert!(desk.read(&pane, Pane::steering_for_test).is_empty());
    assert_eq!(desk.read(&pane, Pane::waiting_for_test), ["and this"]);
    desk.said(&pane, vec![Said::TurnOver]);
    let_go(&mut desk, &pane);
    let prompts: Vec<Value> = sent_to(&file).into_iter().filter(|line| line["method"] == "session/prompt").map(|line| line["params"]["prompt"][0]["text"].clone()).collect();
    assert_eq!(prompts, [json!("go"), json!("and this")]);
}

#[test]
fn an_agent_that_ends_leaves_what_was_waiting_to_you() {
    let mut desk = desk();
    let file = scratch("ended-back");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    typed(&mut desk, &pane, "next");
    desk.key("tab");
    typed(&mut desk, &pane, "and more");
    desk.key("tab");
    desk.said(&pane, vec![Said::Ended]);
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "next\nand more");
    assert!(desk.read(&pane, Pane::waiting_for_test).is_empty());
}
