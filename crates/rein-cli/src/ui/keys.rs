//! Turning the bytes a terminal sends into keys.
//!
//! In raw mode every key arrives as bytes: letters as themselves (UTF-8
//! for anything beyond ASCII), Ctrl-C as byte 3, arrows as escape
//! sequences like `ESC [ A`. Pasted text comes wrapped in `ESC [ 200~` and
//! `ESC [ 201~`, so its newlines aren't taken for Enter.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    /// A newline in the message: Ctrl-J, or Alt-Enter.
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Delete the word before the cursor.
    DeleteWord,
    /// Delete everything before the cursor.
    DeleteToStart,
    /// Delete everything after the cursor.
    DeleteToEnd,
    CtrlC,
    CtrlD,
    CtrlO,
    Esc,
    Paste(String),
}

#[derive(Default)]
pub struct Keys {
    pending: Vec<u8>,
    /// Inside a paste: everything until the end marker is text.
    paste: Option<Vec<u8>>,
}

const PASTE_END: &[u8] = b"\x1b[201~";

impl Keys {
    /// Takes the bytes that arrived and returns the keys they make. An
    /// incomplete sequence at the end waits for the next bytes, except a
    /// lone Esc, which is the Esc key.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            let rest = &self.pending[at..];
            if let Some(paste) = &mut self.paste {
                match find(rest, PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&rest[..end]);
                        let text = String::from_utf8_lossy(paste).replace("\r\n", "\n").replace('\r', "\n");
                        keys.push(Key::Paste(text));
                        self.paste = None;
                        at += end + PASTE_END.len();
                        continue;
                    }
                    None => {
                        // Keep a possible start of the end marker for later.
                        let keep = (1..PASTE_END.len()).rev().find(|&n| rest.ends_with(&PASTE_END[..n])).unwrap_or(0);
                        paste.extend_from_slice(&rest[..rest.len() - keep]);
                        at = self.pending.len() - keep;
                        break;
                    }
                }
            }
            match decode(rest) {
                Decoded::Key(key, used) => {
                    keys.push(key);
                    at += used;
                }
                Decoded::PasteStart(used) => {
                    self.paste = Some(Vec::new());
                    at += used;
                }
                Decoded::Skip(used) => at += used,
                Decoded::Incomplete => break,
            }
        }
        self.pending.drain(..at);
        if self.pending == [0x1b] && self.paste.is_none() {
            self.pending.clear();
            keys.push(Key::Esc);
        }
        keys
    }
}

enum Decoded {
    Key(Key, usize),
    PasteStart(usize),
    /// Bytes that mean nothing to rein, like an unknown escape sequence.
    Skip(usize),
    Incomplete,
}

fn decode(bytes: &[u8]) -> Decoded {
    let key = |key, used| Decoded::Key(key, used);
    match bytes[0] {
        0x1b => escape(bytes),
        0x01 => key(Key::Home, 1),
        0x02 => key(Key::Left, 1),
        0x03 => key(Key::CtrlC, 1),
        0x04 => key(Key::CtrlD, 1),
        0x05 => key(Key::End, 1),
        0x06 => key(Key::Right, 1),
        0x08 | 0x7f => key(Key::Backspace, 1),
        0x0a => key(Key::Newline, 1),
        0x0b => key(Key::DeleteToEnd, 1),
        0x0d => key(Key::Enter, 1),
        0x0e => key(Key::Down, 1),
        0x0f => key(Key::CtrlO, 1),
        0x10 => key(Key::Up, 1),
        0x15 => key(Key::DeleteToStart, 1),
        0x17 => key(Key::DeleteWord, 1),
        0x09 => key(Key::Char(' '), 1),
        b if b < 0x20 => Decoded::Skip(1),
        b => {
            let len = match b {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Decoded::Skip(1),
            };
            if bytes.len() < len {
                return Decoded::Incomplete;
            }
            match std::str::from_utf8(&bytes[..len]).ok().and_then(|s| s.chars().next()) {
                Some(c) => key(Key::Char(c), len),
                None => Decoded::Skip(1),
            }
        }
    }
}

fn escape(bytes: &[u8]) -> Decoded {
    let key = |key, used| Decoded::Key(key, used);
    match bytes.get(1) {
        None => Decoded::Incomplete,
        Some(b'\r') => key(Key::Newline, 2),
        Some(0x7f) => key(Key::DeleteWord, 2),
        Some(b'O') => match bytes.get(2) {
            None => Decoded::Incomplete,
            Some(b'H') => key(Key::Home, 3),
            Some(b'F') => key(Key::End, 3),
            Some(_) => Decoded::Skip(3),
        },
        Some(b'[') => {
            // ESC [ then parameters, ended by a byte from @ to ~.
            let Some(end) = bytes[2..].iter().position(|b| (0x40..=0x7e).contains(b)) else {
                return Decoded::Incomplete;
            };
            let used = end + 3;
            let params = &bytes[2..end + 2];
            match (params, bytes[end + 2]) {
                (_, b'A') => key(Key::Up, used),
                (_, b'B') => key(Key::Down, used),
                (_, b'C') => key(Key::Right, used),
                (_, b'D') => key(Key::Left, used),
                (_, b'H') => key(Key::Home, used),
                (_, b'F') => key(Key::End, used),
                (b"1" | b"7", b'~') => key(Key::Home, used),
                (b"4" | b"8", b'~') => key(Key::End, used),
                (b"3", b'~') => key(Key::Delete, used),
                (b"200", b'~') => Decoded::PasteStart(used),
                _ => Decoded::Skip(used),
            }
        }
        // Alt plus a key: not used.
        Some(_) => Decoded::Skip(2),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        Keys::default().feed(bytes)
    }

    #[test]
    fn decodes_letters_controls_and_arrows() {
        assert_eq!(keys("hé\r".as_bytes()), [Key::Char('h'), Key::Char('é'), Key::Enter]);
        assert_eq!(keys(b"\x03\x04\x7f\x17"), [Key::CtrlC, Key::CtrlD, Key::Backspace, Key::DeleteWord]);
        assert_eq!(keys(b"\x1b[A\x1b[D\x1b[3~\x1b[H\x1bOF"), [Key::Up, Key::Left, Key::Delete, Key::Home, Key::End]);
        assert_eq!(keys(b"\x1b"), [Key::Esc]);
        assert_eq!(keys(b"\x1b[99Zx"), [Key::Char('x')], "unknown sequences are skipped");
        assert_eq!(keys(b"\x1b[4;1Rx"), [Key::Char('x')], "a late cursor report isn't typing");
    }

    #[test]
    fn waits_for_the_rest_of_a_sequence() {
        let mut k = Keys::default();
        assert_eq!(k.feed(b"\x1b["), []);
        assert_eq!(k.feed(b"B"), [Key::Down]);
        let euro = "€".as_bytes();
        assert_eq!(k.feed(&euro[..1]), []);
        assert_eq!(k.feed(&euro[1..]), [Key::Char('€')]);
    }

    #[test]
    fn pasted_text_is_text() {
        assert_eq!(keys(b"\x1b[200~line one\r\nline two\x1b[201~\r"), [Key::Paste("line one\nline two".into()), Key::Enter]);
        let mut k = Keys::default();
        assert_eq!(k.feed(b"\x1b[200~abc\x1b[20"), []);
        assert_eq!(k.feed(b"1~"), [Key::Paste("abc".into())]);
    }
}
