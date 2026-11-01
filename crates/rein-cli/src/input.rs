//! The keyboard, and the live part of the screen it drives: the input box,
//! what rein is doing above it, its questions, and the commands that match
//! what you type under it.
//!
//! ```text
//!        ◐ Running cargo test… (4s · esc to stop)
//!        ↳ and then commit it                          ← queued
//! ╭──────────────────────────────────────────────╮
//! │ › Now add a test                              │
//! ╰──────────────────────────────────────────────╯
//!  gpt-6.1-sol · ask · context ▰▱▱▱▱▱▱▱▱▱ 4% · cache 86%
//! ```
//!
//! The keyboard is read by its own thread, so that waiting for a key never
//! stops rein from noticing Ctrl-C. In a terminal it drives the box;
//! otherwise it reads plain lines, so piping into rein works.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::commands;
use crate::ui::editor::{Action, Editor};
use crate::ui::keys::{Key, Keys};
use crate::ui::screen::{self, Bottom, Drag, Pinned, Raw, Tone};
use crate::ui::style::{GUTTER, Style, fit, width};
use crate::ui::viewer::{Entry, Viewer};
use crate::ui::wrap::Wrap;

/// What reading a line produced.
pub enum Read {
    Line(String),
    /// Ctrl-C while typing. In a real terminal the box turns it into a
    /// keypress, so it arrives here instead of as a signal.
    Interrupted,
    /// Ctrl-D, or input ended.
    Eof,
    /// Shift-Tab: on to the next mode.
    NextMode,
}

/// What the keyboard is for right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Waiting {
    /// rein is working; keys type ahead, Enter queues what's typed, and
    /// Ctrl-C or Esc stop the turn.
    Nothing,
    Message,
    /// The answer to a question.
    Answer,
}

/// A question with a few answers, picked with its key, a digit, or the
/// arrows and Enter.
pub struct Question {
    pub title: String,
    /// The command it's about, shown whole under the title.
    pub command: Option<String>,
    /// Each answer: the key that picks it (`esc` for Esc), and what it says.
    pub answers: Vec<(&'static str, String)>,
}

/// The question on screen, which answer is picked, and since when it's
/// been asked.
struct Asking {
    question: Question,
    picked: usize,
    since: Instant,
}

/// Keys this soon after a question appears, or in a run of typing that
/// hasn't paused this long, still go to the box: someone typing ahead
/// can't answer a question by accident.
const GRACE: Duration = Duration::from_millis(500);

/// How long a note shows in place of the status bar: as long as a second
/// Ctrl-C still leaves.
const NOTICE: Duration = Duration::from_secs(2);

/// The most of your message the box shows at once; it scrolls for more.
const BOX_LINES: usize = 8;

/// The most commands listed under the box at once.
const MENU_LINES: usize = 6;

const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

pub struct Input {
    /// Plain lines: prompts to show; the thread reads one line for each.
    requests: std::sync::mpsc::Sender<String>,
    replies: tokio::sync::mpsc::UnboundedReceiver<Read>,
    /// A read asked for but not answered yet, kept if the asker gave up.
    pending: bool,
    /// The box, when rein runs in a terminal.
    boxed: Option<Arc<Mutex<Boxed>>>,
    /// Puts the keyboard back when rein ends.
    _raw: Option<Raw>,
}

/// The box, shared by the keyboard's thread, the clock's and the rest of rein.
struct Boxed {
    pinned: Pinned,
    editor: Editor,
    waiting: Waiting,
    /// What the empty box says while waiting for a typed answer.
    prompt: String,
    asking: Option<Asking>,
    /// What rein is doing, and since when its turn started.
    busy: Option<(String, Instant)>,
    /// Messages typed while rein worked, to be sent when it's done.
    queued: Vec<String>,
    /// Messages sent with big pastes in them, and how the box showed them,
    /// for the conversation to show the same.
    shown: Vec<(String, String)>,
    /// A note in place of the bar, like "press ctrl-c again to leave".
    notice: Option<(String, Instant)>,
    /// The first of your message's lines in view, when it doesn't all fit.
    scroll: usize,
    /// Which of the matching commands is picked.
    picked: usize,
    /// When the last key came.
    last_key: Instant,
    /// Every step's full output, for the viewer (Ctrl-O).
    outputs: Vec<Entry>,
    viewer: Option<Viewer>,
    bar: String,
    replies: tokio::sync::mpsc::UnboundedSender<Read>,
    interrupted: Arc<tokio::sync::Notify>,
}

fn locked(boxed: &Mutex<Boxed>) -> MutexGuard<'_, Boxed> {
    boxed.lock().unwrap_or_else(|p| p.into_inner())
}

impl Boxed {
    /// The viewer, if it's open, or the box, at the window's new size.
    fn redraw(&mut self) {
        match &mut self.viewer {
            Some(viewer) => {
                let (rows, cols) = screen::size();
                let shown = viewer.render(rows, cols, Style::stderr());
                self.pinned.show_viewer(&shown);
            }
            None => self.draw(),
        }
    }

    /// The commands that match what's typed, while a command's name is
    /// being typed.
    fn menu(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        if self.waiting == Waiting::Answer {
            return Vec::new();
        }
        commands::matching(&self.editor.text())
    }

    /// Picks the command named exactly, or else the first.
    fn pick_default(&mut self) {
        let typed = self.editor.text();
        self.picked = self.menu().iter().position(|(name, _, _)| *name == typed).unwrap_or(0);
    }

    fn draw(&mut self) {
        if self.viewer.is_some() {
            return;
        }
        let s = Style::stderr();
        let pad = " ".repeat(GUTTER);
        let mut above = Vec::new();
        if let Some((what, since)) = &self.busy {
            let elapsed = since.elapsed();
            let frame = SPINNER[(elapsed.as_millis() / 100) as usize % SPINNER.len()];
            let hint = format!("({} · esc to stop)", seconds(elapsed));
            // A long command is cut, so the time and the hint stay in view.
            let room = screen::size().1.saturating_sub(GUTTER + 6 + width(&hint));
            let what = fit(what, room.max(10));
            above.push(format!("{pad}{} {what}… {}", s.yellow(&frame.to_string()), s.dim(&hint)));
        }
        for message in &self.queued {
            let first = message.lines().next().unwrap_or_default();
            above.push(format!("{pad}{} {first}", s.dim("↳ queued:")));
        }

        let bottom = match &self.asking {
            Some(asking) if self.waiting == Waiting::Answer => {
                let mut lines = vec![s.bold(&asking.question.title)];
                if let Some(command) = &asking.question.command {
                    // Wrapped at the box's width now, which a resize changes.
                    let mut shown = command_lines(command, s, self.pinned.text_width());
                    if shown.len() > COMMAND_LINES {
                        let more = shown.len() - (COMMAND_LINES - 1);
                        shown.truncate(COMMAND_LINES - 1);
                        shown.push(s.dim(&format!("  … {more} more lines, shown above")));
                    }
                    lines.extend(shown);
                    lines.push(String::new());
                }
                for (i, (key, text)) in asking.question.answers.iter().enumerate() {
                    let shortcut = s.dim(&format!("({key})"));
                    lines.push(if i == asking.picked {
                        format!("  {} {} {shortcut}", s.yellow("›"), s.bold(text))
                    } else {
                        format!("    {text} {shortcut}")
                    });
                }
                Bottom {
                    above,
                    tone: Tone::Asking,
                    mark: s.bold(&s.yellow("?")),
                    lines,
                    caret: None,
                    notes: (String::new(), String::new()),
                    below: vec![self.bar.clone()],
                    compact: {
                        let keys: Vec<&str> = asking.question.answers.iter().map(|(key, _)| *key).collect();
                        format!("{} {}", asking.question.title, keys.join("/"))
                    },
                }
            }
            _ => self.composer(above),
        };
        self.pinned.draw(&bottom);
    }

    /// The box with your message in it.
    fn composer(&mut self, above: Vec<String>) -> Bottom {
        let s = Style::stderr();
        let (mut lines, (line, column)) = self.editor.layout(self.pinned.text_width());
        // Keep the line with the cursor in view.
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + BOX_LINES {
            self.scroll = line + 1 - BOX_LINES;
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(BOX_LINES));
        let hidden_below = lines.len().saturating_sub(self.scroll + BOX_LINES);
        let notes = (more(self.scroll, "↑"), more(hidden_below, "↓"));
        lines = lines.into_iter().skip(self.scroll).take(BOX_LINES).collect();
        let tone = match self.waiting {
            Waiting::Message => Tone::Typing,
            Waiting::Answer => Tone::Asking,
            Waiting::Nothing => Tone::Busy,
        };
        if self.editor.is_empty() {
            let placeholder = match self.waiting {
                Waiting::Message => "type a message · / for commands · shift-enter or ctrl-j for a new line",
                Waiting::Answer => self.prompt.as_str(),
                Waiting::Nothing => "type your next message · enter queues it for when rein is done",
            };
            lines = vec![s.dim(placeholder)];
        }
        let mark = match (self.scroll, tone) {
            (1.., _) => " ".to_owned(),
            (_, Tone::Typing) => s.bold(&s.blue("›")),
            (_, Tone::Asking) => s.bold(&s.yellow("?")),
            (_, Tone::Busy) => s.dim("›"),
        };
        Bottom { above, tone, mark, lines, caret: Some((line - self.scroll, column)), notes, below: self.below(), compact: String::new() }
    }

    /// Under the box: a note, the commands that match, or the bar.
    fn below(&self) -> Vec<String> {
        let s = Style::stderr();
        if let Some((note, _)) = &self.notice {
            return vec![format!(" {}", s.yellow(note))];
        }
        let menu = self.menu();
        if menu.is_empty() {
            return vec![self.bar.clone()];
        }
        let names = menu.iter().map(|(name, args, _)| width(name) + 1 + width(args)).max().unwrap_or(0);
        let first = (self.picked + 1).saturating_sub(MENU_LINES);
        let mut lines: Vec<String> = menu
            .iter()
            .enumerate()
            .skip(first)
            .take(MENU_LINES)
            .map(|(i, (name, args, what))| {
                let usage = format!("{name} {args}");
                let pad = " ".repeat(names.saturating_sub(width(&usage)));
                if i == self.picked {
                    format!(" {} {}{}{pad}  {what}", s.blue("›"), s.bold(&s.blue(name)), s.dim(&format!(" {args}")))
                } else {
                    format!("   {name}{}{pad}  {}", s.dim(&format!(" {args}")), s.dim(what))
                }
            })
            .collect();
        let hidden = menu.len().saturating_sub(first + MENU_LINES);
        if hidden > 0 {
            lines.push(s.dim(&format!("   … {hidden} more")));
        }
        lines
    }

    fn key(&mut self, key: Key) {
        // The wheel scrolls the viewer's text, when it's open.
        let key = match key {
            Key::Mouse { button, .. } if self.viewer.is_some() && button & 64 != 0 => {
                if button & 1 == 0 { Key::Up } else { Key::Down }
            }
            Key::Mouse { .. } if self.viewer.is_some() => return,
            key => key,
        };
        if let Some(viewer) = &mut self.viewer {
            let (rows, cols) = screen::size();
            if viewer.key(&key) {
                let shown = viewer.render(rows, cols, Style::stderr());
                self.pinned.show_viewer(&shown);
            } else {
                self.viewer = None;
                self.pinned.close_viewer();
                self.draw();
            }
            return;
        }
        if key == Key::CtrlO && !self.outputs.is_empty() {
            let mut viewer = Viewer::new(self.outputs.clone());
            let (rows, cols) = screen::size();
            let shown = viewer.render(rows, cols, Style::stderr());
            self.pinned.open_viewer(&shown);
            self.viewer = Some(viewer);
            return;
        }
        // The conversation scrolls; a drag selects and copies.
        match key {
            Key::Mouse { button, x, y, up } => {
                self.mouse(button, x, y, up);
                return;
            }
            Key::PageUp | Key::PageDown => {
                screen::scroll(if key == Key::PageUp { -screen::page() } else { screen::page() });
                return;
            }
            Key::ScrollUp | Key::ScrollDown => {
                screen::scroll(if key == Key::ScrollUp { -1 } else { 1 });
                return;
            }
            _ => {}
        }
        let paused = std::mem::replace(&mut self.last_key, Instant::now()).elapsed() >= GRACE;
        if key == Key::Redraw {
            // Whatever state the screen is in, print it all again.
            self.pinned.repaint();
            self.redraw();
            return;
        }
        let answering = self.waiting == Waiting::Answer
            && self.asking.as_ref().is_some_and(|asking| (paused && asking.since.elapsed() >= GRACE) || matches!(key, Key::Esc | Key::CtrlC));
        if answering {
            self.answer(&key);
            self.draw();
            return;
        }
        // A question too new to take keys: they go to the box, as typing ahead.
        let waiting = if self.waiting == Waiting::Answer && self.asking.is_some() { Waiting::Nothing } else { self.waiting };
        let menu = self.menu();
        let mut edited = true;
        match (waiting, &key) {
            (Waiting::Nothing, Key::CtrlC | Key::Esc) => {
                // What was queued comes back to the box, to change or send.
                if !self.queued.is_empty() {
                    let mut text = std::mem::take(&mut self.queued).join("\n");
                    if !self.editor.is_empty() {
                        text = format!("{text}\n{}", self.editor.text());
                    }
                    self.editor.replace(&text);
                }
                // Only while a turn runs: a stop kept for later would end
                // the next turn as soon as it started.
                if self.busy.is_some() {
                    self.interrupted.notify_one();
                }
            }
            // Ctrl-C clears what you typed; on an empty box it's for leaving.
            (Waiting::Message, Key::CtrlC) if !self.editor.is_empty() => {
                self.editor.take();
            }
            (_, Key::CtrlC) => self.reply(Read::Interrupted),
            (Waiting::Message, Key::CtrlD) if self.editor.is_empty() => self.reply(Read::Eof),
            (Waiting::Message, Key::BackTab) if menu.is_empty() => self.reply(Read::NextMode),
            // The arrows pick among the matching commands.
            (_, Key::Up | Key::BackTab) if !menu.is_empty() => {
                self.picked = (self.picked + menu.len() - 1) % menu.len();
                edited = false;
            }
            (_, Key::Down) if !menu.is_empty() => {
                self.picked = (self.picked + 1) % menu.len();
                edited = false;
            }
            // Tab completes the picked command's name.
            (_, Key::Tab) if !menu.is_empty() => {
                let (name, args, _) = menu[self.picked.min(menu.len() - 1)];
                self.editor.replace(&format!("{name}{}", if args.is_empty() { "" } else { " " }));
            }
            // Enter sends the picked command.
            (_, Key::Enter) if !menu.is_empty() => {
                let (name, _, _) = menu[self.picked.min(menu.len() - 1)];
                self.editor.replace(name);
                self.submit(waiting);
            }
            _ => {
                if self.editor.key(&key) == Action::Submit {
                    self.submit(waiting);
                }
            }
        }
        if edited {
            self.pick_default();
            self.notice = None;
        }
        self.draw();
    }

    /// What the mouse did: the wheel scrolls three rows at a time; a press,
    /// drag and let go select text, which is copied.
    fn mouse(&mut self, button: u16, x: usize, y: usize, up: bool) {
        // Shift, Alt and Ctrl add 4, 8 and 16.
        let button = button & !(4 | 8 | 16);
        match (button, up) {
            (64, _) => screen::scroll(-3),
            (65, _) => screen::scroll(3),
            (0, false) => drop(screen::select(x, y, Drag::Start)),
            (32, false) => drop(screen::select(x, y, Drag::Move)),
            (0, true) => {
                if let Some(text) = screen::select(x, y, Drag::End) {
                    let lines = text.lines().count();
                    let what = if lines > 1 { format!("{lines} lines") } else { format!("{} characters", text.chars().count()) };
                    self.notice = Some((if copy(&text) { format!("copied {what}") } else { "couldn't copy: no clipboard".into() }, Instant::now()));
                    self.draw();
                }
            }
            _ => {}
        }
    }

    /// Enter: the message goes, or, while rein works, waits its turn.
    fn submit(&mut self, waiting: Waiting) {
        screen::follow();
        let shown = self.editor.shown_text();
        let text = self.editor.take();
        if shown != text {
            self.shown.push((text.clone(), shown));
        }
        if waiting != Waiting::Answer {
            self.editor.remember(&text);
        }
        match waiting {
            Waiting::Nothing if !text.trim().is_empty() => self.queued.push(text),
            Waiting::Nothing => {}
            _ => self.reply(Read::Line(text)),
        }
    }

    /// A key while a question waits for its answer.
    fn answer(&mut self, key: &Key) {
        let Some(asking) = &mut self.asking else { return };
        let count = asking.question.answers.len();
        let pick = |key: &str| asking.question.answers.iter().position(|(k, _)| *k == key);
        let chosen = match key {
            Key::Up | Key::Left | Key::BackTab => {
                asking.picked = (asking.picked + count - 1) % count;
                None
            }
            Key::Down | Key::Right | Key::Tab => {
                asking.picked = (asking.picked + 1) % count;
                None
            }
            Key::Enter => Some(asking.picked),
            Key::Esc | Key::CtrlC => pick("esc").or(pick("n")),
            Key::Char(c) => match c.to_digit(10) {
                Some(n @ 1..) if (n as usize) <= count => Some(n as usize - 1),
                _ => pick(&c.to_lowercase().to_string()),
            },
            _ => None,
        };
        if let Some(i) = chosen {
            let key = asking.question.answers[i].0.to_owned();
            self.asking = None;
            self.reply(Read::Line(key));
        }
    }

    fn reply(&mut self, read: Read) {
        self.waiting = Waiting::Nothing;
        let _ = self.replies.send(read);
    }

    /// The clock's tick: the spinner moves, and old notes go.
    fn tick(&mut self) {
        let expired = self.notice.as_ref().is_some_and(|(_, since)| since.elapsed() > NOTICE);
        if expired {
            self.notice = None;
        }
        if expired || self.busy.is_some() {
            self.draw();
        }
    }
}

/// The most lines of a command a question shows; a longer one is shown
/// above the box too.
pub const COMMAND_LINES: usize = 8;

/// A command as a question shows it: whole, wrapped to `width`, after a
/// `$`. Never cut short at the side, as what isn't seen can't be judged.
pub fn command_lines(command: &str, s: Style, width: usize) -> Vec<String> {
    let room = width.saturating_sub(2).max(10);
    let mut lines = Vec::new();
    for line in command.trim_end().lines() {
        let mut wrap = Wrap::new(room, 0, 0);
        let wrapped = wrap.push(&line.replace('\t', "    ")) + &wrap.finish();
        // A word wider than the box is broken, not cut.
        lines.extend(wrapped.lines().flat_map(|part| hard_wrap(part, room)));
    }
    lines.iter().enumerate().map(|(i, line)| format!("{}{line}", if i == 0 { s.dim("$ ") } else { "  ".to_owned() })).collect()
}

/// `line` in pieces no wider than `width`.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut used = 0;
    for c in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width && used > 0 {
            pieces.push(String::new());
            used = 0;
        }
        pieces.last_mut().expect("never empty").push(c);
        used += w;
    }
    pieces
}

/// Puts `text` on the clipboard: with the system's own command where there
/// is one, and otherwise by asking the terminal (OSC 52), which works over
/// ssh too, in terminals that allow it.
fn copy(text: &str) -> bool {
    use std::io::Write as _;
    let commands: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "--clipboard", "--input"]]
    };
    let local = std::env::var_os("SSH_CONNECTION").is_none();
    for command in commands.iter().filter(|_| local) {
        let child = std::process::Command::new(command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
            if child.wait().is_ok_and(|status| status.success()) && written {
                return true;
            }
        }
    }
    screen::clipboard(&base64(text.as_bytes()));
    true
}

/// Standard base64, for OSC 52.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// `↑ 3 more lines`, or nothing for none.
fn more(count: usize, arrow: &str) -> String {
    match count {
        0 => String::new(),
        1 => format!("{arrow} 1 more line"),
        n => format!("{arrow} {n} more lines"),
    }
}

/// `4s`, `1m 05s`.
fn seconds(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

impl Input {
    /// `pinned` asks for the input box, which needs a terminal all round.
    pub fn start(pinned: bool, interrupted: Arc<tokio::sync::Notify>) -> Self {
        let (requests, asked) = std::sync::mpsc::channel::<String>();
        let (tx, replies) = tokio::sync::mpsc::unbounded_channel();
        let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
        if pinned && terminal && let Some(raw) = Raw::enter() {
            screen::restore_on_panic();
            let boxed = Arc::new(Mutex::new(Boxed {
                pinned: Pinned::start(Style::stderr()),
                editor: Editor::default(),
                waiting: Waiting::Nothing,
                prompt: String::new(),
                asking: None,
                busy: None,
                queued: Vec::new(),
                shown: Vec::new(),
                notice: None,
                scroll: 0,
                picked: 0,
                last_key: Instant::now(),
                outputs: Vec::new(),
                viewer: None,
                bar: String::new(),
                replies: tx,
                interrupted,
            }));
            // A window that changes size gets the box drawn again right
            // away, not only at the next key.
            if let Ok(mut resized) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                let boxed = boxed.clone();
                tokio::spawn(async move {
                    // Laid out again for each size the window goes through.
                    while resized.recv().await.is_some() {
                        let mut boxed = locked(&boxed);
                        boxed.pinned.resized();
                        boxed.redraw();
                    }
                });
            }
            let clock = Arc::downgrade(&boxed);
            std::thread::spawn(move || {
                while let Some(boxed) = clock.upgrade() {
                    locked(&boxed).tick();
                    drop(boxed);
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
            let keyboard = boxed.clone();
            std::thread::spawn(move || {
                let mut keys = Keys::default();
                let mut buffer = [0u8; 1024];
                let mut stdin = std::io::stdin();
                while let Ok(n @ 1..) = std::io::Read::read(&mut stdin, &mut buffer) {
                    for key in keys.feed(&buffer[..n]) {
                        locked(&keyboard).key(key);
                    }
                }
            });
            return Input { requests, replies, pending: false, boxed: Some(boxed), _raw: Some(raw) };
        }
        std::thread::spawn(move || {
            for prompt in asked {
                eprint!("{prompt}");
                let mut line = String::new();
                let read = match std::io::stdin().read_line(&mut line) {
                    Ok(0) | Err(_) => Read::Eof,
                    Ok(_) => Read::Line(line.trim_end_matches(['\n', '\r']).to_owned()),
                };
                if tx.send(read).is_err() {
                    break;
                }
            }
        });
        Input { requests, replies, pending: false, boxed: None, _raw: None }
    }

    pub fn pinned(&self) -> bool {
        self.boxed.is_some()
    }

    /// How wide a line in the box can be.
    pub fn text_width(&self) -> usize {
        self.boxed.as_ref().map_or(80, |boxed| locked(boxed).pinned.text_width())
    }

    fn with(&self, change: impl FnOnce(&mut Boxed)) {
        if let Some(boxed) = &self.boxed {
            let mut boxed = locked(boxed);
            change(&mut boxed);
            boxed.draw();
        }
    }

    /// Reads one line: a message (`message`, kept for ↑) or a typed answer
    /// to `prompt`. Plain lines show `prompt` first. A message queued while
    /// rein worked comes back at once.
    pub async fn read(&mut self, prompt: &str, message: bool) -> Read {
        if !self.pending {
            match &self.boxed {
                Some(boxed) => {
                    let mut boxed = locked(boxed);
                    if message && !boxed.queued.is_empty() {
                        let next = boxed.queued.remove(0);
                        boxed.draw();
                        return Read::Line(next);
                    }
                    boxed.waiting = if message { Waiting::Message } else { Waiting::Answer };
                    boxed.prompt = prompt.trim().to_owned();
                    boxed.pick_default();
                    boxed.draw();
                }
                None => {
                    if self.requests.send(prompt.to_owned()).is_err() {
                        return Read::Eof;
                    }
                }
            }
            self.pending = true;
        }
        let read = self.replies.recv().await.unwrap_or(Read::Eof);
        self.pending = false;
        read
    }

    /// Asks `question` in the box and waits for the key of the answer.
    pub async fn ask(&mut self, question: Question) -> Read {
        if !self.pending {
            self.with(|boxed| {
                boxed.waiting = Waiting::Answer;
                boxed.asking = Some(Asking { question, picked: 0, since: Instant::now() });
            });
            self.pending = true;
        }
        let read = self.replies.recv().await.unwrap_or(Read::Eof);
        self.pending = false;
        read
    }

    /// Says what rein is doing, over the box, with a spinner and the time
    /// since `since`; `None` once it's done.
    pub fn busy(&self, what: Option<String>, since: Instant) {
        self.with(|boxed| boxed.busy = what.map(|what| (what, since)));
    }

    /// A short note under the box for a moment, in place of the bar.
    pub fn notice(&self, note: &str) {
        self.with(|boxed| boxed.notice = Some((note.to_owned(), Instant::now())));
    }

    /// `message` as the box showed it: big pastes as their labels.
    pub fn shown(&self, message: &str) -> String {
        let Some(boxed) = &self.boxed else { return message.to_owned() };
        let mut boxed = locked(boxed);
        match boxed.shown.iter().position(|(full, _)| full.trim() == message.trim()) {
            Some(i) => boxed.shown.remove(i).1,
            None => message.to_owned(),
        }
    }

    /// Keeps a step's full output for the viewer.
    pub fn keep_output(&self, title: String, output: &str) {
        if let Some(boxed) = &self.boxed {
            locked(boxed).outputs.push(Entry::new(title, output));
        }
    }

    /// Shows `bar` under the box.
    pub fn set_bar(&self, bar: String) {
        self.with(|boxed| boxed.bar = bar);
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        if let Some(boxed) = &self.boxed {
            locked(boxed).pinned.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::style::strip;

    #[test]
    fn shows_a_command_whole() {
        let lines: Vec<String> = command_lines("echo one two three\nls /a/very/long/path/without/spaces", Style::plain(), 22).iter().map(|l| strip(l)).collect();
        assert_eq!(lines, ["$ echo one two three", "  ls", "  /a/very/long/path/wi", "  thout/spaces"]);
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("ü".as_bytes()), "w7w=");
    }

    #[test]
    fn counts_what_doesnt_fit() {
        assert_eq!(more(0, "↑"), "");
        assert_eq!(more(1, "↓"), "↓ 1 more line");
        assert_eq!(more(3, "↑"), "↑ 3 more lines");
        assert_eq!(seconds(Duration::from_secs(65)), "1m 05s");
    }
}
