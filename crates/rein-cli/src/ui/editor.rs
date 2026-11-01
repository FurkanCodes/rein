//! The text in the input box and where the cursor is in it. Nothing here
//! touches the terminal: keys go in, and `layout` says what to draw.

use unicode_width::UnicodeWidthChar;

use super::keys::Key;

#[derive(Default)]
pub struct Editor {
    text: Vec<char>,
    /// The cursor, as an index into `text`.
    cursor: usize,
    /// Messages sent before, oldest first, for ↑ and ↓.
    history: Vec<String>,
    /// Where ↑ has got to in `history`, and what was typed before it.
    browsing: Option<(usize, String)>,
}

/// What a key asks for, beyond changing the text.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more: the box just needs drawing again.
    Edited,
    /// Enter: send the text.
    Submit,
}

impl Editor {
    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Takes the text out, leaving the box empty.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
        text
    }

    pub fn remember(&mut self, message: &str) {
        if !message.trim().is_empty() && self.history.last().map(String::as_str) != Some(message) {
            self.history.push(message.to_owned());
        }
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

    fn insert(&mut self, text: &str) {
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
            None => self.text(),
        };
        self.set(&self.history[next].clone());
        self.browsing = Some((next, draft));
    }

    fn newer(&mut self) {
        match self.browsing.take() {
            Some((at, draft)) if at + 1 < self.history.len() => {
                self.set(&self.history[at + 1].clone());
                self.browsing = Some((at + 1, draft));
            }
            Some((_, draft)) => self.set(&draft),
            None => {}
        }
    }

    fn set(&mut self, text: &str) {
        self.text = text.chars().collect();
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
            let w = c.width().unwrap_or(0);
            if column + w > width {
                lines.push(String::new());
                column = 0;
                if i == self.cursor {
                    cursor = (lines.len() - 1, 0);
                }
            }
            lines.last_mut().expect("never empty").push(c);
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

    #[test]
    fn up_and_down_walk_the_history() {
        let mut e = Editor::default();
        e.remember("first");
        e.remember("second");
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
