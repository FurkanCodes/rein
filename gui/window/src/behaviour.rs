//! What a pane and the desk do, in a real window with no GPU behind it: what
//! comes of what an agent says, and of your keys and clicks.

use std::fs;
use std::path::{Path, PathBuf};

use gpui_kit::component::Root;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, Focusable as _, Keystroke, TestAppContext, VisualTestContext};
use rein_transcript::{Asking, Modes, Saved, Setting, Standing, Words};
use serde_json::{Value, json};

use crate::agent::{CameTo, Opening, Said, Start};
use crate::look::{Wearing, wear};
use crate::pane::{About, Pane, Picking, Tab, Told};
use crate::panes::Panes;
use crate::probe::Probe;

/// A desk with nothing on it, in a window.
struct Desk {
    panes: Entity<Panes>,
    cx: VisualTestContext,
}

fn desk() -> Desk {
    desk_serving(vec!["false".to_owned()])
}

/// The same, where an agent is started with `serve`.
fn desk_serving(serve: Vec<String>) -> Desk {
    let app = TestAppContext::single();
    app.update(|cx| {
        gpui_kit::init(cx);
        crate::panes::bind_keys(cx);
        wear(false, cx);
    });
    let mut view = None;
    let window = app.update(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            let panes = cx.new(|cx| Panes::new(Probe::new(), serve, Wearing::Light, window, cx));
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

/// What an agent was sent, in order, as far as it's whole: this looks while
/// the agent is writing, and a line with no end yet is half a message.
fn sent_to(file: &Path) -> Vec<Value> {
    let got = fs::read_to_string(file).unwrap_or_default();
    let whole = got.rfind('\n').map_or("", |end| &got[..end]);
    whole.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
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
        self.said(pane, vec![Said::Update(tool), Said::Asked { id: json!(id), call: format!("call-{id}"), options, words: Words::default() }]);
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

    /// Puts your keys where nothing types.
    fn keys_elsewhere(&mut self) {
        let focus = self.cx.update(|_, cx| self.panes.read(cx).focus.clone());
        self.cx.update(|window, cx| focus.focus(window, cx));
    }

    /// Whether your keys are in `pane`'s box.
    fn keys_in(&mut self, pane: &Entity<Pane>) -> bool {
        self.cx.update(|window, cx| pane.read(cx).input.read(cx).focus_handle(cx).is_focused(window))
    }
}

fn menu_asked() -> Said {
    let params = json!({ "title": "choose a model", "items": [{ "name": "gpt-5", "about": "OpenAI" }, { "name": "opus", "about": "Anthropic" }, { "name": "sonnet", "about": "Anthropic" }], "current": 0 });
    Said::Asking { id: json!(40), asking: Asking::of("_io.github.furkancodes/choose", &params).expect("it reads") }
}

fn answer_to(file: &Path, id: u64) -> Option<Value> {
    sent_to(file).into_iter().find(|line| line["id"] == id && line.get("method").is_none()).map(|line| line["result"].clone())
}

impl Desk {
    /// Lets go of `pane`'s agent, and reads what it was told under `id`.
    fn answered(&mut self, pane: &Entity<Pane>, file: &Path, id: u64) -> Option<Value> {
        self.cx.update(|_, cx| pane.update(cx, |pane, _| pane.finish_agent_for_test()));
        answer_to(file, id)
    }

    /// Writes `text` in the box and presses Send.
    fn box_says(&mut self, pane: &Entity<Pane>, text: &'static str) {
        self.cx.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                pane.input.update(cx, |input, cx| input.set_value(text, window, cx));
                pane.send_box_for_test(window, cx);
            });
        });
        self.cx.run_until_parked();
    }

    fn key(&mut self, stroke: &'static str) {
        self.cx.update(|window, cx| {
            let key = Keystroke::parse(stroke).expect("a key");
            window.dispatch_keystroke(key, cx);
        });
        self.cx.run_until_parked();
    }
}

/// Puts `text` in the box, with the keys there, as you'd type it.
fn typed(desk: &mut Desk, pane: &Entity<Pane>, text: &'static str) {
    desk.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.input.update(cx, |input, cx| input.focus(window, cx));
            pane.set_box_for_test("", window, cx);
        });
    });
    for letter in text.chars() {
        let key = match letter {
            ' ' => "space".to_owned(),
            '\n' => "shift-enter".to_owned(),
            letter => letter.to_string(),
        };
        desk.key(key.leak());
    }
    desk.cx.run_until_parked();
}

fn in_box(desk: &mut Desk, pane: &Entity<Pane>) -> String {
    desk.cx.update(|_, cx| pane.read(cx).input.read(cx).value().to_string())
}

/// It answered what it was asked under `id`.
fn answers(desk: &mut Desk, pane: &Entity<Pane>, id: u64, came_to: CameTo) {
    desk.said(pane, vec![Said::Answer { id, came_to }]);
}

/// What the pane asked of its agent under `method`, by number.
fn asked_for(file: &Path, method: &str) -> Vec<u64> {
    sent_to(file).into_iter().filter(|line| line["method"] == method).filter_map(|line| line["id"].as_u64()).collect()
}

/// How many times it's been asked `method`, looking until that's `expected`:
/// it's a process of its own, and hears in its own time.
fn asks_of(file: &Path, method: &str, expected: usize) -> usize {
    for _ in 0..200 {
        let count = asked_for(file, method).len();
        if count >= expected {
            return count;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    asked_for(file, method).len()
}

/// Lets go of `pane`'s agent, so what it was sent can be read.
fn let_go(desk: &mut Desk, pane: &Entity<Pane>) {
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.finish_agent_for_test()));
}

const SENT: &str = "_io.github.furkancodes/sent";
const PATHS: &str = "_io.github.furkancodes/paths";
const STEER: &str = "_io.github.furkancodes/steer";

/// A pane that's been told what you sent before, and has finished asking.
fn with_sent(desk: &mut Desk, file: &Path, sent: &[&str]) -> Entity<Pane> {
    let pane = desk.pane(file);
    let id = wait_for_ask(file, SENT);
    told(desk, &pane, id, json!({ "sent": sent }));
    pane
}

/// The agent writes what it's sent to a file as it comes, so this looks
/// until it's there.
fn wait_for_ask(file: &Path, method: &str) -> u64 {
    for _ in 0..200 {
        if let Some(id) = asked_for(file, method).last() {
            return *id;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("nothing asked {method}: {:?}", sent_to(file));
}

/// It answered what it was asked under `id`, saying `said`.
fn told(desk: &mut Desk, pane: &Entity<Pane>, id: u64, said: Value) {
    answers(desk, pane, id, CameTo::Told(said));
}

/// A pane that's been told what `@` can point at, with `word` in the box.
fn with_paths(desk: &mut Desk, file: &Path, word: &'static str) -> Entity<Pane> {
    let pane = desk.pane(file);
    typed(desk, &pane, word);
    let id = wait_for_ask(file, PATHS);
    told(desk, &pane, id, json!({ "paths": ["src/", "src/main.rs", "README.md"] }));
    pane
}

const UNDO_TURNS: &str = "_io.github.furkancodes/undo_turns";
const UNDO_PLAN: &str = "_io.github.furkancodes/undo_plan";

fn turn_line(desk: &mut Desk, pane: &Entity<Pane>, text: &str, turn: Option<u64>) {
    let line = json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text }, "_meta": { "io.github.furkancodes/note": true, "io.github.furkancodes/turn": turn } });
    desk.said(pane, vec![Said::Update(line)]);
}

const CONFIG: &str = "_io.github.furkancodes/config";
const SET_CONFIG: &str = "_io.github.furkancodes/set_config";

fn config_page(effort: &str) -> Value {
    json!({ "file": "~/.config/rein/config", "keys": [{ "key": "effort", "file": effort }], "starts": { "effort": effort, "mode": "trust" }, "choices": { "effort": ["low", "high"] }, "rules": [], "skills": [], "hooks": [], "notes": [], "servers": [] })
}

#[test]
fn settings_open_with_a_key_set_one_through_the_rein_behind_the_pane_and_close_with_esc() {
    use rein_transcript::config::Key;
    let mut desk = desk();
    let file = scratch("settings-page");
    let pane = desk.pane(&file);
    desk.key("cmd-,");
    assert!(desk.read(&pane, Pane::shows_page), "⌘, opens it over the pane you're at");
    let id = wait_for_ask(&file, CONFIG);
    told(&mut desk, &pane, id, config_page("low"));
    assert_eq!(desk.read(&pane, |pane| pane.page_starts_for_test(Key::Effort)), Some("low".to_owned()));
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.set_key_for_test(Key::Effort, Some("high"), cx)));
    let set = wait_for_ask(&file, SET_CONFIG);
    let asked = sent_to(&file).into_iter().find(|line| line["id"] == set).unwrap();
    assert_eq!((asked["params"]["key"].clone(), asked["params"]["value"].clone()), (json!("effort"), json!("high")), "{asked}");
    answers(&mut desk, &pane, set, CameTo::Done);
    assert_eq!(asks_of(&file, CONFIG, 2), 2, "set, the page is read again");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.set_key_for_test(Key::Effort, Some("loads"), cx)));
    let refused = *asked_for(&file, SET_CONFIG).last().unwrap();
    answers(&mut desk, &pane, refused, CameTo::Refused("effort should be one of low, high".to_owned()));
    assert_eq!(desk.read(&pane, Pane::page_said_for_test), Some("effort should be one of low, high".to_owned()), "said where it was set");
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.add_rule_for_test("ask git push --force", window, cx)));
    let rule = wait_for_ask(&file, "_io.github.furkancodes/rule");
    answers(&mut desk, &pane, rule, CameTo::Busy("a turn is running".to_owned()));
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).rule_box_for_test(cx)), "ask git push --force", "turned down, it's there to send again");
    assert!(desk.read(&pane, Pane::page_said_for_test).is_some_and(|said| said.contains("once the turn")));
    desk.key("escape");
    assert!(desk.read(&pane, Pane::shows_page), "esc closes the rule's box first");
    desk.key("escape");
    assert!(!desk.read(&pane, Pane::shows_page), "then the page");
    let_go(&mut desk, &pane);
}

fn model_setting() -> Said {
    let options = json!([
        { "id": "model", "currentValue": "openai/gpt-5", "options": [{ "group": "openai", "name": "OpenAI", "options": [{ "value": "openai/gpt-5", "name": "gpt-5", "description": "500000 tokens" }] }, { "group": "groq", "name": "Groq", "options": [{ "value": "groq/llama", "name": "groq/llama" }] }] },
        { "id": "effort", "currentValue": "low", "options": [{ "value": "low", "name": "low" }, { "value": "high", "name": "high" }] },
    ]);
    Said::Settings(Setting::listed(&options))
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
    desk.cx.run_until_parked();
    let draft = desk.cx.update(|_, cx| desk.panes.read(cx).draft_for_test()).expect("with nothing on the desk, there's a box to start in");
    assert!(desk.keys_in(&draft), "with no pane left the box to start in has them, and ⌘N still reaches the window");
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
fn esc_at_a_question_is_rein_s_own_no_and_the_turn_ends_with_it() {
    let mut desk = desk();
    let file = scratch("instead");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    let step = json!({ "sessionUpdate": "tool_call", "toolCallId": "c", "title": "Edit a.rs", "kind": "edit", "status": "pending", "content": [{ "type": "diff", "path": "/p/a.rs", "oldText": "a", "newText": "b" }] });
    let options = json!([
        { "optionId": "allow", "name": "Yes", "kind": "allow_once" },
        { "optionId": "reject", "name": "No", "kind": "reject_once" },
        { "optionId": "stop", "name": "No, and tell rein what to do instead", "kind": "reject_once", "_meta": { "io.github.furkancodes/instead": true } },
    ]);
    desk.said(&pane, vec![Said::Update(step), Said::Asked { id: json!(7), call: "c".to_owned(), options, words: Words::default() }]);
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.pretend_waited_for_test()));
    desk.key("escape");
    assert_eq!(desk.read(&pane, Pane::question_count_for_test), 0);
    assert_eq!(desk.answered(&pane, &file, 7), Some(json!({ "outcome": { "outcome": "selected", "optionId": "stop" } })));
    assert!(!sent_to(&file).iter().any(|line| line["method"] == "session/cancel"), "the answer stops the turn, as the terminal's does");
}

#[test]
fn a_press_on_what_takes_no_keys_leaves_them_with_the_agent_you_are_at() {
    let mut desk = desk();
    let file = scratch("press-keys");
    let pane = desk.pane(&file);
    let panes = desk.panes.clone();
    let press = |desk: &mut Desk| {
        desk.keys_elsewhere();
        desk.cx.update(|window, cx| panes.update(cx, |panes, cx| panes.keys_home(window, cx)));
        desk.cx.run_until_parked();
    };
    press(&mut desk);
    assert!(desk.keys_in(&pane), "what you type next goes in its box");
    desk.key("h");
    assert_eq!(in_box(&mut desk, &pane), "h");
    typed(&mut desk, &pane, "");
    desk.said(&pane, vec![menu_asked()]);
    desk.cx.run_until_parked();
    press(&mut desk);
    desk.key("s");
    desk.cx.run_until_parked();
    assert_eq!(in_box(&mut desk, &pane), "", "a list that's up keeps them");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).asking_typed_for_test(cx)), Some("s".to_owned()));
}

#[test]
fn what_you_type_where_nothing_types_goes_in_the_box_with_the_keys() {
    let mut desk = desk();
    let file = scratch("typed-elsewhere");
    let pane = desk.pane(&file);
    desk.keys_elsewhere();
    desk.key("h");
    desk.key("i");
    assert_eq!(in_box(&mut desk, &pane), "hi", "the second too: the keys went with the first");
    assert!(desk.keys_in(&pane));
    desk.keys_elsewhere();
    desk.key("cmd-c");
    desk.key("enter");
    assert_eq!(in_box(&mut desk, &pane), "hi", "a shortcut isn't typing, and Enter would send what you can't see it send");
    desk.send(&pane, "go");
    desk.asked(&pane, 7, "git push");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.pretend_waited_for_test()));
    desk.keys_elsewhere();
    desk.key("y");
    assert_eq!(desk.read(&pane, Pane::question_count_for_test), 1, "typing elsewhere never answers a question");
}

#[test]
fn a_list_or_a_sheet_thats_up_keeps_what_you_type() {
    let mut desk = desk();
    let file = scratch("typed-held");
    let pane = desk.pane(&file);
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.open_picker_for_test(Picking::Mode, window, cx)));
    desk.cx.run_until_parked();
    desk.keys_elsewhere();
    desk.key("h");
    assert_eq!(in_box(&mut desk, &pane), "", "a list that's up");
    let mut desk = self::desk();
    let file = scratch("typed-held-palette");
    let pane = desk.pane(&file);
    desk.key("cmd-k");
    desk.cx.run_until_parked();
    desk.keys_elsewhere();
    desk.key("h");
    assert_eq!(in_box(&mut desk, &pane), "", "or the palette");
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
/// A question from a command, with no answer for Esc.
fn yes_asked(id: u64) -> Said {
    let params = json!({ "title": "Keep it?", "answers": [{ "key": "y", "says": "Yes" }, { "key": "n", "says": "No" }] });
    Said::Asking { id: json!(id), asking: Asking::of("_io.github.furkancodes/yes", &params).expect("a question") }
}

#[test]
fn a_press_as_a_commands_question_appears_answers_nothing() {
    let mut desk = desk();
    let file = scratch("yes-press");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![yes_asked(41)]);
    desk.cx.run_until_parked();
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.press_asking_for_test("y", window, cx)));
    assert!(desk.read(&pane, |pane| pane.asking_for_test().is_some()), "of a double click, the second press was meant for the list");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.since_for_test()));
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.press_asking_for_test("y", window, cx)));
    assert!(desk.read(&pane, |pane| pane.asking_for_test().is_none()));
    assert_eq!(desk.answered(&pane, &file, 41), Some(json!({ "key": "y" })));
}

#[test]
fn esc_leaves_a_commands_question_that_has_no_answer_for_it() {
    let mut desk = desk();
    let file = scratch("yes-esc");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![yes_asked(42)]);
    desk.cx.run_until_parked();
    desk.key("escape");
    assert!(desk.read(&pane, |pane| pane.asking_for_test().is_none()));
    assert_eq!(desk.answered(&pane, &file, 42), Some(json!({ "key": null })));
}

#[test]
fn typing_narrows_a_commands_menu_and_enter_tells_rein_which() {
    let mut desk = desk();
    let file = scratch("menu");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![menu_asked()]);
    assert_eq!(desk.read(&pane, Pane::standing), Standing::Asking, "the list of agents says it needs you");
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.type_in_asking_for_test("sonnet", window, cx)));
    desk.cx.run_until_parked();
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.take_asking_for_test(window, cx)));
    assert!(desk.read(&pane, |pane| pane.asking_for_test().is_none()));
    assert_eq!(desk.answered(&pane, &file, 40), Some(json!({ "picked": 2 })), "its place in the whole list, not what's left of it");
}
#[test]
fn the_arrows_move_through_a_menu_and_esc_leaves_it() {
    let mut desk = desk();
    let file = scratch("menu-keys");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![menu_asked()]);
    desk.key("down");
    desk.key("down");
    desk.key("down");
    desk.key("enter");
    assert_eq!(desk.answered(&pane, &file, 40), Some(json!({ "picked": 0 })), "round past the end, to the top");

    let file = scratch("menu-esc");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![menu_asked()]);
    desk.key("escape");
    assert_eq!(desk.answered(&pane, &file, 40), Some(json!({ "picked": null })));
}
#[test]
fn what_a_command_asks_for_goes_to_rein_and_the_box_is_left_alone() {
    let mut desk = desk();
    let file = scratch("read");
    let pane = desk.pane(&file);
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.input.update(cx, |input, cx| input.set_value("half a thought", window, cx))));
    let asked = Asking::of("_io.github.furkancodes/read", &json!({ "prompt": "Paste your key", "secret": true })).expect("it reads");
    desk.said(&pane, vec![Said::Asking { id: json!(41), asking: asked }]);
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.type_in_asking_for_test("sk-secret", window, cx)));
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.take_asking_for_test(window, cx)));
    let in_box = desk.cx.update(|_, cx| pane.read(cx).input.read(cx).value().to_string());
    assert_eq!(in_box, "half a thought", "what you were writing isn't your key");
    assert_eq!(desk.answered(&pane, &file, 41), Some(json!({ "text": "sk-secret" })));
}
#[test]
fn a_question_from_a_command_takes_its_keys_once_it_can_have_been_read() {
    let mut desk = desk();
    let file = scratch("yes");
    let pane = desk.pane(&file);
    let asked =
        Asking::of("_io.github.furkancodes/yes", &json!({ "title": "Start in edits from now on?", "answers": [{ "key": "n", "says": "No" }, { "key": "y", "says": "Yes" }] }))
            .expect("it reads");
    desk.said(&pane, vec![Said::Asking { id: json!(42), asking: asked }]);
    desk.key("y");
    assert!(desk.read(&pane, |pane| pane.asking_for_test().is_some()), "a key as it appears was meant for something else");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.since_for_test()));
    desk.key("y");
    assert_eq!(desk.answered(&pane, &file, 42), Some(json!({ "key": "y" })));
}
#[test]
fn what_a_command_asks_goes_when_the_command_does() {
    let mut desk = desk();
    let file = scratch("stopped");
    let pane = desk.pane(&file);
    desk.send(&pane, "/model");
    desk.said(&pane, vec![menu_asked()]);
    let prompt = desk.read(&pane, Pane::last_request_for_test);
    desk.said(&pane, vec![Said::Answer { id: prompt, came_to: CameTo::TurnOver }]);
    assert!(desk.read(&pane, |pane| pane.asking_for_test().is_none()), "nothing's left to take your answer");
    assert!(!desk.read(&pane, Pane::is_asking));
}
#[test]
fn a_commands_line_is_left_for_rein_to_say_back() {
    let mut desk = desk();
    let file = scratch("no-echo");
    let pane = desk.pane(&file);
    desk.send(&pane, "/login groq sk-on-the-line");
    assert_eq!(desk.read(&pane, Pane::rows_for_test), 0, "it may have a key in it, and only rein knows");
    desk.send(&pane, "and this?");
}
#[test]
fn the_windows_own_commands_are_the_windows() {
    let mut desk = desk();
    let (first, second) = (scratch("own-1"), scratch("own-2"));
    let (a, b) = (desk.pane(&first), desk.pane(&second));
    desk.box_says(&b, "/exit");
    assert_eq!(desk.cx.update(|_, cx| desk.panes.read(cx).panes().len()), 1, "it closes the pane, and nothing asks rein");
    desk.box_says(&a, "/new");
    let panes = desk.cx.update(|_, cx| desk.panes.read(cx).panes());
    assert_eq!(panes.len(), 1, "another in its place");
    assert!(panes[0] != a);
}
#[test]
fn copy_takes_the_last_answer_as_it_was_written() {
    let mut desk = desk();
    let file = scratch("copy");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![Said::Update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "**done**\n" } }))]);
    desk.box_says(&pane, "/copy");
    let copied = desk.cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some("**done**"));
}
#[test]
fn what_a_command_changes_the_bar_follows() {
    let mut desk = desk();
    let file = scratch("settings");
    let pane = desk.pane(&file);
    let settings = Setting::listed(&json!([{ "id": "mode", "currentValue": "ask", "options": [{ "value": "trust", "name": "⚠ trust" }, { "value": "ask", "name": "ask" }] }]));
    desk.said(&pane, vec![Said::Settings(settings)]);
    assert_eq!(desk.read(&pane, Pane::mode_for_test), Some("ask".to_owned()), "a /mode typed in the box, as much as one picked");
}
#[test]
fn shift_tab_goes_on_to_the_next_mode_and_back_round() {
    let mut desk = desk();
    let file = scratch("next-mode");
    let pane = desk.pane(&file);
    desk.cx.update(|window, cx| pane.update(cx, |pane, cx| pane.input.update(cx, |input, cx| input.focus(window, cx))));
    desk.key("shift-tab");
    let asked = desk.read(&pane, Pane::last_request_for_test);
    desk.said(&pane, vec![Said::Answer { id: asked, came_to: CameTo::Done }]);
    assert_eq!(desk.read(&pane, Pane::mode_for_test), Some("ask".to_owned()));
    desk.key("shift-tab");
    let asked = desk.read(&pane, Pane::last_request_for_test);
    desk.said(&pane, vec![Said::Answer { id: asked, came_to: CameTo::Done }]);
    assert_eq!(desk.read(&pane, Pane::mode_for_test), Some("trust".to_owned()));
}
#[test]
fn a_cell_waits_for_the_turn_to_end_as_any_command_does() {
    let mut desk = desk();
    let file = scratch("cell");
    let pane = desk.pane(&file);
    desk.send(&pane, "go");
    desk.cx.update(|_, cx| pane.update(cx, |pane, cx| pane.run_for_test("/model", cx)));
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.finish_agent_for_test()));
    let sent = sent_to(&file);
    assert_eq!(sent.iter().filter(|line| line["method"] == "session/prompt").count(), 1, "{sent:?}");
    assert!(!sent.iter().any(|line| line["method"] == "_io.github.furkancodes/steer"), "a command isn't words for the turn: {sent:?}");
}
#[test]
fn what_a_command_asks_isnt_a_turn_to_steer() {
    let mut desk = desk();
    let file = scratch("asked");
    let pane = desk.pane(&file);
    desk.send(&pane, "/mode");
    let menu = rein_transcript::Menu { title: "choose a mode".to_owned(), items: Vec::new(), current: None, at: None };
    desk.said(&pane, vec![Said::Asking { id: json!(7), asking: Asking::Choose(menu) }]);
    desk.box_says(&pane, "words");
    desk.cx.update(|_, cx| pane.update(cx, |pane, _| pane.finish_agent_for_test()));
    let sent = sent_to(&file);
    assert!(!sent.iter().any(|line| line["method"] == "_io.github.furkancodes/steer"), "{sent:?}");
    assert_eq!(desk.cx.update(|_, cx| pane.read(cx).input.read(cx).value().to_string()), "words", "and it's kept for after");
}

mod boards;
mod lists;
mod review;
mod writing;

/// What's been sent to `file`, once a line `wanted` is among it: the agent
/// writing it is another process, which takes its time.
fn sent_in_time(file: &Path, wanted: impl Fn(&Value) -> bool) -> Vec<Value> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let sent = sent_to(file);
        if sent.iter().any(&wanted) {
            return sent;
        }
        assert!(std::time::Instant::now() < until, "never sent: {sent:?}");
        std::thread::yield_now();
    }
}

/// A desk whose pane to start with keeps what it's sent in `file`.
fn desk_with_draft(file: &Path) -> (Desk, Entity<Pane>) {
    // It keeps every line, and lists no saved conversations when asked.
    let script = format!(
        r#"while read -r line; do printf '%s\n' "$line" >> {}; case "$line" in *session/list*) printf '%s\n' '{{"id":2,"result":{{"sessions":[]}}}}';; esac; done"#,
        file.display()
    );
    let mut desk = desk_serving(vec!["sh".to_owned(), "-c".to_owned(), script]);
    desk.cx.run_until_parked();
    let draft = desk.cx.update(|_, cx| desk.panes.read(cx).draft_for_test()).expect("an empty desk has a box to start in");
    (desk, draft)
}

#[test]
fn the_box_to_start_with_becomes_a_conversation_in_the_list_once_it_sends() {
    let file = scratch("draft-begins");
    let (mut desk, draft) = desk_with_draft(&file);
    assert!(desk.keys_in(&draft), "what you type goes in it from the start");
    assert!(desk.cx.update(|_, cx| desk.panes.read(cx).panes()).is_empty(), "it isn't a conversation in the list yet");
    desk.said(&draft, vec![Said::Started("session".to_owned())]);
    desk.box_says(&draft, "fix the flaky test");
    assert_eq!(desk.cx.update(|_, cx| desk.panes.read(cx).panes()), std::slice::from_ref(&draft), "sent, it's one of the agents");
    assert!(desk.cx.update(|_, cx| desk.panes.read(cx).draft_for_test()).is_none());
    assert!(desk.read(&draft, Pane::is_looking));
    desk.told(&draft, Told::Close);
    let again = desk.cx.update(|_, cx| desk.panes.read(cx).draft_for_test()).expect("closed, the desk is empty again, with a box to start in");
    assert_ne!(again, draft);
    sent_in_time(&file, |line| line["method"] == "session/prompt" && line["params"]["prompt"][0]["text"] == "fix the flaky test");
}

#[test]
fn signed_out_the_box_to_start_with_offers_the_ways_and_opens_once_signed_in() {
    use rein_transcript::signing::{Way, ways};
    let file = scratch("draft-signs-in");
    let (mut desk, draft) = desk_with_draft(&file);
    let offered = ways(&json!({ "ways": [{ "way": "chatgpt" }, { "way": "key", "providers": ["Anthropic"] }] }));
    desk.said(&draft, vec![Said::SignedOut(offered)]);
    assert!(desk.read(&draft, Pane::is_signing));
    desk.cx.update(|_, cx| draft.update(cx, |pane, cx| pane.sign_in_for_test(Way::Key, cx)));
    let id = desk.read(&draft, Pane::last_request_for_test);
    desk.said(&draft, vec![Said::Answer { id, came_to: CameTo::Told(json!({ "signedIn": true })) }]);
    desk.said(&draft, vec![Said::Started("session".to_owned())]);
    assert!(!desk.read(&draft, Pane::is_signing), "the conversation opened");
    sent_in_time(&file, |line| line["id"] == id && line["method"] == "_io.github.furkancodes/sign_in" && line["params"]["way"] == "key");
    let opened = sent_in_time(&file, |line| line["method"] == "session/new" && line["id"] == 2);
    assert_eq!(opened.iter().filter(|line| line["method"] == "session/new").count(), 2, "opened again once signed in: {opened:?}");
}

#[test]
fn a_pane_that_isnt_the_first_runs_shows_the_ways_to_sign_in_too() {
    let mut desk = desk();
    let file = scratch("pane-signs-in");
    let pane = desk.pane(&file);
    desk.said(&pane, vec![Said::SignedOut(rein_transcript::signing::ways(&json!({ "ways": [{ "way": "chatgpt" }] })))]);
    desk.cx.run_until_parked();
    desk.cx.update(|window, _| window.refresh());
    desk.cx.run_until_parked();
    assert!(desk.cx.debug_bounds("ways-in").is_some(), "drawn, not only known");
    let_go(&mut desk, &pane);
}

#[test]
fn a_sub_agents_step_opens_the_pane_it_runs_in() {
    let mut desk = desk();
    let file = scratch("open-sub-agent");
    let parent = desk.pane(&file);
    let about = |name: &str| About { name: Some(name.to_owned()), cwd: "/".to_owned(), parent: Some(parent.downgrade()), ..About::default() };
    let (linux, docs) = (about("linux"), about("docs"));
    let children = desk.cx.update(|window, cx| {
        [linux, docs].map(|about| {
            let pane = cx.new(|cx| Pane::new(None, about, window, cx));
            desk.panes.update(cx, |panes, cx| panes.hold_for_test(pane.clone(), window, cx));
            pane
        })
    });
    desk.cx.update(|window, cx| desk.panes.update(cx, |panes, cx| panes.look_at(&parent, true, window, cx)));
    desk.told(&parent, Told::OpenSubAgent("linux".to_owned()));
    assert!(desk.read(&children[0], Pane::is_looking), "the one it named");
    assert!(!desk.read(&children[1], Pane::is_looking));
}

#[test]
fn status_and_compact_typed_alone_open_the_context_panel_and_send_nothing() {
    let mut desk = desk();
    let file = scratch("status-panel");
    let pane = desk.pane(&file);
    desk.box_says(&pane, "/status");
    assert!(desk.read(&pane, |pane| pane.panel_shows_for_test(Tab::Context)));
    desk.box_says(&pane, "/compact");
    assert!(desk.read(&pane, |pane| pane.panel_shows_for_test(Tab::Context)), "it stays open: its Compact is a press away");
    let sent = desk.answered(&pane, &file, 0);
    assert_eq!(sent, None);
    assert!(!sent_to(&file).iter().any(|line| line["method"] == "session/prompt"), "{:?}", sent_to(&file));
}
