//! Colours, used only where they help: blue for rein's labels and code, red
//! and green for what went wrong and right, grey for details. Off when the
//! output isn't a terminal, or when `NO_COLOR` is set.

use std::io::IsTerminal;

/// Labels and the text after them line up in a column this wide.
pub const GUTTER: usize = 7;

#[derive(Clone, Copy)]
pub struct Style {
    on: bool,
}

impl Style {
    /// For stderr, where rein shows its steps and questions.
    pub fn stderr() -> Style {
        Style { on: std::io::stderr().is_terminal() && color_allowed() }
    }

    /// For stdout, where answers go.
    pub fn stdout() -> Style {
        Style { on: std::io::stdout().is_terminal() && color_allowed() }
    }

    /// With no colours: for words that go to a client, which colours them itself.
    pub const fn plain() -> Style {
        Style { on: false }
    }

    #[cfg(test)]
    pub fn colored() -> Style {
        Style { on: true }
    }

    pub fn on(self) -> bool {
        self.on
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.on { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }

    /// `read`, `run`, `rein`…: bold blue, padded to the gutter.
    pub fn label(self, name: &str) -> String {
        format!("{} ", self.paint("1;34", &format!("{name:<width$}", width = GUTTER - 1)))
    }

    /// `you`: your messages' label, grey, so that rein's blue one stands
    /// out from it, padded to the gutter.
    pub fn you(self) -> String {
        format!("{} ", self.paint("90", &format!("{:<width$}", "you", width = GUTTER - 1)))
    }

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    /// The terminal's brightest white: the bullet the model's words start with.
    pub fn bright(self, text: &str) -> String {
        self.paint("97", text)
    }

    pub fn dim(self, text: &str) -> String {
        self.paint("90", text)
    }

    pub fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn blue(self, text: &str) -> String {
        self.paint("34", text)
    }

    pub fn underline(self, text: &str) -> String {
        self.paint("4", text)
    }
}

/// <https://no-color.org>: any non-empty `NO_COLOR` turns colour off.
fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// How many columns `text` takes on screen, not counting colour codes.
pub fn width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(strip(text).as_str())
}

/// `text` cut to `max` columns, colour codes kept, with `…` if anything
/// was cut. A line that fits can't wrap, which a line redrawn in place
/// (like a spinner) depends on.
pub fn fit(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for c in chars.by_ref() {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    if text.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
}

/// `text` without its colour codes.
pub fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip to the end of the escape: a letter after `[`.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_fill_the_gutter() {
        assert_eq!(Style::plain().label("run"), "run    ");
        assert_eq!(width(&Style::colored().label("recall")), GUTTER);
    }

    #[test]
    fn fits_coloured_text_to_a_width() {
        let s = Style::colored();
        let line = format!("{} {}", s.bold("Run"), "a very long command line");
        let cut = fit(&line, 12);
        assert_eq!(strip(&cut), "Run a very …");
        assert_eq!(width(&cut), 12);
        assert_eq!(fit("short", 12), "short");
    }

    #[test]
    fn colour_codes_take_no_room() {
        let red = Style::colored().red("✗ exit 1");
        assert_ne!(red, "✗ exit 1");
        assert_eq!(width(&red), 8);
        assert_eq!(strip(&red), "✗ exit 1");
    }
}
