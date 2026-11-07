//! rein's window: all your agents side by side, each one to read and steer.
//! GPUI is still on trial for it, so it measures itself as well.
//!
//! `rein-window` opens with a list to start agents from, and `--agent 'rein
//! serve'` starts one with it. `--seconds 30 --type --out run.json` runs by
//! itself and reports.

mod agent;
#[cfg(test)]
mod behaviour;
mod look;
mod pane;
mod panes;
mod parts;
mod probe;

use std::time::{Duration, Instant};

use gpui_kit::base::TextSelection;
use gpui_kit::component::Root;
use gpui_kit::*;
use serde_json::json;

use crate::agent::{Opening, Start};
use crate::look::{Wearing, wear};
use crate::pane::{About, Pane};
use crate::panes::{NewAgent, Panes, SIDEBAR, here};
use crate::probe::Probe;

#[derive(Default)]
struct Flags {
    /// Which look to wear: your Mac's, unless a run pins one.
    wearing: Wearing,
    /// One agent command a pane.
    agents: Vec<Vec<String>>,
    /// How a new agent is started, if not as `rein serve`.
    serve: Option<Vec<String>>,
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
    /// Presses at each of these, this many seconds in.
    clicks: Vec<(f32, f32, f32)>,
    /// Moves the pointer about from this many seconds in, for that many.
    sweep: Option<(f32, f32)>,
    /// Puts the pointer here, this many seconds in, to look at what it does.
    over: Option<(f32, f32, f32)>,
    /// Presses here at that moment and doesn't let go.
    hold: bool,
    /// Presses these keys, this many seconds in.
    keys: Vec<(String, f32)>,
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
            "--sweep" => {
                let at = value()?;
                let mut parts = at.split(',').map(str::parse);
                let (Some(Ok(from)), Some(Ok(seconds)), None) = (parts.next(), parts.next(), parts.next()) else {
                    return Err("--sweep takes from,seconds".to_owned());
                };
                flags.sweep = Some((from, seconds));
            }
            "--over" | "--hold" => {
                let at = value()?;
                let mut parts = at.split(',').map(str::parse);
                let (Some(Ok(x)), Some(Ok(y)), Some(Ok(seconds)), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
                    return Err(format!("{flag} takes x,y,seconds"));
                };
                flags.over = Some((x, y, seconds));
                flags.hold = flag == "--hold";
            }
            "--key" => {
                let said = value()?;
                let (key, at) = said.split_once('@').ok_or("--key takes a key and when: y@2.5")?;
                flags.keys.push((key.to_owned(), at.parse().map_err(|_| "--key takes a key and when: y@2.5")?));
            }
            "--look" => {
                flags.wearing = match value()?.as_str() {
                    "light" => Wearing::Light,
                    "dark" => Wearing::Dark,
                    other => return Err(format!("--look takes light or dark, not {other}")),
                };
            }
            "--agent" => flags.agents.push(value()?.split_whitespace().map(str::to_owned).collect()),
            "--serve" => flags.serve = Some(value()?.split_whitespace().map(str::to_owned).collect()),
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
            "--click" => {
                let at = value()?;
                let mut parts = at.split(',').map(str::parse);
                let (Some(Ok(x)), Some(Ok(y)), Some(Ok(seconds)), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
                    return Err("--click takes x,y,seconds".to_owned());
                };
                flags.clicks.push((x, y, seconds));
            }
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

/// How often the UI thread's lateness is checked.
const BEAT: Duration = Duration::from_millis(4);
/// How often `--type` presses a key: a fast typist's pace.
const KEY_EVERY: Duration = Duration::from_millis(80);
/// How often `--resize` moves the edge, and how far, in steps of a drag.
const RESIZE_EVERY: Duration = Duration::from_millis(16);
const RESIZE_BY: f32 = 5.;
/// How many moves it takes to go out and back: 200 wider and back.
const RESIZE_MOVES: u16 = 80;
/// How wide the window is, then: panes of 900, and the list of agents.
const WIDTH: f32 = 900. + SIDEBAR;
/// How tall the window opens.
const HEIGHT: f32 = 780.;
/// Where a run's pointer goes: well inside the first pane.
const IN_FIRST_PANE: f32 = SIDEBAR + 120.;

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
fn keep_typing(window: AnyWindowHandle, input: Entity<gpui_kit::component::input::TextareaState>, probe: Probe, cx: &mut App) {
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
            let width = WIDTH - 200. + RESIZE_BY * f32::from(move_at.min(RESIZE_MOVES - move_at));
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
        position: point(px(IN_FIRST_PANE + 80.), px(300.)),
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

/// Moves the pointer back and forth over the window, as a hand does: a
/// window is seldom left alone with nothing near it.
fn sweep(handle: AnyWindowHandle, (from, seconds): (f32, f32), cx: &mut App) {
    cx.spawn(async move |cx| {
        let clock = cx.background_executor().clone();
        clock.timer(Duration::from_secs_f32(from)).await;
        let started = Instant::now();
        let mut step = 0_u32;
        while started.elapsed().as_secs_f32() < seconds {
            clock.timer(Duration::from_millis(16)).await;
            step = step.wrapping_add(1);
            let (x, y) = ((step * 17 % 1100) as f32, ((step * 7 % 600) + 60) as f32);
            let event = MouseMoveEvent { position: point(px(x), px(y)), pressed_button: None, modifiers: Modifiers::default() };
            let _ = handle.update(cx, |_, window, cx| window.dispatch_event(PlatformInput::MouseMove(event), cx));
        }
    })
    .detach();
}

/// Puts the pointer at `x`, `y`, `seconds` in, and leaves it there: pressed
/// down if `hold`. A frame later, as your hand would be.
fn point_at(handle: AnyWindowHandle, (x, y, seconds): (f32, f32, f32), hold: bool, cx: &mut App) {
    cx.spawn(async move |cx| {
        let clock = cx.background_executor().clone();
        let at = point(px(x), px(y));
        clock.timer(Duration::from_secs_f32(seconds)).await;
        let _ = handle.update(cx, |_, window, cx| {
            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: at, pressed_button: None, modifiers: Modifiers::default() }), cx)
        });
        clock.timer(Duration::from_millis(100)).await;
        if hold {
            let _ = handle.update(cx, |_, window, cx| press(at, false, window, cx));
        }
    })
    .detach();
}

/// Presses each of `keys`, when it says, as your keyboard would.
fn press_keys(handle: AnyWindowHandle, keys: Vec<(String, f32)>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let clock = cx.background_executor().clone();
        let mut passed = 0.;
        for (key, at) in keys {
            clock.timer(Duration::from_secs_f32((at - passed).max(0.))).await;
            passed = at;
            // Parsing takes only what lasts, and a flag lasts the whole run.
            let Ok(stroke) = Keystroke::parse(key.leak()) else { continue };
            let _ = handle.update(cx, |_, window, cx| window.dispatch_keystroke(stroke, cx));
        }
    })
    .detach();
}

/// Presses and releases at each of `clicks`, in turn, as your mouse would.
fn click_through(handle: AnyWindowHandle, clicks: Vec<(f32, f32, f32)>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let clock = cx.background_executor().clone();
        let started = Instant::now();
        for (x, y, seconds) in clicks {
            let at = point(px(x), px(y));
            if let Some(wait) = Duration::from_secs_f32(seconds).checked_sub(started.elapsed()) {
                clock.timer(wait).await;
            }
            // The pointer gets there a frame ahead, as yours does: a menu's
            // item takes a click only once it's been drawn as hovered.
            let _ = handle.update(cx, |_, window, cx| {
                window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent { position: at, pressed_button: None, modifiers: Modifiers::default() }), cx)
            });
            clock.timer(Duration::from_millis(100)).await;
            let _ = handle.update(cx, |_, window, cx| press(at, false, window, cx));
            clock.timer(Duration::from_millis(100)).await;
            let _ = handle.update(cx, |_, window, cx| release(at, Modifiers::default(), window, cx));
        }
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
        let (from, to) = (point(px(IN_FIRST_PANE), px(120.)), point(px(IN_FIRST_PANE), px(420.)));
        let _ = handle.update(cx, |_, window, cx| press(from, false, window, cx));
        wait(100).await;
        let _ = handle.update(cx, |_, window, cx| drag(point(px(IN_FIRST_PANE), px(260.)), window, cx));
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
    let cwd = here().display().to_string();
    // On macOS an app stays by default, and this one is only its window.
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).with_quit_mode(QuitMode::LastWindowClosed).run(move |cx| {
        gpui_kit::init(cx);
        // Nobody's looking by then, and past it nothing would end what won't.
        cx.on_app_quit(|_| async { crate::agent::wait_for_those_ending() }).detach();
        cx.bind_keys([KeyBinding::new(if cfg!(target_os = "macos") { "cmd-n" } else { "ctrl-n" }, NewAgent, None)]);
        let wearing = flags.wearing;
        wear(wearing.dark(cx.window_appearance()), cx);
        let probe = Probe::new();
        let bounds = Bounds::new(point(px(240.), px(60.)), size(px(WIDTH), px(HEIGHT)));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Lights over the list, where the top of the desk is.
            titlebar: Some(TitlebarOptions { title: Some("rein".into()), appears_transparent: true, traffic_light_position: Some(point(px(14.), px(16.))) }),
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
        let (resizing, focusing, resize_probe, clicks, keys) = (flags.resize, flags.focus, probe.clone(), flags.clicks.clone(), flags.keys.clone());
        let (over, hold, sweeping) = (flags.over, flags.hold, flags.sweep);
        let serve = flags.serve.clone().unwrap_or_else(|| ["rein", "serve"].map(str::to_owned).to_vec());
        let select_probe = probe.clone();
        let settle_probe = probe.clone();
        let (root_probe, typing_probe, scroll_probe, scroll) = (probe.clone(), probe.clone(), probe.clone(), flags.scroll);
        gpui_kit::open_window(options, cx, move |window, cx| {
            let view = cx.new(|cx| {
                let mut view = Panes::new(root_probe, serve, wearing, window, cx);
                for (i, command) in agents.iter().enumerate() {
                    let settles_at = settle.filter(|_| i == 0).map(|count| (count, settle_probe.clone()));
                    let about = About { cwd: cwd.clone(), settles_at, ..About::default() };
                    view.open(&Start { command, cwd: &cwd, env: &[], opening: Opening::New }, about, window, cx);
                }
                for _ in 0..empty {
                    view.add_empty(&cwd, window, cx);
                }
                view
            });
            let panes: Vec<Entity<Pane>> = view.read(cx).panes();
            if let Some(first) = panes.first() {
                view.update(cx, |view, cx| view.look_at(first, false, window, cx));
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
            if !clicks.is_empty() {
                click_through(window.window_handle(), clicks, cx);
            }
            if !keys.is_empty() {
                press_keys(window.window_handle(), keys, cx);
            }
            if let Some(over) = over {
                point_at(window.window_handle(), over, hold, cx);
            }
            if let Some(sweeping) = sweeping {
                sweep(window.window_handle(), sweeping, cx);
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
