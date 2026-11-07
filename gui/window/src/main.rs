//! The throwaway test window: all your agents side by side, and what it
//! takes to draw them. It exists to find out whether GPUI holds up on rein's
//! workload, so it measures itself, and gives up if it doesn't.
//!
//! `rein-window --agent 'rein serve' --agent 'rein serve'` is a window to try
//! by hand. `--seconds 30 --type --out run.json` runs by itself and reports.

mod agent;
mod pane;
mod probe;

use std::time::{Duration, Instant};

use gpui_kit::base::TextSelection;
use gpui_kit::component::Root;
use gpui_kit::*;
use serde_json::json;

use crate::agent::Agent;
use crate::pane::Pane;
use crate::probe::Probe;

#[derive(Default)]
struct Flags {
    /// One agent command a pane.
    agents: Vec<Vec<String>>,
    /// How many panes with no agent to add, to type in.
    empty: usize,
    /// Ends after this long, writing what it measured to `out`.
    seconds: Option<u64>,
    out: Option<String>,
    /// Types into the last pane as it runs, as you would.
    typing: bool,
    /// Stays above other windows and leaves your keyboard alone.
    aside: bool,
    /// Notes how long the UI thread is held up, which wakes it all the time.
    stalls: bool,
    /// The first pane's conversation is whole at this many messages.
    settle: Option<usize>,
    /// Scrolls the first pane this far a frame, once it's settled.
    scroll: Option<f32>,
    /// Counts what's drawn after this many seconds apart.
    mark: Option<u64>,
    /// Sent to every agent at the start.
    prompt: Option<String>,
    /// Selects across rows with the mouse, scrolls away, and copies.
    select: bool,
    /// Presses here once the first pane has a question up.
    press: Option<(f32, f32)>,
    /// Moves the window's edge in and out as it runs.
    resize: bool,
    /// Puts the caret in the last pane's box, where it would be in use.
    focus: bool,
}

fn flags() -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--agent" => flags.agents.push(value()?.split_whitespace().map(str::to_owned).collect()),
            "--empty" => flags.empty = value()?.parse().map_err(|_| "--empty takes a number")?,
            "--seconds" => flags.seconds = Some(value()?.parse().map_err(|_| "--seconds takes a number")?),
            "--out" => flags.out = Some(value()?),
            "--type" => flags.typing = true,
            "--aside" => flags.aside = true,
            "--stalls" => flags.stalls = true,
            "--select" => flags.select = true,
            "--resize" => flags.resize = true,
            "--focus" => flags.focus = true,
            "--settle" => flags.settle = Some(value()?.parse().map_err(|_| "--settle takes a number")?),
            "--scroll" => flags.scroll = Some(value()?.parse().map_err(|_| "--scroll takes a number")?),
            "--mark" => flags.mark = Some(value()?.parse().map_err(|_| "--mark takes a number")?),
            "--prompt" => flags.prompt = Some(value()?),
            "--press" => {
                let at = value()?;
                let (x, y) = at.split_once(',').and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?))).ok_or("--press takes x,y")?;
                flags.press = Some((x, y));
            }
            _ => return Err(format!("no such flag: {flag}")),
        }
    }
    Ok(flags)
}

/// Every pane, and what they cost.
struct Panes {
    panes: Vec<Entity<Pane>>,
    probe: Probe,
}

impl Render for Panes {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let probe = self.probe.clone();
        let frames = canvas(|_, _, _| (), move |_, (), _, _| probe.painted()).absolute().size_0();
        let pointer = self.probe.clone();
        // Cached: typing in one pane doesn't draw the others again.
        let panes = self.panes.iter().map(|pane| AnyView::from(pane.clone()).cached(StyleRefinement::default().flex_1().min_w_0().size_full()));
        div().size_full().flex().flex_row().on_mouse_move(move |_, _, _| pointer.pointer_moved()).children(panes).child(frames)
    }
}

/// How often the UI thread's lateness is checked.
const BEAT: Duration = Duration::from_millis(4);
/// How often `--type` presses a key: a fast typist's pace.
const KEY_EVERY: Duration = Duration::from_millis(80);
/// How often `--resize` moves the edge, and how far, in steps of a drag.
const RESIZE_EVERY: Duration = Duration::from_millis(16);
const RESIZE_BY: f32 = 5.;
/// How many moves it takes to go out and back: 700 to 900 wide and back.
const RESIZE_MOVES: u16 = 80;

/// Notes how late a timer wakes, which is how long the UI thread was busy.
fn listen_for_stalls(probe: Probe, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            let asked = Instant::now();
            cx.background_executor().timer(BEAT).await;
            probe.woke_late(asked.elapsed().saturating_sub(BEAT));
        }
    })
    .detach();
}

/// Presses keys in `input`, and clears it now and then, as you would.
fn keep_typing(window: AnyWindowHandle, input: Entity<gpui_kit::component::input::InputState>, probe: Probe, cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut pressed = 0_u32;
        loop {
            cx.background_executor().timer(KEY_EVERY).await;
            pressed += 1;
            let went = window.update(cx, |_, window, cx| {
                if pressed.is_multiple_of(40) {
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
                let key = Keystroke::parse("a").expect("a is a key");
                probe.key_sent();
                window.dispatch_keystroke(key, cx);
            });
            if went.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Drags the window's edge in and out, so every line wraps again as it goes.
fn keep_resizing(handle: AnyWindowHandle, probe: Probe, cx: &mut App) {
    cx.spawn(async move |cx| {
        let clock = cx.background_executor().clone();
        let (mut move_at, mut narrowest, mut widest) = (0, f32::MAX, 0_f32);
        loop {
            clock.timer(RESIZE_EVERY).await;
            move_at = (move_at + 1) % RESIZE_MOVES;
            let width = 700. + RESIZE_BY * f32::from(move_at.min(RESIZE_MOVES - move_at));
            let went = handle.update(cx, |_, window, _| {
                window.resize(size(px(width), px(640.)));
                // What the window is, so a run that moved nothing shows it.
                let now = f32::from(window.viewport_size().width);
                (narrowest, widest) = (narrowest.min(now), widest.max(now));
                probe.note("width_from_to", json!([narrowest, widest]));
            });
            if went.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Scrolls up by `by`, as a trackpad would: a real event, through the same
/// path as yours.
fn scroll(by: f32, window: &mut Window, cx: &mut App) {
    let event = ScrollEvent {
        position: point(px(200.), px(300.)),
        delta: ScrollDelta::Pixels(point(px(0.), px(by))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
        source: ScrollSource::default(),
    };
    window.dispatch_event(PlatformInput::Scroll(event), cx);
}

/// Goes on scrolling, a frame at a time.
fn keep_scrolling(by: f32, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        scroll(by, window, cx);
        keep_scrolling(by, window);
    });
}

/// What the first `count` characters of `text` come to, for setting a
/// selection beside what was copied: letters alone, as Markdown's marks are
/// in one and not the other.
fn start_of(text: &str, count: usize) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).take(count).collect()
}

fn press(at: Point<Pixels>, shift: bool, window: &mut Window, cx: &mut App) {
    let modifiers = Modifiers { shift, ..Modifiers::default() };
    window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: at, pressed_button: None, modifiers }), cx);
    window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: at, modifiers, click_count: 1, first_mouse: false }), cx);
}

fn drag(to: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let event = MouseMoveEvent { position: to, pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() };
    window.dispatch_event(PlatformInput::MouseMove(event), cx);
}

fn release(at: Point<Pixels>, modifiers: Modifiers, window: &mut Window, cx: &mut App) {
    window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: at, modifiers, click_count: 1 }), cx);
}

/// A press and release at `at` once `pane` is asking, through the same path
/// as your mouse: whether a button's answer gets back to the agent.
fn press_when_asked(handle: AnyWindowHandle, pane: Entity<Pane>, at: Point<Pixels>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let clock = cx.background_executor().clone();
        let wait = move |ms| clock.timer(Duration::from_millis(ms));
        while !cx.update(|cx| pane.read(cx).is_asking()) {
            wait(50).await;
        }
        wait(500).await;
        let _ = handle.update(cx, |_, window, cx| press(at, false, window, cx));
        wait(100).await;
        let _ = handle.update(cx, |_, window, cx| release(at, Modifiers::default(), window, cx));
    })
    .detach();
}

/// Real mouse events, in steps, with frames between: the selection gate.
fn select_across(handle: AnyWindowHandle, pane: Entity<Pane>, probe: Probe, cx: &mut App) {
    cx.spawn(async move |cx| {
        let clock = cx.background_executor().clone();
        let wait = move |ms| clock.timer(Duration::from_millis(ms));
        while !probe.is_settled() {
            wait(50).await;
        }
        wait(400).await;
        let (from, to) = (point(px(120.), px(120.)), point(px(120.), px(420.)));
        let _ = handle.update(cx, |_, window, cx| press(from, false, window, cx));
        wait(100).await;
        let _ = handle.update(cx, |_, window, cx| drag(point(px(120.), px(260.)), window, cx));
        wait(100).await;
        let _ = handle.update(cx, |_, window, cx| drag(to, window, cx));
        wait(100).await;
        let _ = handle.update(cx, |_, window, cx| release(to, Modifiers::default(), window, cx));
        wait(300).await;
        let _ = handle.update(cx, |_, window, cx| {
            let native = TextSelection::selected_text(window, cx);
            probe.note("native_bytes_across_rows", json!(native.len()));
            probe.note("native_start", json!(start_of(&native, 40)));
            let model = pane.read(cx).selection().unwrap_or_default();
            probe.note("model_bytes_across_rows", json!(model.len()));
            probe.note("model_start", json!(start_of(&model, 40)));
            // A wheel, as you'd scroll: only that stops following the end.
            scroll(4_000_000., window, cx);
        });
        wait(800).await;
        let _ = handle.update(cx, |_, window, cx| {
            let native = TextSelection::selected_text(window, cx);
            probe.note("native_bytes_after_scrolling_away", json!(native.len()));
            let model = pane.read(cx).selection().unwrap_or_default();
            probe.note("model_bytes_after_scrolling_away", json!(model.len()));
            press(from, true, window, cx);
        });
        wait(100).await;
        let _ = handle.update(cx, |_, window, cx| release(from, Modifiers { shift: true, ..Modifiers::default() }, window, cx));
        wait(300).await;
        let _ = handle.update(cx, |_, _, cx| {
            let started = Instant::now();
            let model = pane.read(cx).selection().unwrap_or_default();
            probe.note("copy_ms", json!(started.elapsed().as_secs_f64() * 1000.0));
            probe.note("model_bytes_extended", json!(model.len()));
            probe.note("model_lines_extended", json!(model.lines().count()));
        });
    })
    .detach();
}

fn main() {
    let flags = match flags() {
        Ok(flags) => flags,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let cwd = std::env::current_dir().map(|dir| dir.display().to_string()).unwrap_or_default();
    // On macOS an app stays by default, and this one is only its window.
    gpui_kit::application().with_quit_mode(QuitMode::LastWindowClosed).run(move |cx| {
        gpui_kit::init(cx);
        let probe = Probe::new();
        let bounds = Bounds::new(point(px(560.), px(60.)), size(px(900.), px(640.)));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            focus: !flags.aside,
            kind: if flags.aside { WindowKind::PopUp } else { WindowKind::Normal },
            // A window not in front is held to about 30 frames a second, and
            // one that leaves your keyboard alone is never in front: without
            // this, a run measures that limit, not the window.
            inactive_frame_interval: if flags.aside { None } else { WindowOptions::default().inactive_frame_interval },
            ..WindowOptions::default()
        };
        let (agents, empty, typing, settle, prompt, select, press_at) =
            (flags.agents.clone(), flags.empty, flags.typing, flags.settle, flags.prompt.clone(), flags.select, flags.press);
        let (resizing, focusing, resize_probe) = (flags.resize, flags.focus, probe.clone());
        let select_probe = probe.clone();
        let settle_probe = probe.clone();
        let (root_probe, typing_probe, scroll_probe, scroll) = (probe.clone(), probe.clone(), probe.clone(), flags.scroll);
        gpui_kit::open_window(options, cx, move |window, cx| {
            let mut panes: Vec<Entity<Pane>> = Vec::new();
            for (i, command) in agents.iter().enumerate() {
                let agent = Agent::start(command, &cwd).map_err(|e| eprintln!("{e}")).ok();
                let settles_at = settle.filter(|_| i == 0).map(|count| (count, settle_probe.clone()));
                panes.push(cx.new(|cx| Pane::new(agent, settles_at, window, cx)));
            }
            for _ in 0..empty {
                panes.push(cx.new(|cx| Pane::new(None, None, window, cx)));
            }
            if let Some(prompt) = &prompt {
                for pane in &panes {
                    pane.update(cx, |pane, cx| pane.send(prompt, cx));
                }
            }
            if typing && let Some(last) = panes.last() {
                let input = last.read(cx).input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
                keep_typing(window.window_handle(), input, typing_probe, cx);
            }
            if select && let Some(first) = panes.first() {
                select_across(window.window_handle(), first.clone(), select_probe, cx);
            }
            if focusing && let Some(last) = panes.last() {
                let input = last.read(cx).input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
            }
            if resizing {
                keep_resizing(window.window_handle(), resize_probe, cx);
            }
            if let (Some((x, y)), Some(first)) = (press_at, panes.first()) {
                press_when_asked(window.window_handle(), first.clone(), point(px(x), px(y)), cx);
            }
            if let Some(by) = scroll {
                let (handle, probe) = (window.window_handle(), scroll_probe);
                cx.spawn(async move |cx| {
                    while !probe.is_settled() {
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                    }
                    probe.mark();
                    let _ = handle.update(cx, |_, window, _| keep_scrolling(by, window));
                })
                .detach();
            }
            let view = cx.new(|_| Panes { panes, probe: root_probe });
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("failed to open window");
        // Started from a terminal, an app stays behind it unless it asks.
        if !flags.aside {
            cx.activate(true);
        }
        if flags.stalls {
            listen_for_stalls(probe.clone(), cx);
        }
        if let Some(seconds) = flags.mark {
            let probe = probe.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(seconds)).await;
                probe.mark();
            })
            .detach();
        }
        if let Some(seconds) = flags.seconds {
            let (out, probe) = (flags.out.clone(), probe.clone());
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(seconds)).await;
                let report = probe.report();
                match out {
                    Some(out) => {
                        if let Err(e) = std::fs::write(&out, report.to_string()) {
                            eprintln!("can't write {out}: {e}");
                        }
                    }
                    None => println!("{report}"),
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
}
