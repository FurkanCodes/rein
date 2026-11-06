//! Turning the bytes a terminal sends into keys.
//!
//! In raw mode every key arrives as bytes: letters as themselves (UTF-8
//! for anything beyond ASCII), Ctrl-C as byte 3, arrows as escape
//! sequences like `ESC [ A`. Pasted text comes wrapped in `ESC [ 200~` and
//! `ESC [ 201~`, so its newlines aren't taken for Enter.
//!
//! Terminals that speak the kitty keyboard protocol, which rein asks for,
//! spell out keys pressed with Shift, Alt or Ctrl as `ESC [ code ; mods u`,
//! so Shift-Enter can mean a new line, and send Esc as `ESC [ 27 u`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Tab,
    Enter,
    /// A newline in the message: Shift-Enter, Alt-Enter or Ctrl-J.
    Newline,
    /// Shift-Tab.
    BackTab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// A word to the left or right: Alt or Ctrl with an arrow.
    WordLeft,
    WordRight,
    PageUp,
    PageDown,
    /// Shift-↑ and Shift-↓: the conversation, a line at a time.
    ScrollUp,
    ScrollDown,
    /// The mouse, as the terminal reports it once asked: which button
    /// (64 and 65 are the wheel; 32 is added while dragging), where, from
    /// 1, and whether it was let go.
    Mouse {
        button: u16,
        x: usize,
        y: usize,
        up: bool,
    },
    /// Delete the word before the cursor.
    DeleteWord,
    /// Delete everything before the cursor.
    DeleteToStart,
    /// Delete everything after the cursor.
    DeleteToEnd,
    CtrlC,
    CtrlD,
    /// Ctrl-V: an image from the clipboard (text comes as a paste).
    CtrlV,
    CtrlO,
    /// Ctrl-B: a cursor left, or with nothing typed, the running command
    /// goes on in the background.
    CtrlB,
    /// Ctrl-R: find a message sent before.
    CtrlR,
    /// The window came to the front (`true`) or went behind another, as
    /// terminals that are asked report it.
    Focus(bool),
    /// Ctrl-L: draw the screen again.
    Redraw,
    /// The terminal's background colour, as it answered rein's question
    /// ([`super::tint::ASK`]).
    Background(super::tint::Rgb),
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
        0x02 => key(Key::CtrlB, 1),
        0x03 => key(Key::CtrlC, 1),
        0x04 => key(Key::CtrlD, 1),
        0x05 => key(Key::End, 1),
        0x06 => key(Key::Right, 1),
        0x08 | 0x7f => key(Key::Backspace, 1),
        0x0a => key(Key::Newline, 1),
        0x0b => key(Key::DeleteToEnd, 1),
        0x0c => key(Key::Redraw, 1),
        0x0d => key(Key::Enter, 1),
        0x0e => key(Key::Down, 1),
        0x0f => key(Key::CtrlO, 1),
        0x16 => key(Key::CtrlV, 1),
        0x10 => key(Key::Up, 1),
        0x12 => key(Key::CtrlR, 1),
        0x15 => key(Key::DeleteToStart, 1),
        0x17 => key(Key::DeleteWord, 1),
        0x09 => key(Key::Tab, 1),
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
        Some(0x7f | 0x08) => key(Key::DeleteWord, 2),
        Some(b'b') => key(Key::WordLeft, 2),
        Some(b'f') => key(Key::WordRight, 2),
        Some(b']') => answer(bytes),
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
            // With Alt or Ctrl (`1;3`, `1;5`), the side arrows go by words.
            let by_word = modifiers(params).is_some_and(|m| m & (ALT | CTRL) != 0);
            let shifted = modifiers(params) == Some(SHIFT);
            match (params, bytes[end + 2]) {
                (p, b'M' | b'm') if p.starts_with(b"<") => mouse(&p[1..], bytes[end + 2] == b'm').map_or(Decoded::Skip(used), |k| key(k, used)),
                (_, b'A') => key(if shifted { Key::ScrollUp } else { Key::Up }, used),
                (_, b'B') => key(if shifted { Key::ScrollDown } else { Key::Down }, used),
                (_, b'C') => key(if by_word { Key::WordRight } else { Key::Right }, used),
                (_, b'D') => key(if by_word { Key::WordLeft } else { Key::Left }, used),
                (_, b'Z') => key(Key::BackTab, used),
                (b"", b'I') => key(Key::Focus(true), used),
                (b"", b'O') => key(Key::Focus(false), used),
                (_, b'u') => spelled(params).map_or(Decoded::Skip(used), |k| key(k, used)),
                (_, b'H') => key(Key::Home, used),
                (_, b'F') => key(Key::End, used),
                (b"1" | b"7", b'~') => key(Key::Home, used),
                (b"4" | b"8", b'~') => key(Key::End, used),
                (b"3", b'~') => key(Key::Delete, used),
                (b"5", b'~') => key(Key::PageUp, used),
                (b"6", b'~') => key(Key::PageDown, used),
                (b"200", b'~') => Decoded::PasteStart(used),
                // xterm's way of spelling out a key: `27 ; mods ; code ~`.
                (p, b'~') if p.starts_with(b"27;") => {
                    let rest = std::str::from_utf8(&p[3..]).unwrap_or_default();
                    let (mods, code) = rest.split_once(';').unwrap_or_default();
                    spelled(format!("{code};{mods}").as_bytes()).map_or(Decoded::Skip(used), |k| key(k, used))
                }
                _ => Decoded::Skip(used),
            }
        }
        // Alt plus a key: not used.
        Some(_) => Decoded::Skip(2),
    }
}

/// The longest answer rein waits for: `11;rgb:ffff/ffff/ffff` and room.
const ANSWER_MOST: usize = 40;

/// What the terminal answered after `ESC ]`, up to BEL or `ESC \`: the
/// background colour rein asked for. What can't be part of one, like
/// Alt-] and the keys typed after it, is left for the keys it is, rather
/// than held until an end that never comes.
fn answer(bytes: &[u8]) -> Decoded {
    let body = &bytes[2..];
    let end = body.iter().position(|&b| b == 0x07 || b == 0x1b);
    let looks = |part: &[u8]| part.first().is_none_or(u8::is_ascii_digit) && part.iter().all(|&b| b.is_ascii_graphic());
    match end {
        Some(at) if body[at] == 0x07 => found(&body[..at], at + 3),
        Some(at) => match body.get(at + 1) {
            None if looks(&body[..at]) => Decoded::Incomplete,
            Some(b'\\') => found(&body[..at], at + 4),
            _ => Decoded::Skip(2),
        },
        None if looks(body) && body.len() < ANSWER_MOST => Decoded::Incomplete,
        None => Decoded::Skip(2),
    }
}

fn found(answer: &[u8], used: usize) -> Decoded {
    super::tint::parse_answer(answer).map_or(Decoded::Skip(used), |rgb| Decoded::Key(Key::Background(rgb), used))
}

/// `button ; x ; y`, from `ESC [ < … M` (pressed) or `m` (let go).
fn mouse(params: &[u8], up: bool) -> Option<Key> {
    let text = std::str::from_utf8(params).ok()?;
    let mut parts = text.split(';').map(str::parse::<usize>);
    let (button, x, y) = (parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?);
    Some(Key::Mouse { button: u16::try_from(button).ok()?, x, y, up })
}

const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CTRL: u32 = 4;

/// The modifier keys in `1;5`-style parameters, as bits: Shift 1, Alt 2,
/// Ctrl 4. Caps Lock and Num Lock don't count.
fn modifiers(params: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(params).ok()?;
    let (_, mods) = text.split_once(';')?;
    let mods: u32 = mods.split(':').next()?.parse().ok()?;
    Some(mods.saturating_sub(1) & !(64 | 128))
}

/// A key the kitty protocol spelled out: `code ; mods`, where `code` is
/// the key's character.
fn spelled(params: &[u8]) -> Option<Key> {
    let text = std::str::from_utf8(params).ok()?;
    let code: u32 = text.split([';', ':']).next()?.parse().ok()?;
    let mods = modifiers(params).unwrap_or(0);
    let (shift, alt, ctrl) = (mods & SHIFT != 0, mods & ALT != 0, mods & CTRL != 0);
    Some(match code {
        13 if shift || alt || ctrl => Key::Newline,
        // The keypad's Enter has a code of its own.
        13 | 57414 => Key::Enter,
        27 => Key::Esc,
        9 if shift => Key::BackTab,
        9 => Key::Tab,
        8 | 127 if alt || ctrl => Key::DeleteWord,
        8 | 127 => Key::Backspace,
        _ => {
            let c = char::from_u32(code)?;
            match (ctrl, alt, c.to_ascii_lowercase()) {
                (true, false, 'a') => Key::Home,
                (true, false, 'b') => Key::CtrlB,
                (true, false, 'c') => Key::CtrlC,
                (true, false, 'd') => Key::CtrlD,
                (true, false, 'e') => Key::End,
                (true, false, 'f') => Key::Right,
                (true, false, 'h') => Key::Backspace,
                (true, false, 'j') => Key::Newline,
                (true, false, 'k') => Key::DeleteToEnd,
                (true, false, 'l') => Key::Redraw,
                (true, false, 'n') => Key::Down,
                (true, false, 'o') => Key::CtrlO,
                (true, false, 'v') => Key::CtrlV,
                (true, false, 'p') => Key::Up,
                (true, false, 'r') => Key::CtrlR,
                (true, false, 'u') => Key::DeleteToStart,
                (true, false, 'w') => Key::DeleteWord,
                (false, true, 'b') => Key::WordLeft,
                (false, true, 'f') => Key::WordRight,
                (false, false, _) if !c.is_control() => Key::Char(if shift { c.to_uppercase().next()? } else { c }),
                _ => return None,
            }
        }
    })
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
        assert_eq!(keys(b"\x1b[99Xx"), [Key::Char('x')], "unknown sequences are skipped");
        assert_eq!(keys(b"\x1b[4;12Rx"), [Key::Char('x')], "a cursor report isn't typing");
    }

    #[test]
    fn reads_keys_the_kitty_protocol_spells_out() {
        assert_eq!(keys(b"\x1b[13;2u\x1b[13u"), [Key::Newline, Key::Enter], "Shift-Enter is a new line");
        assert_eq!(keys(b"\x1b[27u\x1b[99;5u\x1b[100;5u"), [Key::Esc, Key::CtrlC, Key::CtrlD]);
        assert_eq!(keys(b"\x1b[111;5u\x1b[106;5u\x1b[9;2u"), [Key::CtrlO, Key::Newline, Key::BackTab]);
        assert_eq!(keys(b"\x1b[127;3u\x1b[99;69u"), [Key::DeleteWord, Key::CtrlC], "Caps Lock doesn't change Ctrl-C");
        assert_eq!(keys(b"\x1b[27;2;13~"), [Key::Newline], "xterm spells Shift-Enter this way");
        assert_eq!(keys(b"\x1b[1;3D\x1b[1;5C\x1bb\x1b[1;2D"), [Key::WordLeft, Key::WordRight, Key::WordLeft, Key::Left]);
        assert_eq!(keys(b"\x1b[Z"), [Key::BackTab]);
        assert_eq!(keys(b"\x12\x1b[114;5u"), [Key::CtrlR, Key::CtrlR]);
        assert_eq!(keys(b"\x16\x1b[118;5u"), [Key::CtrlV, Key::CtrlV], "as a byte, and as the kitty protocol says it");
    }

    #[test]
    fn reads_the_window_coming_and_going() {
        assert_eq!(keys(b"\x1b[O\x1b[Ix"), [Key::Focus(false), Key::Focus(true), Key::Char('x')]);
        assert_eq!(keys(b"\x1bOH"), [Key::Home], "not to be mistaken for the Home key's spelling");
    }

    #[test]
    fn reads_the_terminals_background_and_nothing_typed_after_alt_bracket() {
        assert_eq!(keys(b"\x1b]11;rgb:1515/1717/1c1c\x07x"), [Key::Background((21, 23, 28)), Key::Char('x')], "ended by BEL");
        assert_eq!(keys(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"), [Key::Background((255, 255, 255))], "ended by ESC \\");
        let mut k = Keys::default();
        assert_eq!(k.feed(b"\x1b]11;rgb:15"), [], "the rest is on its way");
        assert_eq!(k.feed(b"15/1717/1c1c\x1b"), []);
        assert_eq!(k.feed(b"\\"), [Key::Background((21, 23, 28))]);
        assert_eq!(keys(b"\x1b]10;rgb:0/0/0\x07ok"), [Key::Char('o'), Key::Char('k')], "an answer rein didn't ask for");
        // Alt-] then words: the words are typed, not held as an answer.
        assert_eq!(keys(b"\x1b]hi"), [Key::Char('h'), Key::Char('i')]);
        assert_eq!(keys(b"\x1b]1 a"), [Key::Char('1'), Key::Char(' '), Key::Char('a')]);
    }

    #[test]
    fn reads_the_mouse_and_scrolling_keys() {
        assert_eq!(keys(b"\x1b[<64;10;5M"), [Key::Mouse { button: 64, x: 10, y: 5, up: false }], "the wheel, up");
        assert_eq!(
            keys(b"\x1b[<0;3;4M\x1b[<32;6;4M\x1b[<0;6;4m"),
            [Key::Mouse { button: 0, x: 3, y: 4, up: false }, Key::Mouse { button: 32, x: 6, y: 4, up: false }, Key::Mouse { button: 0, x: 6, y: 4, up: true },]
        );
        assert_eq!(keys(b"\x1b[1;2A\x1b[1;2B\x1b[A"), [Key::ScrollUp, Key::ScrollDown, Key::Up]);
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
