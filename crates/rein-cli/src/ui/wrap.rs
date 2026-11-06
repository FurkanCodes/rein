//! Streamed text, wrapped between words at the terminal's edge.

/// Wraps streamed text at word boundaries. Words and the spaces before
/// them are held until the word is complete: printed text can't be taken
/// back, so a space is only printed once the next word is known to fit on
/// the same line. Every line starts at `indent`, so text lines up after
/// the label column. Colour codes are part of words but take no room. A
/// word longer than a whole line is printed as is, so the terminal breaks
/// it rather than rein. A list item's lines after its first start under
/// its text, not under its bullet.
pub struct Wrap {
    width: usize,
    indent: usize,
    column: usize,
    /// Where this line's next lines start, when it's a list item.
    hang: Option<usize>,
    /// Nothing but spaces on this line yet.
    fresh: bool,
    spaces: String,
    word: String,
    /// Inside a colour code, which ends at its first letter.
    escape: bool,
}

impl Wrap {
    /// `column` is where the cursor already is, after a label.
    pub fn new(width: usize, indent: usize, column: usize) -> Self {
        Wrap { width, indent, column, hang: None, fresh: true, spaces: String::new(), word: String::new(), escape: false }
    }

    /// The window changed size: lines from here on fit the new width.
    pub fn set_width(&mut self, width: usize) {
        self.width = width;
    }

    /// Takes the next piece of text and returns what can be printed now.
    pub fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            if self.escape {
                self.word.push(c);
                self.escape = !c.is_ascii_alphabetic();
                continue;
            }
            match c {
                '\x1b' => {
                    self.word.push(c);
                    self.escape = true;
                }
                '\n' => {
                    self.flush(&mut out);
                    self.spaces.clear(); // trailing spaces show nothing
                    out.push('\n');
                    self.column = 0;
                    self.hang = None;
                    self.fresh = true;
                }
                ' ' | '\t' => {
                    self.flush(&mut out);
                    self.spaces.push(c);
                }
                c => self.word.push(c),
            }
        }
        out
    }

    /// Whatever is still held back, at the end of an answer.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        self.flush(&mut out);
        self.spaces.clear();
        out
    }

    fn flush(&mut self, out: &mut String) {
        if self.word.is_empty() {
            return;
        }
        if self.column == 0 && self.indent > 0 {
            out.push_str(&" ".repeat(self.indent));
            self.column = self.indent;
        }
        let mut column = self.column;
        for c in self.spaces.chars() {
            column += if c == '\t' { 8 - column % 8 } else { 1 };
        }
        let width = super::style::width(&self.word);
        if std::mem::take(&mut self.fresh) && bullet(&super::style::strip(&self.word)) {
            // The text starts after the bullet and one space.
            self.hang = Some(column + width + 1);
        }
        if self.column > self.indent && column + width > self.width {
            // The spaces before the word are dropped.
            let hang = self.hang.filter(|&hang| hang + width <= self.width).unwrap_or(self.indent);
            out.push('\n');
            out.push_str(&" ".repeat(hang));
            column = hang;
        } else {
            out.push_str(&self.spaces);
        }
        self.spaces.clear();
        out.push_str(&self.word);
        self.word.clear();
        self.column = column + width;
        if self.column > self.width {
            // Longer than a line: the terminal breaks it; follow along.
            self.column %= self.width.max(1);
        }
    }
}

/// What starts a list item: `·`, `-`, `1.`, `☐`…, and the `●` the
/// model's words start with.
fn bullet(word: &str) -> bool {
    let digits = word.chars().take_while(char::is_ascii_digit).count();
    matches!(word, "·" | "-" | "*" | "+" | "•" | "●" | "☐" | "☑" | "▸") || (digits > 0 && matches!(&word[digits..], "." | ")"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapped(width: usize, indent: usize, text: &str) -> String {
        let mut wrap = Wrap::new(width, indent, indent);
        // One character at a time, the hardest way it can arrive.
        let mut out: String = text.chars().map(|c| wrap.push(&c.to_string())).collect();
        out.push_str(&wrap.finish());
        out
    }

    #[test]
    fn wraps_at_words_not_inside_them() {
        let out = wrapped(20, 0, "The quick brown fox jumps over the lazy dog.");
        assert_eq!(out, "The quick brown fox\njumps over the lazy\ndog.");
        assert!(out.lines().all(|line| line.chars().count() <= 20));
        // A line that fits exactly still wraps the next word.
        assert_eq!(wrapped(9, 0, "abcd efgh ijkl"), "abcd efgh\nijkl");
        // Lines the model breaks itself are kept, indentation too.
        assert_eq!(wrapped(40, 0, "list:\n  - one\n  - two"), "list:\n  - one\n  - two");
    }

    #[test]
    fn indents_every_line_after_the_label() {
        assert_eq!(wrapped(16, 7, "one two three four\nfive"), "one two\n       three\n       four\n       five");
        // Blank lines stay blank.
        assert_eq!(wrapped(30, 7, "a\n\nb"), "a\n\n       b");
    }

    #[test]
    fn a_list_item_lines_up_under_its_text() {
        assert_eq!(wrapped(20, 2, "· one two three four five"), "· one two three\n    four five");
        assert_eq!(wrapped(20, 0, "  12. alpha beta gamma delta"), "  12. alpha beta\n      gamma delta");
        // Only the line that starts with the bullet hangs.
        assert_eq!(wrapped(12, 0, "· a\nplain words here"), "· a\nplain words\nhere");
    }

    #[test]
    fn follows_a_new_width() {
        let mut wrap = Wrap::new(20, 0, 0);
        let mut out = wrap.push("one two three ");
        wrap.set_width(10);
        out += &wrap.push("four five six");
        out += &wrap.finish();
        assert_eq!(out, "one two three\nfour five\nsix");
    }

    #[test]
    fn colour_codes_take_no_room() {
        let out = wrapped(10, 0, "\x1b[1mbold\x1b[0m text here");
        assert_eq!(out, "\x1b[1mbold\x1b[0m text\nhere");
    }
}
