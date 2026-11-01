//! The whole window, while a conversation runs: the conversation above,
//! and at the bottom, what rein is doing, the input box and the status bar.
//!
//! ```text
//! rein   Done. The empty list now says: …           ← the conversation,
//!                                                       laid out for the width
//!        ◐ Running cargo test… (4s · esc to stop)
//! ╭──────────────────────────────────────────╮
//! │ › Now add a test                          │      ← the live part
//! ╰──────────────────────────────────────────╯
//!  gpt-6.1-sol · ask · context ▰▱▱▱▱ 2% · …
//! ```
//!
//! It's the terminal's other screen, the one `less` and `vim` use, so
//! nothing rein draws gets into the terminal's scrollback, and what was
//! there before rein is left alone. rein keeps the conversation as text
//! ([`Transcript`]) and draws the window from it: after a resize it's laid
//! out again for the new width, so nothing is lost and words wrap where
//! they should. The mouse wheel scrolls it, and a drag selects text and
//! copies it. When the conversation ends it's printed into the normal
//! screen, so it stays in the scrollback.
//!
//! Without a terminal (`rein "task"`, or output to a file) it's printed as
//! it comes instead.
//!
//! Three things draw at once (the answer, the keyboard's thread and the
//! clock that moves the spinner), so every write takes [`lock`].

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::{LazyLock, Mutex, MutexGuard};

use super::style::{Style, fit, strip, width};
use super::transcript::{Draw, Row, Transcript, split_at_column};

static SCREEN: Mutex<()> = Mutex::new(());

/// Hold this while writing to the terminal.
pub fn lock() -> MutexGuard<'static, ()> {
    SCREEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the window shows, and what it's made from.
struct State {
    transcript: Transcript,
    /// The full screen is up.
    full: bool,
    /// The viewer covers it: nothing else draws.
    viewing: bool,
    /// The live part's lines, and where the caret goes in them.
    live: Frame,
    /// The first row of the conversation on screen when it's scrolled
    /// back; `None` follows the end.
    top: Option<usize>,
    /// Text being selected: where the drag started and where it is, as
    /// (row of the conversation, column).
    selection: Option<((usize, usize), (usize, usize))>,
    /// Where the conversation's rows are on screen: the first screen row
    /// they take, the first conversation row there, and how many.
    area: (usize, usize, usize),
    /// The rows on screen, to rewrite only those that changed.
    shown: Vec<String>,
    /// Every row needs drawing again: the size changed, or the viewer
    /// covered the screen.
    stale: bool,
    /// The transcript's length when the full screen came up: what was
    /// printed before is in the terminal's own scrollback already.
    since: usize,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        transcript: Transcript::default(),
        full: false,
        viewing: false,
        live: Frame { lines: Vec::new(), caret: None },
        top: None,
        selection: None,
        area: (0, 0, 0),
        shown: Vec::new(),
        stale: true,
        since: 0,
    })
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The viewer is open: nothing else may draw.
pub fn viewing() -> bool {
    state().viewing
}

/// Writes `text` to stderr in one piece, under the lock.
pub fn write(text: &str) {
    print(text, &mut std::io::stderr());
}

/// Writes `text` to stdout in one piece, under the lock.
pub fn write_out(text: &str) {
    print(text, &mut std::io::stdout());
}

fn print(text: &str, out: &mut impl Write) {
    let _guard = lock();
    let mut state = state();
    state.transcript.write(text);
    if state.full {
        render(&mut state);
        return;
    }
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Lines that lay themselves out for the window's width, like a step: on
/// the full screen, again at each new width; otherwise once, now.
pub fn show(draw: Draw) {
    let full = state().full;
    if !full {
        let lines = draw(size().1);
        write(&lines.iter().map(|line| format!("{line}\n")).collect::<String>());
        return;
    }
    let _guard = lock();
    let mut state = state();
    state.transcript.draw(draw);
    render(&mut state);
}

/// Scrolls the conversation by `rows`, back for negative.
pub fn scroll(rows: isize) {
    let _guard = lock();
    let mut state = state();
    if !state.full || state.viewing {
        return;
    }
    let total = {
        let (closed, open) = state.transcript.rows(size().1);
        closed.len() + open.len()
    };
    let end = total.saturating_sub(conversation_height(&state));
    let top = (state.top.unwrap_or(end) as isize + rows).clamp(0, end as isize) as usize;
    state.top = (top < end).then_some(top);
    render(&mut state);
}

/// A page: the conversation's height on screen, less a little to read on from.
pub fn page() -> isize {
    (conversation_height(&state()).saturating_sub(2)).max(1) as isize
}

/// Back to the end of the conversation.
pub fn follow() {
    let _guard = lock();
    let mut state = state();
    if state.top.take().is_some() && state.full {
        render(&mut state);
    }
}

/// Asks the terminal to put `encoded` (base64) on the clipboard: OSC 52.
pub fn clipboard(encoded: &str) {
    let _guard = lock();
    write_raw(&format!("\x1b]52;c;{encoded}\x07"));
}

/// What the mouse is doing to a selection.
pub enum Drag {
    Start,
    Move,
    End,
}

/// The mouse at screen `(x, y)`, counted from 1. The end of a drag gives
/// the selected text, to copy.
pub fn select(x: usize, y: usize, drag: Drag) -> Option<String> {
    let _guard = lock();
    let mut state = state();
    if !state.full || state.viewing {
        return None;
    }
    let (first, top, count) = state.area;
    let row = (y.saturating_sub(1)).clamp(first, (first + count).max(1) - 1) - first + top;
    let at = (row, x.saturating_sub(1));
    match drag {
        Drag::Start => {
            // Only in the conversation.
            let inside = y >= 1 && (first..first + count).contains(&(y - 1));
            state.selection = inside.then_some((at, at));
        }
        Drag::Move => {
            if let Some((from, _)) = state.selection {
                state.selection = Some((from, at));
            }
        }
        Drag::End => {
            let (from, to) = state.selection.take()?;
            let text = (from != to).then(|| selected_text(&mut state, from, to));
            render(&mut state);
            return text.filter(|text| !text.trim().is_empty());
        }
    }
    render(&mut state);
    None
}

/// The text between two points of the conversation, as it reads: a line
/// wrapped over rows comes out as one line again.
fn selected_text(state: &mut State, from: (usize, usize), to: (usize, usize)) -> String {
    let (from, to) = if from <= to { (from, to) } else { (to, from) };
    let (closed, open) = state.transcript.rows(size().1);
    let rows: Vec<&Row> = closed.iter().chain(open.iter()).collect();
    let mut out = String::new();
    for i in from.0..=to.0.min(rows.len().saturating_sub(1)) {
        let row = rows[i];
        let plain: Vec<char> = strip(&row.text).chars().collect();
        let start = if i == from.0 { from.1 } else { 0 };
        let end = if i == to.0 { (to.1 + 1).min(plain.len()) } else { plain.len() };
        let piece: String = plain.get(start.min(plain.len())..end.max(start).min(plain.len())).unwrap_or_default().iter().collect();
        if i > from.0 {
            if row.wrapped {
                out = out.trim_end().to_owned();
                out.push(' ');
                out += piece.trim_start();
                continue;
            }
            out.push('\n');
        }
        out += piece.trim_end();
    }
    out
}

fn conversation_height(state: &State) -> usize {
    let rows = size().0;
    let live = state.live.lines.len();
    rows.saturating_sub(live + usize::from(rows > live + 1))
}

/// Draws the window from the state: the rows that changed, or all of them.
fn render(state: &mut State) {
    if !state.full || state.viewing {
        return;
    }
    let (rows, cols) = size();
    let s = Style::stderr();
    let area = conversation_height(state);
    let gap = rows.saturating_sub(area + state.live.lines.len());
    let selection = state.selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
    let (closed, open) = state.transcript.rows(cols);
    let total = closed.len() + open.len();
    let end = total.saturating_sub(area);
    let top = state.top.map_or(end, |top| top.min(end));
    let count = total.min(area);
    let row_at = |i: usize| if i < closed.len() { &closed[i] } else { &open[i - closed.len()] };
    // Anchored to the bottom: a short conversation sits just over the box.
    let mut screen: Vec<String> = vec![String::new(); area - count];
    for i in top..top + count {
        let text = &row_at(i).text;
        screen.push(match selection {
            Some(((a, ac), (b, bc))) if (a..=b).contains(&i) => {
                let from = if i == a { ac } else { 0 };
                let to = if i == b { bc + 1 } else { cols };
                highlight(text, from, to)
            }
            _ => text.clone(),
        });
    }
    let below = total - (top + count);
    if below > 0 && count > 0 {
        let note = format!(" ↓ {below} more line{} · scroll or pgdn ", if below == 1 { "" } else { "s" });
        let last = screen.len() - 1;
        screen[last] = format!("{}{}", " ".repeat(cols.saturating_sub(width(&note) + 1)), s.yellow(&note));
    }
    state.area = (area - count, top, count);
    screen.extend(std::iter::repeat_n(String::new(), gap));
    let live_top = screen.len();
    screen.extend(state.live.lines.iter().cloned());
    screen.truncate(rows);

    let mut out = String::from("\x1b[?2026h\x1b[?7l");
    if std::mem::take(&mut state.stale) || state.shown.len() != screen.len() {
        out += "\x1b[0m\x1b[H\x1b[2J";
        state.shown.clear();
    }
    for (y, row) in screen.iter().enumerate() {
        if state.shown.get(y) != Some(row) {
            // Cleared first, then written: erasing after a row that fills
            // the width would take its last character with it.
            out += &format!("\x1b[{};1H\x1b[0m\x1b[2K{}\x1b[0m", y + 1, fit(row, cols));
        }
    }
    state.shown = screen;
    // The terminal's own cursor goes where you type, so it blinks there
    // and an input method's window opens there.
    match state.live.caret {
        Some((row, column, _)) if live_top + row < rows => out += &format!("\x1b[{};{column}H\x1b[?25h", live_top + row + 1),
        _ => out += "\x1b[?25l",
    }
    out += "\x1b[?7h\x1b[?2026l";
    write_raw(&out);
}

/// `text` with screen columns `from` to `to` in reverse, as a selection.
fn highlight(text: &str, from: usize, to: usize) -> String {
    let (left, rest) = split_at_column(text, from);
    let (middle, right) = split_at_column(&rest, to.saturating_sub(from));
    let middle = if strip(&middle).is_empty() { " ".to_owned() } else { strip(&middle) };
    format!("{left}\x1b[0m\x1b[7m{middle}\x1b[27m{right}")
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

/// What starts the full screen: the terminal's other screen, the mouse
/// reported (the wheel and drags, as `ESC [ < …`), pasted text marked, and
/// keys with Shift, Alt or Ctrl spelled out where the terminal knows how
/// (the "kitty keyboard protocol", so Shift-Enter isn't just Enter).
const START: &str = "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[>1u\x1b[?25l";
/// And what undoes it.
const END: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[<u\x1b[?7h\x1b[?25h\x1b[?1049l";

/// Makes a panic leave the terminal usable: normal keyboard, the normal
/// screen, the mouse let go, cursor shown.
pub fn restore_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(saved) = *RESTORE.lock().unwrap_or_else(|p| p.into_inner()) {
            // SAFETY: as in Raw::drop.
            unsafe { libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, &saved) };
            eprint!("{END}\r\n");
        }
        previous(info);
    }));
}

/// Whose turn it is, which the box's border says by its colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    /// Yours, to type a message: blue.
    Typing,
    /// rein's: grey, though you can type ahead.
    Busy,
    /// rein needs a decision from you: yellow.
    Asking,
}

/// What the bottom shows, from the top down.
pub struct Bottom {
    /// Lines over the box: what rein is doing, messages waiting to be sent.
    pub above: Vec<String>,
    pub tone: Tone,
    /// What starts the box's first line, like `›`.
    pub mark: String,
    /// The lines in the box, styled already, each as wide as
    /// [`Pinned::text_width`] at most.
    pub lines: Vec<String>,
    /// Where the caret is among `lines`, (line, column), if anywhere.
    pub caret: Option<(usize, usize)>,
    /// Short notes for the right end of the top and bottom borders, like
    /// `↑ 3 more lines`.
    pub notes: (String, String),
    /// Lines under the box: the status bar, or the commands that match.
    pub below: Vec<String>,
    /// All of it in one line, for a window too short for the box: a
    /// question, say. Empty means the line with the caret.
    pub compact: String,
}

/// The box and what's around it, on the full screen.
pub struct Pinned {
    style: Style,
}

impl Pinned {
    /// Puts the full screen up; [`Pinned::draw`] draws on it.
    pub fn start(style: Style) -> Pinned {
        let _guard = lock();
        let mut state = state();
        state.full = true;
        state.stale = true;
        state.since = state.transcript.mark();
        write_raw(START);
        Pinned { style }
    }

    /// The width text gets inside the box, at the window's size now.
    pub fn text_width(&self) -> usize {
        size().1.saturating_sub(6).max(10)
    }

    /// Covers the window with the viewer's `screen`. Until `close_viewer`,
    /// nothing else draws.
    pub fn open_viewer(&mut self, screen: &str) {
        let _guard = lock();
        state().viewing = true;
        write_raw(&format!("\x1b[?25l{screen}"));
    }

    /// Draws the viewer again.
    pub fn show_viewer(&self, screen: &str) {
        let _guard = lock();
        write_raw(screen);
    }

    /// Back to the conversation, with what was said meanwhile. The box
    /// comes with the next [`Pinned::draw`].
    pub fn close_viewer(&mut self) {
        let _guard = lock();
        let mut state = state();
        state.viewing = false;
        state.stale = true;
    }

    /// The window changed size: everything is laid out again at the next
    /// [`Pinned::draw`]. A selection's rows have moved, so it goes.
    pub fn resized(&mut self) {
        let mut state = state();
        state.stale = true;
        state.selection = None;
    }

    /// Ctrl-L: every row drawn again at the next [`Pinned::draw`].
    pub fn repaint(&mut self) {
        state().stale = true;
    }

    /// Draws the bottom, and the conversation over it. Takes the lock itself.
    pub fn draw(&mut self, bottom: &Bottom) {
        let _guard = lock();
        let mut state = state();
        state.live = self.layout(bottom);
        render(&mut state);
    }

    /// The frame's lines, fitted to the window.
    fn layout(&self, bottom: &Bottom) -> Frame {
        let (rows, cols) = size();
        let s = self.style;
        let texts: Vec<&str> = if bottom.lines.is_empty() { vec![""] } else { bottom.lines.iter().map(String::as_str).collect() };
        // Never taller than the window, less the cursor's line and a gap:
        // a taller frame would scroll its top into the scrollback, to come
        // back as litter when the window grows.
        let most = rows.saturating_sub(2);
        if texts.len() + 2 > most {
            return self.squeezed(bottom, &texts, most, cols);
        }
        let paint = |text: &str| match bottom.tone {
            Tone::Typing => s.blue(text),
            Tone::Busy => s.dim(text),
            Tone::Asking => s.yellow(text),
        };
        let inner = cols.saturating_sub(4);
        let room = inner.saturating_sub(2);
        let border = |left: char, right: char, note: &str| {
            let note = if note.is_empty() { String::new() } else { format!(" {} ", fit(note, room)) };
            let fill = (inner + 2).saturating_sub(width(&note) + 1);
            format!("{}{}{}", paint(&format!("{left}{}", "─".repeat(fill))), s.dim(&note), paint(&format!("─{right}")))
        };
        // What's above the box goes first, then what's below.
        let spare = most - (texts.len() + 2);
        let above = &bottom.above[bottom.above.len().saturating_sub(spare)..];
        let below = &bottom.below[..bottom.below.len().min(spare - above.len())];

        let mut lines: Vec<String> = above.iter().map(|line| fit(line, cols)).collect();
        lines.push(border('╭', '╮', &bottom.notes.0));
        let top = lines.len();
        let mut caret = None;
        for (i, text) in texts.iter().enumerate() {
            let mark = if i == 0 { bottom.mark.as_str() } else { " " };
            let text = fit(text, room);
            if let Some((line, column)) = bottom.caret
                && line == i
            {
                // The box's text starts in the fifth column: `│ › `.
                caret = Some((top + i, column + 5, char_at(&text, column)));
            }
            let pad = room.saturating_sub(width(&text));
            lines.push(format!("{} {mark} {text}{} {}", paint("│"), " ".repeat(pad), paint("│")));
        }
        lines.push(border('╰', '╯', &bottom.notes.1));
        lines.extend(below.iter().map(|line| fit(line, cols)));
        Frame { lines, caret }
    }

    /// The frame in a window too short for the box: one line without
    /// borders, or, with no room even for that, nothing.
    fn squeezed(&self, bottom: &Bottom, texts: &[&str], most: usize, cols: usize) -> Frame {
        if most == 0 {
            return Frame { lines: Vec::new(), caret: None };
        }
        let room = cols.saturating_sub(3);
        if !bottom.compact.is_empty() {
            return Frame { lines: vec![format!("{} {}", bottom.mark, fit(&bottom.compact, room))], caret: None };
        }
        let (line, column) = bottom.caret.unwrap_or((0, 0));
        let text = fit(texts.get(line).copied().unwrap_or_default(), room);
        let mark = if line == 0 { bottom.mark.as_str() } else { " " };
        let caret = bottom.caret.map(|_| (0, column + 3, char_at(&text, column)));
        Frame { lines: vec![format!("{mark} {text}")], caret }
    }

    /// Puts the terminal back, and prints the conversation into the
    /// normal screen, so it's still there in the scrollback.
    pub fn stop(&mut self) {
        let _guard = lock();
        let mut state = state();
        if !std::mem::take(&mut state.full) {
            return;
        }
        let said = state.transcript.since(state.since, size().1);
        write_raw(&format!("{END}{said}"));
    }
}

/// The live part as laid out: its lines, and where the caret goes: which
/// line, the screen column (from 1), and the character under it.
struct Frame {
    lines: Vec<String>,
    caret: Option<(usize, usize, char)>,
}

/// The character `text` shows at screen column `at` (from 0), or a space
/// past its end.
fn char_at(text: &str, at: usize) -> char {
    let mut used = 0;
    for c in strip(text).chars() {
        used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used > at {
            return c;
        }
    }
    ' '
}

/// Writes one frame. Terminals that know synchronized output (mode 2026)
/// show it all at once, never half drawn; others ignore the markers.
fn write_raw(text: &str) {
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(format!("\x1b[?2026h{text}\x1b[?2026l").as_bytes());
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bottom(lines: &[&str]) -> Bottom {
        Bottom {
            above: vec!["working".into()],
            tone: Tone::Typing,
            mark: "›".into(),
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            caret: Some((0, 2)),
            notes: (String::new(), "↓ 2 more lines".into()),
            below: vec!["bar".into()],
            compact: String::new(),
        }
    }

    #[test]
    fn lays_out_the_box_with_what_goes_around_it() {
        let frame = Pinned { style: Style::plain() }.layout(&bottom(&["hello"]));
        let cols = size().1;
        assert_eq!(frame.lines.len(), 5);
        assert_eq!(frame.lines[0], "working");
        assert!(frame.lines[2].starts_with("│ › hello "), "{}", frame.lines[2]);
        assert!(frame.lines[3].ends_with(" ↓ 2 more lines ─╯"), "{}", frame.lines[3]);
        assert!(frame.lines[1..4].iter().all(|line| width(line) == cols), "every border line fills the width");
        assert_eq!(frame.caret, Some((2, 7, 'l')), "the caret is on the third character");
    }

    #[test]
    fn never_taller_than_the_window() {
        let pinned = Pinned { style: Style::plain() };
        let (rows, cols) = size();
        // As many lines as the window has: no room for the box.
        let lines: Vec<String> = (0..rows).map(|i| format!("line {i}")).collect();
        let tall = Bottom { lines, caret: Some((rows - 1, 1)), ..bottom(&[]) };
        let frame = pinned.squeezed(&tall, &tall.lines.iter().map(String::as_str).collect::<Vec<_>>(), 3, cols);
        assert_eq!(frame.lines, [format!("  line {}", rows - 1)], "only the line with the caret, without borders");
        assert_eq!(frame.caret.map(|(row, column, _)| (row, column)), Some((0, 4)));
        assert!(pinned.layout(&tall).lines.len() + 1 < rows);
        // A question says it all in one line.
        let asked = Bottom { compact: "Allow this? y/a/n/esc".into(), ..bottom(&["Allow this?", "Yes", "No"]) };
        assert_eq!(pinned.squeezed(&asked, &[], 1, cols).lines, ["› Allow this? y/a/n/esc"]);
        assert!(pinned.squeezed(&asked, &[], 0, cols).lines.is_empty(), "no room at all: nothing");
    }
}
