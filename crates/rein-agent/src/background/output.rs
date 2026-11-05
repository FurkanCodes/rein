//! What a command prints, as it's kept, and as it's shown: to the model,
//! cut to fit, and to you, without what a terminal acts on.

use std::collections::VecDeque;

/// What's kept of what a command prints: its start, and its latest.
const HEAD: usize = 64 * 1024;
const TAIL: usize = 1024 * 1024;

/// What a command printed, stdout and stderr as they came: the first
/// [`HEAD`] bytes, and the latest [`TAIL`].
#[derive(Default)]
pub(super) struct Output {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    /// How much it printed in all.
    pub(super) total: usize,
}

impl Output {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        let room = HEAD.saturating_sub(self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..room]);
        self.tail.extend(&bytes[room..]);
        let over = self.tail.len().saturating_sub(TAIL);
        self.tail.drain(..over);
        self.total = self.total.saturating_add(bytes.len());
    }

    /// What's kept of it from byte `from` on, and how much from there
    /// wasn't kept.
    pub(super) fn since(&self, from: usize) -> (Vec<u8>, usize) {
        let tail_start = self.total - self.tail.len();
        if from >= tail_start {
            return (self.tail.iter().skip(from - tail_start).copied().collect(), 0);
        }
        let mut bytes = self.head.get(from..).unwrap_or_default().to_vec();
        let lost = tail_start - from.max(self.head.len());
        bytes.extend(&self.tail);
        (bytes, lost)
    }

    /// All of it that's kept, with a note where some wasn't.
    pub(super) fn all(&self) -> String {
        text(self.since(0))
    }

    /// Its last line with something on it, plain.
    pub(super) fn last_line(&self) -> Option<String> {
        let start = self.tail.len().saturating_sub(4096);
        let latest: Vec<u8> = if self.tail.is_empty() { self.head[self.head.len().saturating_sub(4096)..].to_vec() } else { self.tail.iter().skip(start).copied().collect() };
        String::from_utf8_lossy(&latest).lines().rev().map(plain).find(|line| !line.trim().is_empty()).map(|line| line.trim().to_owned())
    }
}

/// `bytes` as text, with a note of what wasn't kept before them.
pub(super) fn text((bytes, lost): (Vec<u8>, usize)) -> String {
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if lost == 0 { text } else { format!("[{lost} bytes before this weren't kept]\n{text}") }
}

/// `line` without terminal codes or other control characters: what a
/// command prints is never sent to your terminal as it is.
pub fn plain(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // A control sequence: up to its final letter.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // An operating system command: up to BEL or ESC \.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `text` cut to `budget` bytes, if it's longer: its start, and more of its
/// end, where a command says how it went.
pub(super) fn fit(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let head = floor(text, budget / 5);
    let tail = ceil(text, text.len() - (budget - budget / 5));
    format!("{}\n[{} bytes in between not shown]\n{}", &text[..head], tail - head, &text[tail..])
}

fn floor(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_keeps_its_start_and_its_latest() {
        let mut output = Output::default();
        output.push(&vec![b'a'; HEAD]);
        output.push(&vec![b'b'; TAIL]);
        output.push(b"end");
        assert_eq!(output.total, HEAD + TAIL + 3);
        let (all, lost) = output.since(0);
        assert_eq!((all.len(), lost), (HEAD + TAIL, 3));
        assert!(all.ends_with(b"bbend"));
        let (new, lost) = output.since(HEAD + TAIL);
        assert_eq!((new, lost), (b"end".to_vec(), 0));
        let (from_head, lost) = output.since(10);
        assert_eq!((from_head.len(), lost), (HEAD - 10 + TAIL, 3));
    }

    #[test]
    fn a_line_is_shown_without_what_a_terminal_acts_on() {
        assert_eq!(plain("\x1b[32m  ready\x1b[0m in 300 ms\x1b]0;title\x07!\r"), "  ready in 300 ms!");
        assert_eq!(plain("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), "link");
    }

    #[test]
    fn a_long_text_keeps_more_of_its_end() {
        let text = format!("{}{}", "a".repeat(1000), "z".repeat(1000));
        let fitted = fit(&text, 500);
        assert!(fitted.starts_with(&"a".repeat(100)) && fitted.ends_with(&"z".repeat(400)), "{fitted}");
        assert!(fitted.contains("[1500 bytes in between not shown]"), "{fitted}");
    }
}
