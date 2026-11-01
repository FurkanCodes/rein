//! Markdown, rendered as it streams in.
//!
//! Answers arrive a few characters at a time, so this works one character
//! at a time too. Two things can't be decided straight away, and wait for
//! the characters that settle them: the start of a line (is `- ` a list
//! item? is "```" a code block?) and a run of `*` (emphasis, or a times
//! sign?). Everything else goes out at once. The result is the same however
//! the text is split into pieces.
//!
//! Covered: headings, bold, italics, inline code, code blocks, lists and
//! quotes. Anything else is shown as written.

pub struct Markdown {
    color: bool,
    /// The start of the current line, until it's clear what it is.
    start: String,
    deciding: bool,
    in_block: bool,
    /// A code fence line: shown as nothing at all.
    skip_line: bool,
    /// A line inside a code block: shown as written.
    raw_line: bool,
    heading: bool,
    bold: bool,
    italic: bool,
    code: bool,
    /// `*`s seen but not yet known to be emphasis.
    stars: usize,
    /// The last character shown, for deciding what a `*` is.
    prev: char,
}

/// Characters that may still turn out to be a line's marker.
fn marker(c: char) -> bool {
    matches!(c, ' ' | '#' | '-' | '*' | '+' | '>' | '`' | '.' | ')') || c.is_ascii_digit()
}

impl Markdown {
    pub fn new(color: bool) -> Self {
        Markdown {
            color,
            start: String::new(),
            deciding: true,
            in_block: false,
            skip_line: false,
            raw_line: false,
            heading: false,
            bold: false,
            italic: false,
            code: false,
            stars: 0,
            prev: ' ',
        }
    }

    pub fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            if c == '\n' {
                if self.deciding {
                    out += &self.decide();
                }
                out += &self.newline();
            } else if self.deciding {
                self.start.push(c);
                if !marker(c) || self.start.chars().count() >= 8 {
                    out += &self.decide();
                }
            } else {
                out += &self.inline(c);
            }
        }
        out
    }

    /// Whatever is still held back, at the end of an answer.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if self.deciding && !self.start.is_empty() {
            out += &self.decide();
        }
        out += &self.resolve(' ');
        if self.styled() {
            self.reset_inline();
            out += &self.sgr();
        }
        self.in_block = false;
        self.deciding = true;
        out
    }

    /// The start of the line is settled: show its marker, then the rest.
    fn decide(&mut self) -> String {
        self.deciding = false;
        let line = std::mem::take(&mut self.start);
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if self.in_block {
            if trimmed.starts_with("```") {
                self.in_block = false;
                self.skip_line = true;
                return String::new();
            }
            self.raw_line = true;
            return format!("{}{line}", self.paint("34", "│ "));
        }
        if trimmed.starts_with("```") {
            self.in_block = true;
            self.skip_line = true;
            return String::new();
        }
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            self.heading = true;
            return self.sgr() + &self.inline_str(&trimmed[hashes + 1..]);
        }
        for bullet in ["- ", "* ", "+ "] {
            if let Some(rest) = trimmed.strip_prefix(bullet) {
                return format!("{indent}· {}", self.inline_str(rest));
            }
        }
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && (trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") ")) {
            return format!("{indent}{}", &trimmed[..digits + 2]) + &self.inline_str(&trimmed[digits + 2..]);
        }
        if let Some(rest) = trimmed.strip_prefix("> ") {
            return format!("{indent}{}", self.paint("90", "│ ")) + &self.inline_str(rest);
        }
        self.inline_str(&line)
    }

    fn newline(&mut self) -> String {
        let mut out = self.resolve(' ');
        if self.styled() {
            self.reset_inline();
            out += &self.sgr();
        }
        self.heading = false;
        self.raw_line = false;
        self.deciding = true;
        self.prev = ' ';
        if std::mem::take(&mut self.skip_line) {
            return out; // a fence line leaves no trace, not even a newline
        }
        out.push('\n');
        out
    }

    fn inline_str(&mut self, text: &str) -> String {
        text.chars().map(|c| self.inline(c)).collect()
    }

    fn inline(&mut self, c: char) -> String {
        if self.skip_line {
            return String::new();
        }
        if self.raw_line {
            return c.to_string();
        }
        if c == '*' && !self.code {
            self.stars += 1;
            return String::new();
        }
        let mut out = self.resolve(c);
        if c == '`' {
            self.code = !self.code;
            out += &self.sgr();
        } else {
            out.push(c);
        }
        self.prev = c;
        out
    }

    /// Decides what the waiting `*`s were, now that `next` is known.
    fn resolve(&mut self, next: char) -> String {
        let stars = std::mem::take(&mut self.stars);
        let opens = !next.is_whitespace();
        let after_space = self.prev.is_whitespace();
        let closes = !self.prev.is_whitespace();
        let changed = match stars {
            0 => return String::new(),
            1 if self.italic && closes => {
                self.italic = false;
                true
            }
            1 if !self.italic && opens && after_space => {
                self.italic = true;
                true
            }
            2 if self.bold && closes => {
                self.bold = false;
                true
            }
            2 if !self.bold && opens => {
                self.bold = true;
                true
            }
            3 if self.bold && self.italic && closes => {
                self.bold = false;
                self.italic = false;
                true
            }
            3 if !self.bold && !self.italic && opens => {
                self.bold = true;
                self.italic = true;
                true
            }
            _ => false,
        };
        if changed {
            self.prev = '*';
            self.sgr()
        } else {
            self.prev = '*';
            "*".repeat(stars)
        }
    }

    fn styled(&self) -> bool {
        self.heading || self.bold || self.italic || self.code
    }

    fn reset_inline(&mut self) {
        self.heading = false;
        self.bold = false;
        self.italic = false;
        self.code = false;
    }

    /// The colour codes for the current style, from scratch.
    fn sgr(&self) -> String {
        if !self.color {
            return String::new();
        }
        let mut codes = vec!["0"];
        if self.bold || self.heading {
            codes.push("1");
        }
        if self.italic {
            codes.push("3");
        }
        if self.code || self.heading {
            codes.push("34");
        }
        format!("\x1b[{}m", codes.join(";"))
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str) -> String {
        let mut md = Markdown::new(false);
        md.push(text) + &md.finish()
    }

    #[test]
    fn renders_the_common_parts() {
        let text = "# Title\n**Done.** It says `ok`:\n\n```text\nNo tasks yet. *Add* one.\n```\n\n- first\n  * nested\n1. one\n> a quote\n2 * 3 = 6, and a_b_c stays.\n";
        assert_eq!(
            render(text),
            "Title\nDone. It says ok:\n\n│ No tasks yet. *Add* one.\n\n· first\n  · nested\n1. one\n│ a quote\n2 * 3 = 6, and a_b_c stays.\n"
        );
    }

    #[test]
    fn styles_with_colour() {
        let mut md = Markdown::new(true);
        let out = md.push("**Done.** `ls`\n") + &md.finish();
        assert_eq!(out, "\x1b[0;1mDone.\x1b[0m \x1b[0;34mls\x1b[0m\n");
    }

    #[test]
    fn the_same_however_the_text_is_split() {
        let text = "## Plan\nSome **bold** and *italic* and `code` here.\n\n```rust\nfn main() { let x = 2 * 3; }\n```\n- item **one**\n10. ten\nend";
        let whole = {
            let mut md = Markdown::new(true);
            md.push(text) + &md.finish()
        };
        let chars: Vec<char> = text.chars().collect();
        for size in 1..=7 {
            let mut md = Markdown::new(true);
            let mut out = String::new();
            for piece in chars.chunks(size) {
                out += &md.push(&piece.iter().collect::<String>());
            }
            out += &md.finish();
            assert_eq!(out, whole, "pieces of {size}");
        }
    }

    #[test]
    fn unclosed_styles_end_with_the_line() {
        let mut md = Markdown::new(true);
        let out = md.push("**never closed\nnext") + &md.finish();
        assert_eq!(out, "\x1b[0;1mnever closed\x1b[0m\nnext");
    }
}
