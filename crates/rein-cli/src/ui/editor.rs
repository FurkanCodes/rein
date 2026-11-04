//! The text in the input box and where the cursor is in it. Nothing here
//! touches the terminal: keys go in, and `layout` says what to draw.
//!
//! A big paste shows as one piece, `[pasted 40 lines]`, which the cursor
//! steps over and Backspace takes out whole; what's sent is the text.

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

use super::history::History;
use super::keys::Key;

#[derive(Default)]
pub struct Editor {
    text: Vec<char>,
    /// The cursor, as an index into `text`.
    cursor: usize,
    /// Messages sent before, for ↑ and ↓.
    history: History,
    /// Where ↑ has got to in `history`, and what was typed before it.
    browsing: Option<(usize, Entry)>,
    /// Big pastes, each standing in `text` as one character of its own:
    /// [`PASTED`] plus its index here.
    pastes: Vec<String>,
}

/// What the box held, as kept for ↑ and Ctrl-R: the text, with each big
/// paste standing in it as one character, and the pastes, so it comes back
/// looking as it did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pastes: Vec<String>,
}

impl Entry {
    /// The text, with pastes in full: what's sent.
    pub fn full(&self) -> String {
        self.text.chars().map(|c| paste(c, &self.pastes).map_or_else(|| c.to_string(), str::to_owned)).collect()
    }

    /// The text as the box shows it, pastes as their labels.
    pub fn shown(&self) -> String {
        self.text.chars().map(|c| paste(c, &self.pastes).map_or_else(|| c.to_string(), label)).collect()
    }
}

/// The paste `c` stands for among `pastes`, if it stands for one.
fn paste(c: char, pastes: &[String]) -> Option<&str> {
    let index = (c as u32).checked_sub(PASTED)? as usize;
    pastes.get(index).map(String::as_str)
}

/// How a paste shows in the box: `[pasted 40 lines]`.
fn label(paste: &str) -> String {
    let lines = paste.lines().count();
    if lines > 1 { format!("[pasted {lines} lines]") } else { format!("[pasted {} characters]", paste.chars().count()) }
}

/// The character that stands for the `index`th paste.
fn stand_in(index: usize) -> char {
    char::from_u32(PASTED + index as u32).unwrap_or('?')
}

/// Where the characters that stand for pastes start: a private-use plane
/// of Unicode, which nobody types.
const PASTED: u32 = 0xF0000;

/// A paste longer than this shows as one piece.
const BIG_PASTE_LINES: usize = 5;
const BIG_PASTE_CHARS: usize = 1000;

/// What a key asks for, beyond changing the text.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more: the box just needs drawing again.
    Edited,
    /// Enter: send the text.
    Submit,
}

impl Editor {
    /// An empty box, with the messages in `history` for ↑ and ↓.
    pub fn with_history(history: History) -> Editor {
        Editor { history, ..Editor::default() }
    }

    /// The text, with pastes in full.
    pub fn text(&self) -> String {
        self.entry().full()
    }

    /// What the box holds, with only the pastes still in it.
    pub fn entry(&self) -> Entry {
        let mut pastes = Vec::new();
        let text = self
            .text
            .iter()
            .map(|&c| match paste(c, &self.pastes) {
                Some(paste) => {
                    pastes.push(paste.to_owned());
                    stand_in(pastes.len() - 1)
                }
                None => c,
            })
            .collect();
        Entry { text, pastes }
    }

    /// Puts `entry` in the box, cursor at the end.
    pub fn restore(&mut self, entry: &Entry) {
        self.load(entry);
        self.browsing = None;
    }

    fn load(&mut self, entry: &Entry) {
        self.text = entry.text.chars().collect();
        self.pastes = entry.pastes.clone();
        self.cursor = self.text.len();
    }

    /// How a character looks in the box: a paste as its label.
    fn shown(&self, c: char) -> Option<String> {
        paste(c, &self.pastes).map(label)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the text, cursor at the end.
    pub fn replace(&mut self, text: &str) {
        self.set(text);
        self.browsing = None;
    }

    /// Takes the text out, leaving the box empty.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.text.clear();
        self.pastes.clear();
        self.cursor = 0;
        self.browsing = None;
        text
    }

    /// The `@` word the cursor is at the end of, if it's at one: where it
    /// starts, and what's typed after the `@`.
    pub fn mention(&self) -> Option<(usize, String)> {
        let start = self.text[..self.cursor].iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1);
        let at_end = self.text.get(self.cursor).is_none_or(|c| c.is_whitespace());
        (self.text.get(start) == Some(&'@') && at_end).then(|| (start, self.text[start + 1..self.cursor].iter().collect()))
    }

    /// Puts `@path` in place of the `@` word being typed: with a space
    /// after a file, to go on with the message, and none after a folder,
    /// to go on into it.
    pub fn complete_mention(&mut self, path: &str) {
        let Some((start, _)) = self.mention() else { return };
        let space = if path.ends_with('/') { "" } else { " " };
        let new: Vec<char> = format!("@{path}{space}").chars().collect();
        let end = self.cursor;
        self.cursor = start + new.len();
        self.text.splice(start..end, new);
        self.browsing = None;
    }

    /// Keeps a message sent, as the newest for ↑.
    pub fn remember(&mut self, entry: Entry) {
        self.history.add(entry);
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn key(&mut self, key: &Key) -> Action {
        match key {
            Key::Enter => {
                // A line ending in `\` goes on to the next.
                if self.cursor == self.text.len() && self.text.last() == Some(&'\\') {
                    self.text.pop();
                    self.cursor -= 1;
                    self.insert("\n");
                } else {
                    return Action::Submit;
                }
            }
            Key::Newline => self.insert("\n"),
            Key::Char(c) => self.insert(&c.to_string()),
            Key::Paste(text) if text.lines().count() > BIG_PASTE_LINES || text.chars().count() > BIG_PASTE_CHARS => {
                let c = stand_in(self.pastes.len());
                self.pastes.push(text.clone());
                self.insert(&c.to_string());
            }
            Key::Paste(text) => self.insert(text),
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.cursor);
            }
            Key::Delete | Key::CtrlD if self.cursor < self.text.len() => {
                self.text.remove(self.cursor);
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.text.len()),
            Key::WordLeft => {
                while self.cursor > 0 && !self.text[self.cursor - 1].is_alphanumeric() {
                    self.cursor -= 1;
                }
                while self.cursor > 0 && self.text[self.cursor - 1].is_alphanumeric() {
                    self.cursor -= 1;
                }
            }
            Key::WordRight => {
                while self.cursor < self.text.len() && !self.text[self.cursor].is_alphanumeric() {
                    self.cursor += 1;
                }
                while self.cursor < self.text.len() && self.text[self.cursor].is_alphanumeric() {
                    self.cursor += 1;
                }
            }
            Key::Home => self.cursor = self.line_start(),
            Key::End => self.cursor = self.line_end(),
            Key::DeleteWord => {
                let mut start = self.cursor;
                while start > 0 && self.text[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !self.text[start - 1].is_whitespace() {
                    start -= 1;
                }
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            Key::DeleteToStart => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::DeleteToEnd => {
                self.text.truncate(self.cursor);
            }
            Key::Up => {
                if self.on_first_line() {
                    self.older();
                } else {
                    self.vertical(-1);
                }
            }
            Key::Down => {
                if self.on_last_line() {
                    self.newer();
                } else {
                    self.vertical(1);
                }
            }
            _ => {}
        }
        Action::Edited
    }

    /// Puts `text` in at the cursor, as if typed.
    pub fn insert(&mut self, text: &str) {
        for c in text.chars() {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].iter().position(|&c| c == '\n').map_or(self.text.len(), |i| self.cursor + i)
    }

    fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains(&'\n')
    }

    fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains(&'\n')
    }

    /// Moves up (`-1`) or down (`1`) a line, keeping the column if it can.
    fn vertical(&mut self, step: isize) {
        let column = self.cursor - self.line_start();
        if step < 0 {
            self.cursor = self.line_start() - 1; // the end of the line above
        } else {
            self.cursor = self.line_end() + 1; // the start of the line below
        }
        let start = self.line_start();
        self.cursor = (start + column).min(self.line_end());
    }

    fn older(&mut self) {
        let next = match &self.browsing {
            Some((at, _)) if *at > 0 => *at - 1,
            Some(_) => return,
            None if self.history.is_empty() => return,
            None => self.history.len() - 1,
        };
        let draft = match self.browsing.take() {
            Some((_, draft)) => draft,
            None => self.entry(),
        };
        let entry = self.history.get(next).clone();
        self.load(&entry);
        self.browsing = Some((next, draft));
    }

    fn newer(&mut self) {
        match self.browsing.take() {
            Some((at, draft)) if at + 1 < self.history.len() => {
                let entry = self.history.get(at + 1).clone();
                self.load(&entry);
                self.browsing = Some((at + 1, draft));
            }
            Some((_, draft)) => self.load(&draft),
            None => {}
        }
    }

    fn set(&mut self, text: &str) {
        self.text = text.chars().collect();
        self.pastes.clear();
        self.cursor = self.text.len();
    }

    /// The text cut into lines at most `width` columns wide, and where the
    /// cursor is among them: (line, column).
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut lines = vec![String::new()];
        let mut column = 0;
        let mut cursor = (0, 0);
        for (i, &c) in self.text.iter().enumerate() {
            if i == self.cursor {
                cursor = (lines.len() - 1, column);
            }
            if c == '\n' {
                lines.push(String::new());
                column = 0;
                continue;
            }
            let shown = self.shown(c);
            let w = match &shown {
                Some(label) => unicode_width::UnicodeWidthStr::width(label.as_str()),
                None => c.width().unwrap_or(0),
            };
            if column + w > width && column > 0 {
                lines.push(String::new());
                column = 0;
                if i == self.cursor {
                    cursor = (lines.len() - 1, 0);
                }
            }
            let line = lines.last_mut().expect("never empty");
            match shown {
                Some(label) => line.push_str(&label),
                None => line.push(c),
            }
            column += w;
        }
        if self.cursor == self.text.len() {
            if column >= width {
                lines.push(String::new());
                column = 0;
            }
            cursor = (lines.len() - 1, column);
        }
        (lines, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.key(&Key::Char(c));
        }
    }

    #[test]
    fn edits_like_a_shell() {
        let mut e = Editor::default();
        typed(&mut e, "fix the bug");
        e.key(&Key::DeleteWord);
        typed(&mut e, "tests");
        assert_eq!(e.text(), "fix the tests");
        e.key(&Key::Home);
        e.key(&Key::Delete);
        typed(&mut e, "F");
        assert_eq!(e.text(), "Fix the tests");
        e.key(&Key::End);
        e.key(&Key::Backspace);
        assert_eq!(e.text(), "Fix the test");
        e.key(&Key::WordLeft);
        typed(&mut e, "unit ");
        assert_eq!(e.text(), "Fix the unit test");
        e.key(&Key::Home);
        e.key(&Key::WordRight);
        typed(&mut e, "ed");
        assert_eq!(e.text(), "Fixed the unit test");
        assert_eq!(e.key(&Key::Enter), Action::Submit);
    }

    #[test]
    fn a_backslash_or_ctrl_j_starts_a_new_line() {
        let mut e = Editor::default();
        typed(&mut e, "one\\");
        assert_eq!(e.key(&Key::Enter), Action::Edited);
        typed(&mut e, "two");
        e.key(&Key::Newline);
        typed(&mut e, "three");
        assert_eq!(e.text(), "one\ntwo\nthree");
        // ↑ moves between lines before it reaches the history.
        e.key(&Key::Up);
        e.key(&Key::Up);
        typed(&mut e, ">");
        assert_eq!(e.text(), "one>\ntwo\nthree");
    }

    fn entry(text: &str) -> Entry {
        Entry { text: text.to_owned(), pastes: Vec::new() }
    }

    #[test]
    fn up_and_down_walk_the_history() {
        let mut e = Editor::default();
        e.remember(entry("first"));
        e.remember(entry("second"));
        typed(&mut e, "draft");
        e.key(&Key::Up);
        assert_eq!(e.text(), "second");
        e.key(&Key::Up);
        e.key(&Key::Up);
        assert_eq!(e.text(), "first");
        e.key(&Key::Down);
        e.key(&Key::Down);
        assert_eq!(e.text(), "draft");
    }

    #[test]
    fn a_big_paste_is_one_piece() {
        let mut e = Editor::default();
        typed(&mut e, "look: ");
        let log: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        e.key(&Key::Paste(log.clone()));
        typed(&mut e, " why?");
        assert_eq!(e.layout(80).0, ["look: [pasted 40 lines] why?"]);
        assert_eq!(e.text(), format!("look: {log} why?"), "what's sent is the whole paste");
        assert_eq!(e.entry().shown(), "look: [pasted 40 lines] why?");
        // Backspace over it takes it out whole.
        for _ in 0..6 {
            e.key(&Key::Backspace);
        }
        assert_eq!(e.text(), "look: ");
        // A short paste is just text.
        e.key(&Key::Paste("two\nlines".into()));
        assert_eq!(e.layout(80).0, ["look: two", "lines"]);
    }

    #[test]
    fn completes_what_an_at_points_at() {
        let mut e = Editor::default();
        typed(&mut e, "why does @src/pa");
        assert_eq!(e.mention(), Some((9, "src/pa".into())));
        e.complete_mention("src/parser.rs");
        assert_eq!(e.text(), "why does @src/parser.rs ");
        assert_eq!(e.mention(), None, "the space ends it");
        typed(&mut e, "and @s");
        e.complete_mention("src/");
        typed(&mut e, "ui");
        assert_eq!(e.mention(), Some((28, "src/ui".into())), "a folder goes on");
        let mut mail = Editor::default();
        typed(&mut mail, "me@example");
        assert_eq!(mail.mention(), None, "not at the start of a word");
    }

    #[test]
    fn a_message_with_a_paste_comes_back_as_it_looked() {
        let mut e = Editor::default();
        e.key(&Key::Paste("dropped\n".repeat(10)));
        e.key(&Key::Backspace);
        typed(&mut e, "see: ");
        let log = "line\n".repeat(40);
        e.key(&Key::Paste(log.clone()));
        let sent = e.entry();
        assert_eq!(sent.pastes, std::slice::from_ref(&log), "only the paste still in the box is kept");
        assert_eq!(sent.full(), format!("see: {log}"));
        e.remember(sent);
        e.take();
        typed(&mut e, "draft ");
        e.key(&Key::Paste("x\n".repeat(9)));
        e.key(&Key::Up);
        assert_eq!(e.entry().shown(), "see: [pasted 40 lines]");
        assert_eq!(e.text(), format!("see: {log}"));
        e.key(&Key::Down);
        assert_eq!(e.entry().shown(), "draft [pasted 9 lines]", "the draft, paste and all");
    }

    #[test]
    fn lays_the_text_out_in_the_box() {
        let mut e = Editor::default();
        typed(&mut e, "abcdefgh");
        assert_eq!(e.layout(5), (vec!["abcde".into(), "fgh".into()], (1, 3)));
        e.key(&Key::Newline);
        assert_eq!(e.layout(5), (vec!["abcde".into(), "fgh".into(), String::new()], (2, 0)));
        e.key(&Key::Home);
        e.key(&Key::Left);
        e.key(&Key::Left);
        assert_eq!(e.layout(5).1, (1, 2));
        // Wide characters take two columns.
        let mut wide = Editor::default();
        typed(&mut wide, "日本語");
        assert_eq!(wide.layout(4).0, ["日本", "語"]);
    }
}
