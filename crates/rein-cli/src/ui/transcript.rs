//! What the conversation has shown, kept as lines of text rather than as
//! rows of a screen, so it can be laid out again for any width: after a
//! resize, words wrap where they should at the new width, and nothing is
//! lost.
//!
//! A line is wrapped between words. Its later rows start where its text
//! did: under the label column for `rein   …`, and under the text of a list
//! item rather than its bullet. Steps lay themselves out for the width
//! (a long command is cut, so the result stays in view).

use super::style::{GUTTER, fit, strip, width};
use super::wrap::Wrap;

/// One row of the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub follows: Follows,
}

/// How a row goes on from the one above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follows {
    /// It doesn't: it starts a line.
    Nothing,
    /// One line, wrapped between two words.
    Word,
    /// One word, too long for a row, like a link: no space goes between
    /// them, or a link copied from the screen leads nowhere.
    SameWord,
}

/// Lays out a step for a width.
pub type Draw = Box<dyn Fn(usize) -> Vec<String> + Send>;

enum Block {
    /// A line of text, wrapped at the width.
    Text(String),
    /// Lines laid out by a function, like a step and its details. One
    /// drawn under a key can be drawn again in place, while it's last.
    Drawn(Draw, Option<u64>),
}

#[derive(Default)]
pub struct Transcript {
    blocks: Vec<Block>,
    /// The last line, still being written.
    open: String,
    /// Every block's rows, at `width`.
    rows: Vec<Row>,
    /// How many blocks `rows` holds.
    laid_out: usize,
    /// How many of `rows` the last of them made.
    last_rows: usize,
    width: usize,
}

impl Transcript {
    /// Text as it's printed: complete lines become blocks, and what comes
    /// after the last newline waits for the rest of its line.
    pub fn write(&mut self, text: &str) {
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            self.open.push_str(first);
        }
        for part in parts {
            let line = std::mem::replace(&mut self.open, part.to_owned());
            self.blocks.push(Block::Text(line));
        }
    }

    /// Lines that lay themselves out, after whatever is being written.
    pub fn draw(&mut self, draw: Draw) {
        self.draw_as(None, draw);
    }

    /// The same, under `key`, for [`Transcript::redraw`].
    pub fn draw_as(&mut self, key: Option<u64>, draw: Draw) {
        if !self.open.is_empty() {
            let line = std::mem::take(&mut self.open);
            self.blocks.push(Block::Text(line));
        }
        self.blocks.push(Block::Drawn(draw, key));
    }

    /// Draws the block under `key` again, with `draw`, if nothing has come
    /// after it. Says whether it could.
    pub fn redraw(&mut self, key: u64, draw: Draw) -> bool {
        let last = matches!(self.blocks.last(), Some(Block::Drawn(_, Some(k))) if *k == key);
        if !last || !self.open.is_empty() {
            return false;
        }
        if let Some(block) = self.blocks.last_mut() {
            *block = Block::Drawn(draw, Some(key));
        }
        // Its rows are laid out again, and only its.
        if self.laid_out == self.blocks.len() {
            self.rows.truncate(self.rows.len() - self.last_rows);
            self.laid_out -= 1;
        }
        true
    }

    /// Forgets every block's rows, so the next [`Transcript::rows`] lays
    /// them all out again.
    pub fn lay_out_again(&mut self) {
        self.width = 0;
    }

    /// How many blocks there are, to show the ones after it later.
    pub fn mark(&self) -> usize {
        self.blocks.len()
    }

    /// All the rows at `width`, the open line's included.
    pub fn rows(&mut self, width: usize) -> (&[Row], Vec<Row>) {
        let width = width.max(10);
        if width != self.width {
            self.width = width;
            self.rows.clear();
            self.laid_out = 0;
        }
        for block in &self.blocks[self.laid_out..] {
            let rows = lay_out(block, width);
            self.last_rows = rows.len();
            self.rows.extend(rows);
        }
        self.laid_out = self.blocks.len();
        let open = if self.open.is_empty() { Vec::new() } else { text_rows(&self.open, width) };
        (&self.rows, open)
    }

    /// The rows from block `from` on, at `width`, as text to print: for
    /// leaving the full screen with the conversation still in view.
    pub fn since(&self, from: usize, width: usize) -> String {
        let mut out = String::new();
        for block in &self.blocks[from.min(self.blocks.len())..] {
            for row in lay_out(block, width.max(10)) {
                out += &row.text;
                out += "\x1b[0m\n";
            }
        }
        if !self.open.is_empty() {
            out += &self.open;
            out += "\x1b[0m\n";
        }
        out
    }
}

fn lay_out(block: &Block, width: usize) -> Vec<Row> {
    match block {
        Block::Text(line) => text_rows(line, width),
        Block::Drawn(draw, _) => {
            draw(width).into_iter().flat_map(|line| line.split('\n').map(|l| fit(l, width)).collect::<Vec<_>>()).map(|text| Row { text, follows: Follows::Nothing }).collect()
        }
    }
}

/// One line of text in rows no wider than `width`. Its later rows line up
/// under its text: after a label (`rein   `) or the spaces it starts with.
pub fn text_rows(line: &str, cols: usize) -> Vec<Row> {
    let shown = strip(line);
    let spaces = shown.chars().take_while(|&c| c == ' ').count();
    let label = shown.chars().count() > GUTTER
        && shown.chars().nth(GUTTER - 1) == Some(' ')
        && shown.chars().next().is_some_and(|c| c.is_alphabetic())
        && !shown.chars().take(GUTTER).collect::<String>().trim().contains(' ');
    let indent = if spaces > 0 && spaces < cols / 2 {
        spaces
    } else if label {
        GUTTER
    } else {
        0
    };
    let (lead, rest) = split_at_column(line, indent);
    let mut wrap = Wrap::new(cols, indent, indent);
    let wrapped = wrap.push(&rest) + &wrap.finish();
    let mut rows = Vec::new();
    // Each row is drawn on its own, so a style that runs on from the row
    // above (bold, a colour) starts it again.
    let mut style = String::new();
    for (i, text) in wrapped.split('\n').enumerate() {
        let text = if i == 0 { format!("{lead}{text}") } else { format!("{style}{text}") };
        style = in_force(&text, style);
        // A word longer than the row is broken, not cut.
        let mut text = text;
        let mut follows = if i > 0 { Follows::Word } else { Follows::Nothing };
        loop {
            if width(&text) <= cols {
                rows.push(Row { text, follows });
                break;
            }
            let (head, tail) = split_at_column(&text, cols);
            rows.push(Row { text: head, follows });
            text = format!("{}{tail}", " ".repeat(indent.min(cols / 2)));
            follows = Follows::SameWord;
        }
    }
    rows
}

/// The colour codes in force at the end of `text`, given those in force
/// at its start: the ones since the last that starts over (`ESC [ 0 …`).
fn in_force(text: &str, mut codes: String) -> String {
    let mut rest = text;
    while let Some(start) = rest.find('\x1b') {
        let after = &rest[start..];
        let end = after.find(|c: char| c.is_ascii_alphabetic()).map_or(after.len(), |i| i + 1);
        let code = &after[..end];
        if code.ends_with('m') {
            let params = &code[2..code.len() - 1];
            if params.is_empty() || params == "0" || params.starts_with("0;") {
                codes.clear();
            }
            if !(params.is_empty() || params == "0") {
                codes += code;
            }
        }
        rest = &after[end..];
    }
    codes
}

/// `text` cut at screen column `at`: what's before, and the rest, which
/// starts with the colour codes in force there.
pub fn split_at_column(text: &str, at: usize) -> (String, String) {
    let mut left = String::new();
    let mut codes = String::new();
    let mut used = 0;
    let mut chars = text.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c == '\x1b' {
            let start = i;
            chars.next();
            let mut end = text.len();
            for (j, c) in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    end = j + c.len_utf8();
                    break;
                }
            }
            left += &text[start..end];
            codes += &text[start..end];
            continue;
        }
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > at {
            let rest = &text[i..];
            let reset = if codes.is_empty() { "" } else { "\x1b[0m" };
            return (format!("{left}{reset}"), format!("{codes}{rest}"));
        }
        left.push(c);
        used += w;
        chars.next();
    }
    (left, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|row| strip(&row.text)).collect()
    }

    #[test]
    fn lays_a_line_out_again_for_a_new_width() {
        let mut t = Transcript::default();
        t.write("rein   The quick brown fox jumps over the lazy dog.\n");
        let (rows, _) = t.rows(30);
        assert_eq!(shown(rows), ["rein   The quick brown fox", "       jumps over the lazy", "       dog."]);
        let (rows, _) = t.rows(80);
        assert_eq!(shown(rows), ["rein   The quick brown fox jumps over the lazy dog."], "wider: one row again");
        let (rows, _) = t.rows(20);
        assert!(shown(rows).iter().all(|row| !row.contains("ju mps") && row.chars().count() <= 20));
        assert_eq!((rows[0].follows, rows[1].follows), (Follows::Nothing, Follows::Word));
    }

    #[test]
    fn list_items_and_details_hang() {
        let rows = text_rows("       · one two three four five six", 24);
        assert_eq!(shown(&rows), ["       · one two three", "         four five six"]);
        let rows = text_rows("Using ChatGPT plan (manage usage)", 20);
        assert_eq!(shown(&rows), ["Using ChatGPT plan", "(manage usage)"], "no label, no indent");
    }

    #[test]
    fn a_long_word_is_broken_not_cut() {
        let rows = text_rows("       https://example.com/a/very/long/path", 24);
        assert_eq!(shown(&rows).concat().replace(' ', ""), "https://example.com/a/very/long/path");
        assert!(rows.iter().all(|row| width(&row.text) <= 24));
    }

    #[test]
    fn the_open_line_and_drawn_steps() {
        let mut t = Transcript::default();
        t.write("rein   half a li");
        t.write("ne");
        let (rows, open) = t.rows(40);
        assert!(rows.is_empty());
        assert_eq!(shown(&open), ["rein   half a line"]);
        t.draw(Box::new(|width| vec![format!("step {width}")]));
        let (rows, open) = t.rows(40);
        assert_eq!(shown(rows), ["rein   half a line", "step 40"]);
        assert!(open.is_empty());
    }

    #[test]
    fn a_drawn_block_is_drawn_again_while_its_last() {
        let mut t = Transcript::default();
        t.draw_as(Some(7), Box::new(|_| vec!["read a.rs".into()]));
        let _ = t.rows(40);
        assert!(t.redraw(7, Box::new(|_| vec!["read a.rs, b.rs".into(), "two rows".into()])));
        assert_eq!(shown(t.rows(40).0), ["read a.rs, b.rs", "two rows"], "its rows were laid out again, no others");
        assert!(!t.redraw(8, Box::new(|_| Vec::new())), "another key");
        t.write("rein   then words\n");
        assert!(!t.redraw(7, Box::new(|_| vec!["read a.rs, b.rs, c.rs".into()])), "something came after it");
        assert_eq!(shown(t.rows(40).0), ["read a.rs, b.rs", "two rows", "rein   then words"]);
    }

    #[test]
    fn a_style_carries_on_to_the_next_row() {
        let rows = text_rows("you    \x1b[1mone two three four five six\x1b[0m", 20);
        assert_eq!(shown(&rows), ["you    one two three", "       four five six"]);
        assert!(rows[1].text.contains("\x1b[1m"), "the wrapped row is still bold: {:?}", rows[1].text);
        assert_eq!(in_force("\x1b[1ma\x1b[0mb", String::new()), "", "a reset ends it");
        assert_eq!(in_force("\x1b[0;34mx", "\x1b[1m".into()), "\x1b[0;34m");
    }

    #[test]
    fn splits_coloured_text_at_a_column() {
        let (left, right) = split_at_column("\x1b[1mbold\x1b[0m plain", 2);
        assert_eq!((strip(&left), strip(&right)), ("bo".to_owned(), "ld plain".to_owned()));
        assert!(right.starts_with("\x1b[1m"), "the rest keeps its colour");
    }
}
