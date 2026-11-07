//! A message as rows. A window lays out a row whole, so one row for a whole
//! message means a 200 KB answer is laid out whole on every frame it grows.
//! Each finished block is a row of its own, so only the last ever changes,
//! and what's off screen isn't laid out at all.
//!
//! A block is finished once what follows it can't change it: a blank line
//! outside a code fence, and the start of the next block. A long code fence
//! is cut every [`FENCE_LINES`] lines, each piece a fence of its own, or one
//! 20,000-line block would be the same problem again. A table stays whole
//! until it ends: a new row can change the width of every row before it.

use std::ops::Range;

/// The most lines of code in one row.
const FENCE_LINES: usize = 64;
/// How much of a table that's still arriving is shown.
const HELD_LINES: usize = 6;

/// What the block being read is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Open {
    Prose,
    /// Inside a code fence, opened by this line, `lines` into this piece.
    Fence {
        opener: String,
        lines: usize,
    },
}

/// One message cut into rows as it arrives. It keeps only offsets into the
/// message's text, and never reads further back than the last row: what's
/// finished is never looked at again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocks {
    /// Each finished row's range of the text.
    finished: Vec<Range<usize>>,
    /// Where the row still growing starts.
    tail: usize,
    /// Where the next line to look at starts.
    read: usize,
    open: Open,
    /// The fence that opens the piece being read, which isn't in the text.
    /// `(row, opener)`: rows from `row` on start with `opener`.
    reopened: Vec<(usize, String)>,
}

impl Default for Blocks {
    fn default() -> Blocks {
        Blocks { finished: Vec::new(), tail: 0, read: 0, open: Open::Prose, reopened: Vec::new() }
    }
}

/// The run of fence marks `line` starts with, if it's long enough to be one.
fn fence(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let mark = *line.as_bytes().first()?;
    let marks = line.bytes().take_while(|byte| *byte == mark).count();
    (matches!(mark, b'`' | b'~') && marks >= 3).then(|| &line[..marks])
}

/// Whether `line` opens a fence: backticks with more behind them are inline code.
fn opens(line: &str) -> bool {
    fence(line).is_some_and(|marks| !marks.starts_with('`') || !line.trim_start()[marks.len()..].contains('`'))
}

/// Whether `line` closes the fence `opener` opened, as CommonMark has it: the
/// same mark, at least as many, and nothing after them.
fn closes(line: &str, opener: &str) -> bool {
    let (Some(marks), Some(opened)) = (fence(line), fence(opener)) else { return false };
    marks.as_bytes()[0] == opened.as_bytes()[0] && marks.len() >= opened.len() && line.trim() == marks
}

impl Blocks {
    /// Reads on, now that the message's text is `text`. How many rows there
    /// are now.
    pub fn grew(&mut self, text: &str) -> usize {
        while let Some(end) = text[self.read..].find('\n').map(|at| self.read + at + 1) {
            let line = &text[self.read..end];
            self.read = end;
            self.line(line);
        }
        self.rows(text)
    }

    fn line(&mut self, line: &str) {
        let body = line.trim_end_matches('\n');
        match &mut self.open {
            Open::Prose => {
                if opens(body) {
                    self.finish_before(line.len());
                    self.open = Open::Fence { opener: body.trim_start().to_owned(), lines: 0 };
                } else if body.trim().is_empty() {
                    self.blank(line.len());
                }
            }
            Open::Fence { opener, lines } => {
                let closed = closes(body, opener);
                *lines += 1;
                if closed {
                    self.open = Open::Prose;
                    self.finish();
                } else if *lines >= FENCE_LINES {
                    let opener = opener.clone();
                    self.open = Open::Fence { opener: opener.clone(), lines: 0 };
                    self.finish();
                    self.reopened.push((self.finished.len(), opener));
                }
            }
        }
    }

    /// A blank line of `length` bytes ends the row it follows. One with
    /// nothing before it, as after a code block, is no row of its own, which
    /// would draw as an empty line: it stays with the row above, so what's
    /// copied keeps it.
    fn blank(&mut self, length: usize) {
        if self.read - length > self.tail {
            self.finish();
        } else if let Some(last) = self.finished.last_mut() {
            last.end = self.read;
            self.tail = self.read;
        } else {
            self.tail = self.read;
        }
    }

    /// Ends the row at where reading got to.
    fn finish(&mut self) {
        if self.read > self.tail {
            self.finished.push(self.tail..self.read);
            self.tail = self.read;
        }
    }

    /// Ends the row before the line just read, of `length` bytes.
    fn finish_before(&mut self, length: usize) {
        let before = self.read - length;
        if before > self.tail {
            self.finished.push(self.tail..before);
            self.tail = before;
        }
    }

    /// How many rows `text` is: the finished ones, and any still growing.
    pub fn rows(&self, text: &str) -> usize {
        self.finished.len() + usize::from(self.tail < text.len())
    }

    /// How many rows will never change again.
    pub fn finished(&self) -> usize {
        self.finished.len()
    }

    /// Row `row` as Markdown of its own. A piece of a cut fence gets its
    /// fence put back at either end, so each piece reads as code.
    pub fn row<'a>(&self, row: usize, text: &'a str) -> Option<std::borrow::Cow<'a, str>> {
        let body = &text[self.source(row, text)?];
        // A table laid out on every row that arrives is the cost that moved
        // this into rows: it's shown as it is until it's whole, as Codex does.
        if self.held(row, body) {
            let lines: Vec<&str> = body.lines().collect();
            let shown = lines[lines.len().saturating_sub(HELD_LINES)..].join("\n");
            return Some(format!("```\n{shown}\n```\n{} rows so far\n", lines.len().saturating_sub(2)).into());
        }
        let opened = self.reopened.iter().find(|(from, _)| *from == row).map(|(_, opener)| opener.as_str());
        let cut_after = self.reopened.iter().any(|(from, _)| *from == row + 1);
        Some(match (opened, cut_after) {
            (None, false) => body.into(),
            (opened, cut_after) => {
                let mut whole = String::with_capacity(body.len() + 16);
                if let Some(opener) = opened {
                    whole.push_str(opener);
                    whole.push('\n');
                }
                whole.push_str(body);
                if cut_after {
                    let opener = opened.or_else(|| self.reopened.iter().find(|(from, _)| *from == row + 1).map(|(_, opener)| opener.as_str())).unwrap_or("```");
                    whole.push_str(fence(opener).unwrap_or("```"));
                    whole.push('\n');
                }
                whole.into()
            }
        })
    }

    /// Row `row`, which is `body`, is a table still arriving.
    fn held(&self, row: usize, body: &str) -> bool {
        row == self.finished.len() && body.starts_with('|')
    }

    /// Where `at`, a place in what [`row`](Self::row) shows, is in what row
    /// `row` covers. What was put back to show it isn't in there, and a table
    /// still arriving shows only its end, so it has no such place.
    pub fn in_source(&self, row: usize, text: &str, at: usize) -> Option<usize> {
        let source = self.source(row, text)?;
        if self.held(row, &text[source.clone()]) {
            return None;
        }
        let put_back = self.reopened.iter().find(|(from, _)| *from == row).map_or(0, |(_, opener)| opener.len() + 1);
        Some(at.saturating_sub(put_back).min(source.len()))
    }

    /// The range of the message's text that row `row` covers, which is what
    /// copying from it means: never the fences put back to show it.
    pub fn source(&self, row: usize, text: &str) -> Option<Range<usize>> {
        self.finished.get(row).cloned().or_else(|| (row == self.finished.len() && self.tail < text.len()).then_some(self.tail..text.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `text` comes to, fed in `size` bytes at a time.
    fn cut(text: &str, size: usize) -> (Blocks, Vec<String>) {
        let mut blocks = Blocks::default();
        let mut to = 0;
        while to < text.len() {
            to = (to + size).min(text.len());
            while !text.is_char_boundary(to) {
                to += 1;
            }
            blocks.grew(&text[..to]);
        }
        let rows = (0..blocks.rows(text)).map(|row| blocks.row(row, text).unwrap().into_owned()).collect();
        (blocks, rows)
    }

    #[test]
    fn a_blank_line_on_its_own_is_no_row() {
        let text = "```\ncode\n```\n\n\nAfter it.\n";
        let (blocks, rows) = cut(text, 3);
        assert_eq!(rows, ["```\ncode\n```\n\n\n", "After it.\n"]);
        assert_eq!(blocks.source(0, text), Some(0..15), "copied, the blank lines are still there");
        assert_eq!(cut("\n\nFirst.\n", 2).1, ["First.\n"], "nor is one at the very start");
    }

    #[test]
    fn a_block_ends_at_a_blank_line_and_only_the_last_is_still_growing() {
        let text = "# Title\n\nFirst paragraph\nstill first.\n\nSecond, not ended";
        let (blocks, rows) = cut(text, 3);
        assert_eq!(rows, ["# Title\n\n", "First paragraph\nstill first.\n\n", "Second, not ended"]);
        assert_eq!(blocks.finished(), 2, "the last one can still change");
    }

    #[test]
    fn however_it_arrives_the_rows_are_the_same() {
        let text = "One.\n\n```rust\nlet a = 1;\n\nlet b = 2;\n```\nAfter, with no gap.\n\n| a |\n|---|\n| 1 |\n\nEnd.\n";
        let (_, whole) = cut(text, text.len());
        for size in [1, 2, 3, 7, 16] {
            assert_eq!(cut(text, size).1, whole, "{size} bytes at a time");
        }
    }

    #[test]
    fn a_blank_line_inside_code_doesnt_end_it_and_a_fence_starts_a_block_of_its_own() {
        let text = "Before\n```\none\n\ntwo\n```\nafter\n";
        let (_, rows) = cut(text, 4);
        assert_eq!(rows, ["Before\n", "```\none\n\ntwo\n```\n", "after\n"]);
    }

    #[test]
    fn a_table_stays_whole_until_it_ends() {
        let text = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\nafter\n";
        let (_, rows) = cut(text, 5);
        assert_eq!(rows[0], "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n");
    }

    #[test]
    fn a_table_still_arriving_is_shown_as_it_is_with_only_its_end_in_view() {
        let rows: String = (0..50).map(|i| format!("| {i} |\n")).collect();
        let text = format!("| a |\n|---|\n{rows}");
        let (_, shown) = cut(&text, 9);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].starts_with("```\n") && shown[0].contains("| 49 |") && !shown[0].contains("| 1 |"), "{}", shown[0]);
        assert!(shown[0].ends_with("50 rows so far\n"), "{}", shown[0]);
        let ended = format!("{text}\n");
        let (_, whole) = cut(&ended, 9);
        assert_eq!(whole, std::slice::from_ref(&ended), "and whole once it has ended");
    }

    #[test]
    fn a_long_fence_is_cut_and_each_piece_is_code_and_the_source_is_what_was_said() {
        let lines: String = (0..FENCE_LINES * 2 + 3).map(|i| format!("line {i}\n")).collect();
        let text = format!("```rust\n{lines}```\nafter\n");
        let (blocks, rows) = cut(&text, 11);
        assert!(rows.len() >= 3, "{}", rows.len());
        assert!(rows[0].ends_with("line 63\n```\n"), "{}", rows[0]);
        for row in &rows[..rows.len() - 1] {
            assert!(row.starts_with("```rust\n") && row.ends_with("```\n"), "{row}");
        }
        let copied: String = (0..rows.len()).map(|row| text[blocks.source(row, &text).unwrap()].to_owned()).collect();
        assert_eq!(copied, text, "nothing added to what's copied, nothing lost");
    }

    #[test]
    fn a_fence_closes_as_commonmark_says() {
        let holds_a_shorter_one = "````md\n```\ninner\n```\n````\nafter\n";
        assert_eq!(cut(holds_a_shorter_one, 5).1, ["````md\n```\ninner\n```\n````\n", "after\n"]);
        assert_eq!(cut("```\ncode\n````\nafter\n", 5).1, ["```\ncode\n````\n", "after\n"], "more marks than it opened with close it");
        assert_eq!(cut("```x```\n\nnext\n", 4).1, ["```x```\n\n", "next\n"], "backticks with more behind them are inline code");
        assert_eq!(cut("~~~\none\n\ntwo\n~~~\nafter\n", 4).1, ["~~~\none\n\ntwo\n~~~\n", "after\n"], "tildes are a fence too");
        assert_eq!(cut("~~~\n```\n~~~\nafter\n", 4).1, ["~~~\n```\n~~~\n", "after\n"], "one mark doesn't close the other");
        assert_eq!(cut("```\ncode\n``` not yet\n```\nafter\n", 6).1, ["```\ncode\n``` not yet\n```\n", "after\n"], "words after it mean it isn't a close");
    }

    #[test]
    fn a_place_in_what_a_row_shows_is_found_in_what_was_said() {
        let lines: String = (0..FENCE_LINES * 2).map(|i| format!("line {i}\n")).collect();
        let text = format!("```rust\n{lines}```\n");
        let (blocks, rows) = cut(&text, 11);
        assert_eq!(blocks.in_source(0, &text, 8), Some(8), "a row shown as it is");
        let shown = rows[1].find("line 70").unwrap();
        let start = blocks.source(1, &text).unwrap().start;
        assert_eq!(&text[start + blocks.in_source(1, &text, shown).unwrap()..][..7], "line 70", "the fence put back isn't in what was said");
        let table = "| a |\n|---|\n| 1 |\n";
        assert_eq!(cut(table, 5).0.in_source(0, table, 5), None, "a table still arriving shows only its end");
    }

    #[test]
    fn reading_on_never_goes_back_over_whats_finished() {
        let mut text = String::new();
        let mut blocks = Blocks::default();
        for i in 0..2_000 {
            text.push_str(&format!("Paragraph {i}.\n\n"));
            blocks.grew(&text);
        }
        assert_eq!(blocks.finished(), 2_000);
        assert_eq!(blocks.read, text.len(), "it only ever reads what's new");
    }
}
