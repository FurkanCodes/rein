//! What a pane and the desk do, in a real window with no GPU behind it: what
//! comes of what an agent says, and of your keys and clicks.

use std::fs;
use std::path::{Path, PathBuf};

use gpui_kit::component::Root;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use rein_transcript::{Modes, Saved, Standing};
use serde_json::{Value, json};

use crate::agent::{CameTo, Opening, Said, Start};
use crate::look::{Wearing, wear};
use crate::pane::{About, Pane, Told};
use crate::panes::Panes;
use crate::probe::Probe;

/// A desk with nothing on it, in a window.
struct Desk {
    panes: Entity<Panes>,
    cx: VisualTestContext,
}

fn desk() -> Desk {
    let app = TestAppContext::single();
    app.update(|cx| {
        gpui_kit::init(cx);
        wear(false, cx);
    });
    let mut view = None;
    let window = app.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let panes = cx.new(|cx| Panes::new(Probe::new(), vec!["false".to_owned()], Wearing::Light, window, cx));
            view = Some(panes.clone());
            cx.new(|cx| Root::new(panes, window, cx))
        })
    });
    let handle: AnyWindowHandle = window.expect("the window opened").into();
    Desk { panes: view.expect("the window opened"), cx: VisualTestContext::from_window(handle, &app) }
}

/// A file for what an agent is sent.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rein-behaviour-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join("got")
}

/// What an agent was sent, in order.
fn sent_to(file: &Path) -> Vec<Value> {
    fs::read_to_string(file).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

impl Desk {
    /// A pane whose agent keeps what it's sent in `file`, and has started a
    /// conversation. What it says is up to the test, through [`Desk::said`].
    fn pane(&mut self, file: &Path) -> Entity<Pane> {
        let command = ["sh".to_owned(), "-c".to_owned(), format!("cat > {}", file.display())];
        let pane = self.cx.update(|window, cx| {
            self.panes.update(cx, |panes, cx| {
                let about = About { cwd: "/".to_owned(), ..About::default() };
                let pane = panes.open(&Start { command: &command, cwd: "/", env: &[], opening: Opening::New }, about, window, cx);
                panes.look_at(&pane, true, window, cx);
                pane
            })
        });
        let modes = Modes::of(&json!({ "currentModeId": "trust", "availableModes": [{ "id": "trust", "name": "trust" }, { "id": "ask", "name": "ask" }] }));
        self.said(&pane, vec![Said::Modes(modes.expect("they read")), Said::Started("session".to_owned())]);
        pane
    }

    fn said(&mut self, pane: &Entity<Pane>, said: Vec<Said>) {
        self.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.take(said, cx)));
        self.cx.run_until_parked();
    }

    fn asked(&mut self, pane: &Entity<Pane>, id: u64, command: &str) {
        let tool = json!({ "sessionUpdate": "tool_call", "toolCallId": format!("call-{id}"), "title": format!("Run {command}"), "kind": "execute", "status": "pending", "rawInput": { "command": command } });
        let options = json!([{ "optionId": "yes", "name": "Allow", "kind": "allow_once" }, { "optionId": "no", "name": "Reject", "kind": "reject_once" }]);
        self.said(pane, vec![Said::Update(tool), Said::Asked { id: json!(id), call: format!("call-{id}"), options }]);
    }

    /// Writes `text` as if you'd pressed Send.
    fn send(&mut self, pane: &Entity<Pane>, text: &str) {
        self.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.send(text, cx)));
        self.cx.run_until_parked();
    }

    fn told(&mut self, pane: &Entity<Pane>, told: Told) {
        self.cx.update(|_, cx| pane.update(cx, |_, cx| cx.emit(told)));
        self.cx.run_until_parked();
    }

    fn read<T>(&mut self, pane: &Entity<Pane>, look: impl FnOnce(&Pane) -> T) -> T {
        self.cx.update(|_, cx| look(pane.read(cx)))
    }

    /// Whether your keys are in `pane`'s box.
    fn keys_in(&mut self, pane: &Entity<Pane>) -> bool {
        self.cx.update(|window, cx| pane.read(cx).input.read(cx).focus_handle(cx).is_focused(window))
    }
}

#[test]
fn a_refused_mode_leaves_the_turn_its_question_and_the_bar_as_they_were() {
    let mut desk = desk();
    let file = scratch("mode");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    desk.asked(&pane, 7, "git push");
    let mode = desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.ask_mode_for_test("read-only")));
    desk.said(&pane, vec![Said::Answer { id: mode, came_to: CameTo::Busy("a turn is running".to_owned()) }]);
    assert!(desk.read(&pane, Pane::is_running), "a mode turned down doesn't end the turn");
    assert!(desk.read(&pane, Pane::is_asking), "and what it asked still needs you: the agent's waiting on it");
    assert_eq!(desk.read(&pane, Pane::standing), Standing::Asking);
    assert_eq!(desk.read(&pane, Pane::mode_for_test), Some("trust".to_owned()), "the bar says what it's in, not what you asked for");
}

#[test]
fn a_mode_counts_once_the_agent_has_said_so() {
    let mut desk = desk();
    let file = scratch("mode-yes");
    let pane = desk.pane(&file);
    let mode = desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.ask_mode_for_test("ask")));
    assert_eq!(desk.read(&pane, Pane::mode_for_test), Some("trust".to_owned()));
    desk.said(&pane, vec![Said::Answer { id: mode, came_to: CameTo::Done }]);
    assert_eq!(desk.read(&pane, Pane::mode_for_test), Some("ask".to_owned()));
}

#[test]
fn a_prompt_that_meets_a_turn_already_running_goes_into_it() {
    let mut desk = desk();
    let file = scratch("busy");
    let pane = desk.pane(&file);
    desk.send(&pane, "hello");
    let prompt = desk.read(&pane, Pane::last_request_for_test);
    desk.said(&pane, vec![Said::TurnStarted, Said::Answer { id: prompt, came_to: CameTo::Busy("a turn is running".to_owned()) }]);
    assert!(desk.read(&pane, Pane::is_running), "the turn that got there first goes on");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.finish_agent_for_test()));
    let sent = sent_to(&file);
    let steer = sent.last().unwrap();
    assert_eq!((steer["method"].as_str(), steer["params"]["message"].as_str()), (Some("_io.github.furkancodes/steer"), Some("hello")), "{sent:?}");
}

#[test]
fn a_prompt_turned_down_for_any_other_reason_is_a_failed_turn() {
    let mut desk = desk();
    let file = scratch("refused");
    let pane = desk.pane(&file);
    desk.send(&pane, "hello");
    let prompt = desk.read(&pane, Pane::last_request_for_test);
    desk.said(&pane, vec![Said::Answer { id: prompt, came_to: CameTo::Refused("the key was refused".to_owned()) }]);
    assert!(!desk.read(&pane, Pane::is_running));
    assert_eq!(desk.read(&pane, Pane::standing), Standing::Failed);
    assert_eq!(desk.read(&pane, |pane| pane.failure().map(str::to_owned)), Some("the key was refused".to_owned()));
}

#[test]
fn a_second_press_of_a_double_click_answers_nothing() {
    let mut desk = desk();
    let file = scratch("double");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    desk.asked(&pane, 7, "git status");
    desk.asked(&pane, 8, "git push");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.pretend_waited_for_test()));
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.press_for_test(&json!(7), "yes", cx)));
    assert_eq!(desk.read(&pane, Pane::question_count_for_test), 1, "the first press answers the question it was drawn for");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.press_for_test(&json!(7), "yes", cx)));
    assert_eq!(desk.read(&pane, Pane::question_count_for_test), 1, "drawn for the first, it can't answer the second");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.press_for_test(&json!(8), "yes", cx)));
    assert_eq!(desk.read(&pane, Pane::question_count_for_test), 1, "and even drawn for the second, it hasn't waited to be read");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.pretend_waited_for_test()));
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.press_for_test(&json!(7), "yes", cx)));
    assert_eq!(desk.read(&pane, Pane::question_count_for_test), 1, "however long it's waited, a press drawn for another question isn't for this one");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.finish_agent_for_test()));
    let answers = sent_to(&file).into_iter().filter(|line| line["result"]["outcome"].is_object()).count();
    assert_eq!(answers, 1, "only one yes went to the agent");
}

#[test]
fn a_question_nothing_has_drawn_yet_cant_be_answered() {
    let mut desk = desk();
    let file = scratch("undrawn");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    desk.asked(&pane, 7, "git status");
    desk.asked(&pane, 8, "git push");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.pretend_waited_for_test()));
    let left = desk.cx.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            // One after the other, with no frame between them.
            pane.press_for_test(&json!(7), "yes", cx);
            pane.press_for_test(&json!(8), "yes", cx);
            pane.question_count_for_test()
        })
    });
    assert_eq!(left, 1, "a key can arrive before the frame that shows what it would answer");
}

#[test]
fn closing_a_sub_agent_before_it_starts_tells_the_pane_that_asked() {
    let mut desk = desk();
    let file = scratch("sub-agent");
    let parent = desk.pane(&file);
    let about =
        About { name: Some("tests".to_owned()), cwd: "/".to_owned(), started_for: Some((parent.downgrade(), json!(9))), parent: Some(parent.downgrade()), ..About::default() };
    let child = desk.cx.update(|window, cx| {
        let pane = cx.new(|cx| Pane::new(None, about, window, cx));
        desk.panes.update(cx, |panes, cx| panes.hold_for_test(pane.clone(), window, cx));
        pane
    });
    desk.told(&child, Told::Close);
    desk.cx.update(|_, cx| parent.update(cx, |pane, _| pane.finish_agent_for_test()));
    let refused = sent_to(&file).into_iter().find(|line| line["id"] == 9).expect("it was told");
    assert_eq!(refused["error"]["message"], "its pane was closed", "it waits on this with no timeout");
}

#[test]
fn hiding_or_closing_the_pane_youre_at_takes_you_to_another() {
    let mut desk = desk();
    let (first, second) = (scratch("hide-1"), scratch("hide-2"));
    let (a, b) = (desk.pane(&first), desk.pane(&second));
    assert!(desk.read(&b, Pane::is_looking));
    desk.told(&b, Told::Hide);
    assert!(!desk.read(&b, Pane::is_looking), "a pane you can't see isn't one you're at");
    assert!(desk.read(&a, Pane::is_looking));
    assert!(desk.keys_in(&a), "and your keys came too");
    desk.told(&a, Told::Close);
    let on_desk = desk.cx.update(|window, cx| desk.panes.read(cx).holds_keys_for_test(window));
    assert!(on_desk, "with no pane left the window has them, or ⌘N reaches nothing");
}

#[test]
fn a_press_on_another_pane_brings_the_keys_there() {
    let mut desk = desk();
    let (first, second) = (scratch("press-1"), scratch("press-2"));
    let (a, b) = (desk.pane(&first), desk.pane(&second));
    desk.cx.update(|window, cx| desk.panes.update(cx, |panes, cx| panes.look_at(&a, true, window, cx)));
    desk.cx.run_until_parked();
    assert!(desk.keys_in(&a));
    desk.told(&b, Told::Looked);
    assert!(desk.keys_in(&b), "the pane with the ring has the keys: what you type next goes where it looks like it will");
}

#[test]
fn what_you_write_stays_in_the_box_of_an_agent_thats_gone() {
    let mut desk = desk();
    let file = scratch("ended");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![Said::Ended]);
    let typed = desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.input.update(cx, |input, cx| input.set_value("a long message", window, cx));
            pane.send_box_for_test(window, cx);
            pane.input.read(cx).value()
        })
    });
    assert_eq!(typed.to_string(), "a long message");
}

#[test]
fn a_saved_conversation_thats_open_is_gone_to_not_opened_again() {
    let mut desk = desk();
    let file = scratch("saved");
    desk.pane(&file);
    let saved = Saved::listed(&json!({ "sessions": [{ "sessionId": "session", "cwd": "/" }] }));
    desk.cx.update(|window, cx| desk.panes.update(cx, |panes, cx| panes.open_saved_for_test(&saved[0], window, cx)));
    assert_eq!(desk.cx.update(|_, cx| desk.panes.read(cx).panes().len()), 1, "two agents on one file would each write their own history into it");
}

#[test]
fn a_saved_conversations_old_steps_arent_this_turns() {
    let mut desk = desk();
    let file = scratch("strip");
    let pane = desk.pane(&file);
    let step = json!({ "sessionUpdate": "tool_call", "toolCallId": "old", "title": "Read a", "kind": "read", "status": "completed" });
    desk.said(&pane, vec![Said::Update(step), Said::Started("session".to_owned())]);
    assert!(desk.read(&pane, Pane::ticks).is_empty());
}

#[test]
fn the_beat_rests_while_a_question_waits_for_you() {
    let mut desk = desk();
    let file = scratch("beat");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    assert!(desk.read(&pane, Pane::is_beating_for_test), "a turn running shows it");
    desk.asked(&pane, 7, "git push");
    assert!(!desk.read(&pane, Pane::is_beating_for_test), "eight frames a second, for as long as you're away");
}
