//! The bottom of the terminal, kept still while the conversation scrolls
//! above it: the input box and the status bar.
//!
//! ```text
//! rein   Done. The empty list now says: …          ← scrolls
//! ╭──────────────────────────────────────────╮
//! │ › Now add a test                          │      ← pinned
//! ╰──────────────────────────────────────────╯
//!  gpt-6.1-sol · ask · context ▰▱▱▱▱ 2% · …
//! ```
//!
//! It isn't full screen: the terminal is told that only the lines above
//! the box scroll (a "scroll region"), so the conversation goes into the
//! normal scrollback and can be selected and copied as usual. The box and
//! the bar are drawn into the lines below with absolute cursor moves.
//!
//! Three things write to the screen at once (the answer, the spinner's
//! thread and the keyboard's thread), so every write takes [`lock`].

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::{Mutex, MutexGuard};

use super::style::{Style, fit, strip, width};

static SCREEN: Mutex<()> = Mutex::new(());

/// Hold this while writing to the terminal.
pub fn lock() -> MutexGuard<'static, ()> {
    SCREEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// While the viewer is open, what rein prints waits here, to be shown
/// when it closes.
static HELD: Mutex<Option<String>> = Mutex::new(None);

fn held() -> MutexGuard<'static, Option<String>> {
    HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The viewer is open: nothing else may draw.
pub fn viewing() -> bool {
    held().is_some()
}

/// Writes `text` to stderr in one piece, under the lock.
pub fn write(text: &str) {
    let _guard = lock();
    if let Some(held) = held().as_mut() {
        held.push_str(text);
        return;
    }
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

/// Writes `text` to stdout in one piece, under the lock.
pub fn write_out(text: &str) {
    let _guard = lock();
    if let Some(held) = held().as_mut() {
        held.push_str(text);
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// Rows and columns of the terminal on stderr.
pub fn size() -> (usize, usize) {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a `winsize` into the struct we pass.
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_row > 0 && size.ws_col > 0 { (size.ws_row as usize, size.ws_col as usize) } else { (24, 80) }
}

/// The keyboard in raw mode: every key arrives as it's pressed, nothing is
/// echoed, and Ctrl-C is a key rather than a signal. Output still turns
/// `\n` into a new line. Dropping it puts the terminal back.
pub struct Raw {
    saved: libc::termios,
}

impl Raw {
    pub fn enter() -> Option<Raw> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: tcgetattr fills the termios we pass; an all-zero one is valid.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: as above; the struct came from tcgetattr.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        RESTORE.lock().unwrap_or_else(|p| p.into_inner()).replace(saved);
        Some(Raw { saved })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: puts back the settings tcgetattr gave us.
        unsafe { libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, &self.saved) };
        RESTORE.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

/// The settings to put back if rein panics, as Drop doesn't run then.
static RESTORE: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Makes a panic leave the terminal usable: normal keyboard, no scroll
/// region, cursor shown.
pub fn restore_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(saved) = *RESTORE.lock().unwrap_or_else(|p| p.into_inner()) {
            // SAFETY: as in Raw::drop.
            unsafe { libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, &saved) };
            eprint!("\x1b[r\x1b[?25h\x1b[?2004l\r\n");
        }
        previous(info);
    }));
}

/// Asks the terminal which row the cursor is on. Needs raw mode, and
/// nothing else reading the keyboard yet.
pub fn cursor_row() -> Option<usize> {
    // Straight from the file descriptor: Rust's own stdin buffers, and
    // would swallow the rest of the reply where poll can't see it.
    let fd = std::io::stdin().as_raw_fd();
    eprint!("\x1b[6n");
    std::io::stderr().flush().ok()?;
    let mut reply = Vec::new();
    let mut fds = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    while !reply.ends_with(b"R") && reply.len() < 32 {
        // SAFETY: poll reads one pollfd we own, waiting at most a second:
        // some terminals answer slowly.
        if unsafe { libc::poll(&mut fds, 1, 1000) } <= 0 {
            return None;
        }
        let mut byte = 0u8;
        // SAFETY: reads at most one byte into `byte`.
        if unsafe { libc::read(fd, (&raw mut byte).cast(), 1) } != 1 {
            return None;
        }
        reply.push(byte);
    }
    // The answer is `ESC [ row ; column R`.
    let text = String::from_utf8_lossy(&reply);
    let row = text.rsplit('[').next()?.split(';').next()?;
    row.parse().ok()
}

/// What the bottom shows.
pub struct Bottom {
    /// The box takes keys right now, so its border is blue.
    pub focused: bool,
    /// The text lines in the box, and where the cursor is among them.
    pub lines: Vec<String>,
    pub cursor: (usize, usize),
    /// Shown in grey when the box is empty.
    pub placeholder: String,
    pub bar: String,
}

/// The pinned bottom area.
pub struct Pinned {
    style: Style,
    rows: usize,
    cols: usize,
    /// Lines the bottom takes: the box (two borders plus its text lines)
    /// and the bar.
    height: usize,
    /// The cursor sits in the box rather than in the conversation.
    in_box: bool,
    /// The terminal says where the cursor is when asked (`ESC [ 6n`).
    answers: bool,
    /// Asked, after a resize, and waiting for the answer.
    asked: bool,
    /// Where the cursor sits in the box: which of its text lines, and the
    /// screen column.
    box_cursor: (usize, usize),
}

/// The most text lines the box grows to.
const MAX_LINES: usize = 8;

impl Pinned {
    /// Starts keeping the bottom still, below where the cursor is now.
    pub fn start(style: Style) -> Pinned {
        let (rows, cols) = size();
        let row = cursor_row();
        let mut pinned = Pinned { style, rows, cols, height: 4, in_box: false, answers: row.is_some(), asked: false, box_cursor: (0, 1) };
        let _guard = lock();
        let mut out = String::from("\x1b[?2004h"); // pasted text comes marked
        out += &pinned.make_room(row.unwrap_or(rows), 1);
        write_raw(&out);
        pinned.in_box = false;
        pinned
    }

    /// The width text gets inside the box, at the window's size now.
    pub fn text_width(&self) -> usize {
        size().1.saturating_sub(6).max(10)
    }

    /// Sets the scroll region for the current height, scrolling the
    /// conversation up if the cursor (on `row`) would end up under the box.
    fn make_room(&self, row: usize, column: usize) -> String {
        let bottom = self.rows.saturating_sub(self.height).max(1);
        let mut out = String::new();
        let row = if row > bottom {
            // Scroll the screen up so the cursor's line sits just above the box.
            out += &format!("\x1b[{};1H", self.rows);
            out += &"\n".repeat(row - bottom);
            bottom
        } else {
            row
        };
        // Setting the region moves the cursor to the top; put it back.
        out += &format!("\x1b[1;{bottom}r\x1b[{row};{column}H");
        out
    }

    /// The terminal's answer after a resize: the cursor is on `row`, at
    /// `column`. Everything from the old box down goes, and the
    /// conversation goes on from where it was.
    pub fn cursor_at(&mut self, row: usize, column: usize) {
        if !std::mem::take(&mut self.asked) {
            return;
        }
        let _guard = lock();
        let (rows, cols) = size();
        let (clear, row, column) = if self.in_box {
            // The cursor is in the box. A narrower terminal rewrapped each
            // of the box's lines, all as wide as the old window, onto
            // `wrapped` rows, so its top is that much further up.
            let (line, at) = self.box_cursor;
            let wrapped = self.cols.div_ceil(cols.max(1)).max(1);
            let top = row.saturating_sub((at - 1) / cols.max(1) + (line + 1) * wrapped).max(1);
            // The conversation's cursor waits at the start of the line
            // just above the box.
            (top, top.saturating_sub(1).max(1), 1)
        } else {
            // Text is coming in: the old box is everything below it.
            (row, row, column)
        };
        (self.rows, self.cols) = (rows, cols);
        self.in_box = false;
        let at = if clear == row { format!("\x1b[{row};{column}H") } else { format!("\x1b[{clear};1H") };
        write_raw(&format!("\x1b[r{at}\x1b[J{}", self.make_room(row, column)));
    }

    /// Opens the viewer's screen (the terminal's alternate screen, as `less`
    /// uses) and draws `screen` on it. From now until `close_viewer`,
    /// whatever rein prints is held back.
    pub fn open_viewer(&mut self, screen: &str) {
        let _guard = lock();
        let mut out = String::new();
        if self.in_box {
            // Back to the conversation first: the alternate screen keeps
            // the cursor, and the box's isn't the one to come back to.
            out += "\x1b8";
            self.in_box = false;
        }
        out += "\x1b[?1049h";
        out += screen;
        write_raw(&out);
        held().replace(String::new());
    }

    /// Draws the viewer again.
    pub fn show_viewer(&self, screen: &str) {
        let _guard = lock();
        write_raw(screen);
    }

    /// Back to the conversation, with what was printed meanwhile.
    pub fn close_viewer(&mut self) {
        let _guard = lock();
        let waiting = held().take().unwrap_or_default();
        write_raw(&format!("\x1b[?1049l{waiting}"));
    }

    /// Draws the bottom: the box, then the bar. Takes the lock itself.
    pub fn draw(&mut self, bottom: &Bottom) {
        let _guard = lock();
        if viewing() || self.asked {
            return;
        }
        let mut out = String::new();
        let (rows, cols) = size();
        let lines = bottom.lines.len().clamp(1, MAX_LINES);
        let height = lines + 3;
        let resized = (rows, cols) != (self.rows, self.cols);
        if resized && self.answers {
            // Ask where the terminal moved the cursor when it rewrapped
            // the screen; `cursor_at` goes on from there.
            self.asked = true;
            write_raw("\x1b[6n");
            return;
        }
        if height != self.height || resized {
            // Back to the conversation's cursor, then make the new room.
            if self.in_box {
                out += "\x1b8";
                self.in_box = false;
            }
            let grow = height.saturating_sub(self.height);
            if resized {
                // The terminal rewrapped everything: clear where the old
                // bottom was, and start over at the bottom.
                let old_top = (self.rows + 1).saturating_sub(self.height).min(rows);
                let new_top = (rows + 1).saturating_sub(height);
                self.rows = rows;
                self.cols = cols;
                self.height = height;
                out += &format!("\x1b[r\x1b[{};1H\x1b[J{}", old_top.min(new_top).max(1), self.make_room(rows, 1));
            } else if grow > 0 {
                // Scroll the conversation up out of the way, keeping the
                // cursor on its text.
                let bottom = self.rows - self.height;
                out += &format!("\x1b7\x1b[{bottom};1H{}\x1b8\x1b[{grow}A", "\n".repeat(grow));
                self.height = height;
                out += &format!("\x1b7\x1b[1;{}r\x1b8", self.rows - self.height);
            } else {
                // Shrinking: blank the lines the box no longer uses, and let
                // the region grow down over them.
                let old_top = self.rows - self.height + 1;
                self.height = height;
                let new_top = self.rows - self.height + 1;
                out += "\x1b7";
                for row in old_top..new_top {
                    out += &format!("\x1b[{row};1H\x1b[2K");
                }
                out += &format!("\x1b[1;{}r\x1b8", self.rows - self.height);
            }
        }
        if !self.in_box {
            out += "\x1b7"; // remember the conversation's cursor
        }
        out += "\x1b[?25l";
        let s = self.style;
        let border = |text: &str| if bottom.focused { s.blue(text) } else { s.dim(text) };
        let inner = self.cols.saturating_sub(4);
        let top = self.rows - self.height + 1;
        out += &format!("\x1b[{top};1H\x1b[2K{}", border(&format!("╭{}╮", "─".repeat(inner + 2))));
        let skip = bottom.lines.len().saturating_sub(MAX_LINES);
        let shown = &bottom.lines[skip..];
        for i in 0..lines {
            let mark = if i == 0 { if bottom.focused { s.bold(&s.blue("›")) } else { s.dim("›") } } else { " ".into() };
            let text = match shown.get(i) {
                Some(line) if !(shown.len() == 1 && line.is_empty()) => line.clone(),
                _ if i == 0 => s.dim(&bottom.placeholder),
                _ => String::new(),
            };
            let room = inner.saturating_sub(2);
            let text = if width(&text) > room { strip(&text).chars().take(room).collect() } else { text };
            let pad = room.saturating_sub(width(&text));
            out += &format!("\x1b[{};1H\x1b[2K{} {mark} {text}{} {}", top + 1 + i, border("│"), " ".repeat(pad), border("│"));
        }
        out += &format!("\x1b[{};1H\x1b[2K{}", top + 1 + lines, border(&format!("╰{}╯", "─".repeat(inner + 2))));
        // Cut to fit: a bar that wrapped would leave its end on screen.
        out += &format!("\x1b[{};1H\x1b[2K{}", self.rows, fit(&bottom.bar, self.cols.saturating_sub(1)));
        if bottom.focused {
            let (line, column) = bottom.cursor;
            let line = line.saturating_sub(skip).min(lines - 1);
            out += &format!("\x1b[{};{}H\x1b[?25h", top + 1 + line, column + 5);
            self.box_cursor = (line, column + 5);
            self.in_box = true;
        } else {
            out += "\x1b8";
            self.in_box = false;
        }
        write_raw(&out);
    }

    /// Puts the terminal back: no region, the bottom cleared, cursor shown.
    pub fn stop(&mut self) {
        let _guard = lock();
        let top = self.rows - self.height + 1;
        let mut out = String::new();
        if self.in_box {
            out += "\x1b8";
        }
        out += &format!("\x1b7\x1b[r\x1b[{top};1H\x1b[J\x1b8\x1b[?25h\x1b[?2004l");
        write_raw(&out);
        self.in_box = false;
    }
}

/// Writes one frame. Terminals that know synchronized output (mode 2026)
/// show it all at once, never half drawn; others ignore the markers.
fn write_raw(text: &str) {
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(format!("\x1b[?2026h{text}\x1b[?2026l").as_bytes());
    let _ = stderr.flush();
}
