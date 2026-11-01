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

    #[cfg(test)]
    pub fn plain() -> Style {
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

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
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
}

/// <https://no-color.org>: any non-empty `NO_COLOR` turns colour off.
fn color_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// How many columns `text` takes on screen, not counting colour codes.
pub fn width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(strip(text).as_str())
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
    fn colour_codes_take_no_room() {
        let red = Style::colored().red("✗ exit 1");
        assert_ne!(red, "✗ exit 1");
        assert_eq!(width(&red), 8);
        assert_eq!(strip(&red), "✗ exit 1");
    }
}
