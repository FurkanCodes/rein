//! The full output of rein's steps, one at a time, on a screen of its own
//! (Ctrl-O). Steps only show a few lines in the conversation; this shows
//! all of them.
//!
//! ```text
//! ╭─ run  cargo test · ✗ exit 101 · 212 lines ─────────── 3 of 5 ─╮
//! │    Compiling parser v0.3.1
//! │ …
//! ╰───────────────────────────────────────────────────────────────╯
//!  lines 1-21 of 212 · ↑↓ scroll · ←→ other steps · / search · esc back
//! ```
//!
//! A background command's output goes on coming while it's open: it's
//! given again ([`Viewer::update`]), and at its end the view follows it.
//!
//! Nothing here touches the terminal: keys go in, `render` gives the
//! whole screen as text.

use super::keys::Key;
use super::style::{Style, width};

/// One step's output.
#[derive(Clone)]
pub struct Entry {
    /// What the step was, like `run  cargo test · ✗ exit 101`.
    pub title: String,
    pub lines: Vec<String>,
    /// A background command's name: its entry is found again by it when
    /// the entries are given again.
    command: Option<String>,
    /// It's still running: what it prints next comes in while it's shown.
    live: bool,
}

impl Entry {
    pub fn new(title: String, output: &str) -> Entry {
        Entry { title, lines: output.lines().map(|line| line.replace('\t', "    ")).collect(), command: None, live: false }
    }

    /// The background command `id`'s output, `live` while it runs.
    pub fn command(id: String, title: String, output: &str, live: bool) -> Entry {
        Entry { command: Some(id), live, ..Entry::new(title, output) }
    }
}

pub struct Viewer {
    entries: Vec<Entry>,
    /// Which entry is shown.
    index: usize,
    /// The first line on screen.
    top: usize,
    /// Typing a search, after `/`.
    typing: Option<String>,
    /// The last search, for `n`.
    query: String,
    /// A note for the footer, like "not found".
    note: String,
    /// Lines of text that fit on screen, from the last render.
    page: usize,
}

impl Viewer {
    /// Opens on the latest entry: at its end, if it's still running.
    pub fn new(entries: Vec<Entry>) -> Viewer {
        let index = entries.len().saturating_sub(1);
        let mut viewer = Viewer { entries, index, top: 0, typing: None, query: String::new(), note: String::new(), page: 20 };
        viewer.switch(index);
        viewer
    }

    /// The entries, as they are now: the one shown stays shown, where you
    /// scrolled it, or at its end, which follows what it prints.
    pub fn update(&mut self, entries: Vec<Entry>) {
        let following = self.following();
        let shown = self.entries.get(self.index).and_then(|entry| entry.command.clone());
        let found = shown.and_then(|id| entries.iter().position(|entry| entry.command.as_ref() == Some(&id)));
        self.entries = entries;
        self.index = found.unwrap_or(self.index).min(self.entries.len().saturating_sub(1));
        if following {
            // At the end, whatever fits: `render` knows how much.
            self.top = usize::MAX;
        }
    }

    /// The entry shown is running, and its end is on screen.
    fn following(&self) -> bool {
        self.entries.get(self.index).is_some_and(|entry| entry.live) && self.top >= self.last_top()
    }

    fn lines(&self) -> &[String] {
        self.entries.get(self.index).map_or(&[], |entry| &entry.lines)
    }

    fn last_top(&self) -> usize {
        self.lines().len().saturating_sub(self.page)
    }

    /// Takes a key; `false` means close the viewer.
    pub fn key(&mut self, key: &Key) -> bool {
        self.note.clear();
        if let Some(query) = &mut self.typing {
            match key {
                Key::Char(c) => query.push(*c),
                Key::Paste(text) => query.push_str(text.lines().next().unwrap_or_default()),
                Key::Backspace => {
                    query.pop();
                }
                Key::Enter => {
                    self.query = self.typing.take().unwrap_or_default();
                    self.find(self.top);
                }
                Key::Esc | Key::CtrlC => self.typing = None,
                _ => {}
            }
            return true;
        }
        match key {
            Key::Esc | Key::CtrlO | Key::CtrlC | Key::Char('q') => return false,
            Key::Up | Key::Char('k') => self.top = self.top.saturating_sub(1),
            Key::Down | Key::Char('j') | Key::Enter => self.top = (self.top + 1).min(self.last_top()),
            Key::PageUp | Key::Char('b') => self.top = self.top.saturating_sub(self.page),
            Key::PageDown | Key::Char(' ') => self.top = (self.top + self.page).min(self.last_top()),
            Key::Home | Key::Char('g') => self.top = 0,
            Key::End | Key::Char('G') => self.top = self.last_top(),
            Key::Left | Key::Char('h') if self.index > 0 => self.switch(self.index - 1),
            Key::Right | Key::Char('l') if self.index + 1 < self.entries.len() => self.switch(self.index + 1),
            Key::Char('/') => self.typing = Some(String::new()),
            Key::Char('n') if !self.query.is_empty() => self.find(self.top + 1),
            _ => {}
        }
        true
    }

    /// Shows entry `index`, from its start, or at its end if it's running.
    fn switch(&mut self, index: usize) {
        self.index = index;
        let live = self.entries.get(index).is_some_and(|entry| entry.live);
        self.top = if live { usize::MAX } else { 0 };
    }

    /// Scrolls to the first line from `from` that contains the query,
    /// wrapping around to the start.
    fn find(&mut self, from: usize) {
        if self.query.is_empty() {
            return;
        }
        let query = self.query.to_lowercase();
        let lines = self.lines();
        let hit = (0..lines.len()).map(|i| (from + i) % lines.len()).find(|&i| lines[i].to_lowercase().contains(&query));
        match hit {
            // Near the end, the match is shown lower down instead.
            Some(line) => self.top = line.min(self.last_top()),
            None => self.note = format!("“{}” not found", self.query),
        }
    }

    /// The whole screen, `rows` × `cols`, as text with cursor moves.
    pub fn render(&mut self, rows: usize, cols: usize, style: Style) -> String {
        let s = style;
        self.page = rows.saturating_sub(3).max(1);
        // Past the end means at it.
        self.top = self.top.min(self.last_top());
        let inner = cols.saturating_sub(4).max(10);
        let mut out = String::from("\x1b[H\x1b[2J");

        // Top border, with the step and where it is among the others.
        let title = self.entries.get(self.index).map_or("", |entry| entry.title.as_str());
        let count = self.lines().len();
        // A read's title already says how many lines it has.
        let head =
            if title.ends_with(" lines") || title.ends_with(" line") { format!(" {title} ") } else { format!(" {title} · {count} line{} ", if count == 1 { "" } else { "s" }) };
        let place = format!(" {} of {} ", self.index + 1, self.entries.len());
        let room = (inner + 2).saturating_sub(1 + width(&place));
        let head = cut(&head, room);
        let fill = (inner + 2).saturating_sub(1 + width(&head) + width(&place));
        out += &format!("{}{}{}{}{}", s.blue("╭─"), s.bold(&s.blue(&head)), s.blue(&"─".repeat(fill)), s.dim(&place), s.blue("╮"));

        // The text, with matches of the search in yellow.
        let query = self.query.to_lowercase();
        for row in 0..self.page {
            let line = self.lines().get(self.top + row).map(String::as_str).unwrap_or_default();
            let mut text = cut(line, inner);
            if !query.is_empty() && text.to_lowercase().contains(&query) {
                text = s.yellow(&text);
            }
            out += &format!("\x1b[{};1H{} {text}", row + 2, s.blue("│"));
        }
        out += &format!("\x1b[{};1H{}", self.page + 2, s.blue(&format!("╰{}╯", "─".repeat(inner + 2))));

        // The footer: where you are, the keys, or the search being typed.
        let footer = match &self.typing {
            Some(query) => format!(" search: {query}"),
            None => {
                let last = (self.top + self.page).min(count);
                let mut parts = vec![format!("lines {}-{last} of {count}", if count == 0 { 0 } else { self.top + 1 })];
                if !self.note.is_empty() {
                    parts.push(self.note.clone());
                }
                if self.entries.get(self.index).is_some_and(|entry| entry.live) {
                    parts.push(if self.following() { "following what it prints" } else { "G follows what it prints" }.to_owned());
                }
                parts.extend(["↑↓ scroll", "←→ other steps", "/ search", "esc back"].map(str::to_owned));
                s.dim(&format!(" {}", parts.join(" · ")))
            }
        };
        out += &format!("\x1b[{rows};1H{footer}");
        if self.typing.is_some() {
            out += "\x1b[?25h";
        } else {
            out += "\x1b[?25l";
        }
        out
    }
}

/// `text` cut to `max` columns, with `…` if anything was cut.
fn cut(text: &str, max: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(text) <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewer() -> Viewer {
        let long: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let mut v = Viewer::new(vec![Entry::new("read  a.txt".into(), "short"), Entry::new("run  cargo test · ✗ exit 1".into(), &long)]);
        v.render(12, 60, Style::plain());
        v
    }

    fn screen(v: &mut Viewer) -> String {
        super::super::style::strip(&v.render(12, 60, Style::plain()))
    }

    #[test]
    fn opens_on_the_latest_step_and_scrolls() {
        let mut v = viewer();
        let shown = screen(&mut v);
        assert!(shown.contains("run  cargo test · ✗ exit 1 · 100 lines"), "{shown}");
        assert!(shown.contains("2 of 2") && shown.contains("lines 1-9 of 100"));
        v.key(&Key::PageDown);
        assert!(screen(&mut v).contains("lines 10-18 of 100"));
        v.key(&Key::End);
        assert!(screen(&mut v).contains("lines 92-100 of 100"));
        v.key(&Key::Down);
        assert!(screen(&mut v).contains("lines 92-100 of 100"), "can't scroll past the end");
        v.key(&Key::Home);
        assert!(screen(&mut v).contains("lines 1-9 of 100"));
    }

    #[test]
    fn moves_between_steps() {
        let mut v = viewer();
        v.key(&Key::Left);
        let shown = screen(&mut v);
        assert!(shown.contains("read  a.txt · 1 line") && shown.contains("1 of 2"));
        v.key(&Key::Left);
        assert!(screen(&mut v).contains("1 of 2"), "stays on the first");
        v.key(&Key::Right);
        assert!(screen(&mut v).contains("2 of 2"));
    }

    #[test]
    fn searches_and_closes() {
        let mut v = viewer();
        for key in [Key::Char('/'), Key::Char('5'), Key::Char('0'), Key::Enter] {
            assert!(v.key(&key));
        }
        assert!(screen(&mut v).contains("lines 50-58 of 100"));
        v.key(&Key::Char('n'));
        assert!(screen(&mut v).contains("lines 50-58 of 100"), "the only match: n wraps around to it");
        for key in [Key::Char('/'), Key::Char('9'), Key::Char('9'), Key::Enter] {
            v.key(&key);
        }
        assert!(screen(&mut v).contains("lines 92-100 of 100"), "a match near the end");
        for key in [Key::Char('/'), Key::Char('z'), Key::Enter] {
            v.key(&key);
        }
        assert!(screen(&mut v).contains("“z” not found"));
        assert!(!v.key(&Key::Esc));
        assert!(!viewer().key(&Key::CtrlO));
    }

    fn printed(n: usize) -> String {
        (1..=n).map(|n| format!("line {n}\n")).collect()
    }

    #[test]
    fn a_running_command_opens_at_its_end_and_follows_what_it_prints() {
        let step = Entry::new("read  a.txt".into(), "short");
        let mut v = Viewer::new(vec![step.clone(), Entry::command("b1".into(), "b1  gh run watch · running".into(), &printed(30), true)]);
        let shown = screen(&mut v);
        assert!(shown.contains("lines 22-30 of 30") && shown.contains("following what it prints"), "{shown}");
        v.update(vec![step.clone(), Entry::command("b1".into(), "b1  gh run watch · running".into(), &printed(40), true)]);
        let shown = screen(&mut v);
        assert!(shown.contains("line 40") && shown.contains("lines 32-40 of 40"), "{shown}");
        // Scrolled up, it stays where you are.
        v.key(&Key::PageUp);
        v.update(vec![step.clone(), Entry::command("b1".into(), "b1  gh run watch · running".into(), &printed(50), true)]);
        let shown = screen(&mut v);
        assert!(shown.contains("lines 23-31 of 50") && shown.contains("G follows what it prints"), "{shown}");
        v.key(&Key::End);
        assert!(screen(&mut v).contains("following what it prints"));
        // A step that ends meanwhile goes before it: it's still the one shown.
        let ended = Entry::command("b1".into(), "b1  gh run watch · exit 1".into(), &printed(51), false);
        v.update(vec![step.clone(), Entry::new("run  ls".into(), "a"), ended]);
        let shown = screen(&mut v);
        assert!(shown.contains("b1  gh run watch · exit 1") && shown.contains("3 of 3") && shown.contains("lines 43-51 of 51"), "{shown}");
        assert!(!shown.contains("follow"), "it's over: {shown}");
    }
}
