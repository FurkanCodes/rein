//! The keyboard, and the live part of the screen it drives: the input box,
//! what rein is doing above it, its questions, and the commands that match
//! what you type under it.
//!
//! ```text
//!        ◐ Running cargo test… (4s · esc to stop)
//!        ↳ steering: use the helper · goes in at the next step   ← enter
//!        ↳ after this turn: and then commit it                    ← tab
//! ╭──────────────────────────────────────────────╮
//! │ › Now add a test                              │
//! ╰──────────────────────────────────────────────╯
//!  gpt-6.1-sol · ask · context ▰▱▱▱▱▱▱▱▱▱ 4% · cache 86%
//! ```
//!
//! The keyboard is read by its own thread, so that waiting for a key never
//! stops rein from noticing Ctrl-C. In a terminal it drives the box;
//! otherwise it reads plain lines, so piping into rein works.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::commands;
use crate::ui::editor::{Action, Editor, Entry};
use crate::ui::fuzzy;
use crate::ui::history::{self, History};
use crate::ui::keys::{Key, Keys};
use crate::ui::narrow::{self, Query, Rank};
use crate::ui::screen::{self, Bottom, Drag, Pinned, Raw, Tone};
use crate::ui::slash::{self, Entry as Listed, Offer};
use crate::ui::style::{GUTTER, Style, fit, width};
use crate::ui::viewer::Viewer;
use crate::ui::wrap::Wrap;
use rein_agent::sandbox::Sandbox;

/// What reading a line produced.
pub enum Read {
    Line(String),
    /// Ctrl-C while typing. In a real terminal the box turns it into a
    /// keypress, so it arrives here instead of as a signal.
    Interrupted,
    /// The item of a menu that was picked, by its place in the list.
    Picked(usize),
    /// Ctrl-D, or input ended.
    Eof,
    /// Shift-Tab: on to the next mode.
    NextMode,
    /// Ctrl-V: an image from the clipboard, to go with the message.
    Paste,
}

/// What the keyboard is for right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Waiting {
    /// rein is working; keys type ahead, Enter queues what's typed, and
    /// Ctrl-C or Esc stop the turn.
    Nothing,
    Message,
    /// The answer to a question.
    Answer,
}

/// A question with a few answers, picked with its key, a digit, or the
/// arrows and Enter.
pub struct Question {
    pub title: String,
    /// A line under the title: why the model wants it, say.
    pub detail: Option<String>,
    /// The command it's about, shown whole under the title.
    pub command: Option<String>,
    /// Each answer: the key that picks it (`esc` for Esc), and what it says.
    pub answers: Vec<(&'static str, String)>,
}

/// A list to choose one from: ↑↓ move, typing narrows it, Enter picks,
/// Esc cancels. Every command that shows choices shows them this way.
pub struct Menu {
    /// What it's for, on the box's edge: `choose a model`.
    pub title: String,
    /// Each choice: its name, and a few dim words about it.
    pub items: Vec<(String, String)>,
    /// The one in use: marked, and where the list starts.
    pub current: Option<usize>,
    /// Where the list starts instead, unmarked: the item picked last in a
    /// menu shown again, like one whose items turn on and off.
    pub at: Option<usize>,
}

/// A menu on screen: what's typed to narrow it, and which of the matching
/// items is picked.
struct Picking {
    menu: Menu,
    filter: Editor,
    picked: usize,
}

impl Picking {
    /// What's typed to narrow it.
    fn query(&self) -> Query {
        Query::new(&self.filter.text())
    }

    /// The items that match what's typed, best first (see `narrow`); all
    /// of them, in order, before anything is.
    fn matching(&self) -> Vec<usize> {
        let query = self.query();
        if query.is_empty() {
            return (0..self.menu.items.len()).collect();
        }
        let mut ranked: Vec<(Rank, usize)> = self.menu.items.iter().enumerate().filter_map(|(i, (name, about))| Some((query.rank(name, about)?, i))).collect();
        ranked.sort();
        ranked.into_iter().map(|(_, i)| i).collect()
    }
}

/// The most of a menu shown at once; it scrolls for more.
const PICK_LINES: usize = 10;

/// The question on screen, which answer is picked, and since when it's
/// been asked.
struct Asking {
    question: Question,
    picked: usize,
    since: Instant,
}

/// Keys this soon after a question appears, or in a run of typing that
/// hasn't paused this long, still go to the box: someone typing ahead
/// can't answer a question by accident.
const GRACE: Duration = Duration::from_millis(500);

/// How long a note shows in place of the status bar: as long as a second
/// Ctrl-C still leaves.
const NOTICE: Duration = Duration::from_secs(2);

/// The most of your message the box shows at once; it scrolls for more.
const BOX_LINES: usize = 8;

/// The most commands, or messages found, listed under the box at once.
const MENU_LINES: usize = 6;

/// Where the terminal doesn't say whether its window is in front, you're
/// taken to be away once you haven't touched a key for this long.
const QUIET: Duration = Duration::from_secs(10);

/// Ctrl-R: finding a message you sent before.
struct Search {
    /// The words to find, typed and edited like a message.
    query: Editor,
    /// Which of the messages found is picked: 0 is the newest.
    picked: usize,
    /// What was in the box before, put back if you change your mind.
    draft: Entry,
}

const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

pub struct Input {
    /// Plain lines: prompts to show; the thread reads one line for each.
    requests: std::sync::mpsc::Sender<String>,
    replies: tokio::sync::mpsc::UnboundedReceiver<Read>,
    /// A read asked for but not answered yet, kept if the asker gave up.
    pending: bool,
    /// The box, when rein runs in a terminal.
    boxed: Option<Arc<Mutex<Boxed>>>,
    /// Puts the keyboard back when rein ends.
    _raw: Option<Raw>,
    /// The project.
    root: PathBuf,
    /// What it hides, `@` doesn't offer either.
    sandbox: Arc<Sandbox>,
}

/// The box, shared by the keyboard's thread, the clock's and the rest of rein.
struct Boxed {
    pinned: Pinned,
    editor: Editor,
    waiting: Waiting,
    /// What the empty box says while waiting for a typed answer.
    prompt: String,
    /// The answer being typed is a secret, like a key: it shows as dots,
    /// and is never kept.
    secret: bool,
    /// A menu to choose from.
    picking: Option<Picking>,
    asking: Option<Asking>,
    /// What rein is doing, and since when its turn started.
    busy: Option<(String, Instant)>,
    /// Messages typed while rein worked, sent with Enter: they steer the
    /// turn, going in at its next step.
    steering: Vec<String>,
    /// And those sent with Tab, or commands: sent when it's done.
    queued: Vec<String>,
    /// Messages sent with big pastes in them, and how the box showed them,
    /// for the conversation to show the same.
    shown: Vec<(String, String)>,
    /// A note in place of the bar, like "press ctrl-c again to leave".
    notice: Option<(String, Instant)>,
    /// The first of your message's lines in view, when it doesn't all fit.
    scroll: usize,
    /// Which of the matching commands is picked.
    picked: usize,
    /// When the last key came.
    last_key: Instant,
    /// Every step's full output, for the viewer (Ctrl-O).
    outputs: Vec<crate::ui::viewer::Entry>,
    viewer: Option<Viewer>,
    /// Ctrl-R, while it's finding a message.
    search: Option<Search>,
    /// What `@` can point at in the project, files and folders, as last
    /// looked at.
    paths: Vec<String>,
    /// The `@` word Esc closed the list for, or the `/` word, with its `/`.
    dismissed: Option<String>,
    /// The skills you may call, for the list a `/` word opens.
    skills: Vec<Offer>,
    /// Whether the window is in front, once the terminal has said.
    focused: Option<bool>,
    /// When you last pressed a key, scrolled or clicked.
    touched: Instant,
    /// The bell rings when rein needs you and you're away.
    bell: bool,
    /// A question is waiting that the bell hasn't rung for yet: you were
    /// there when it came.
    ring: bool,
    /// rein has worked since it last waited for a message: when it's done,
    /// it's waiting for you.
    worked: bool,
    bar: String,
    replies: tokio::sync::mpsc::UnboundedSender<Read>,
    interrupted: Arc<tokio::sync::Notify>,
}

fn locked(boxed: &Mutex<Boxed>) -> MutexGuard<'_, Boxed> {
    boxed.lock().unwrap_or_else(|p| p.into_inner())
}

impl Boxed {
    /// The viewer, if it's open, or the box, at the window's new size.
    fn redraw(&mut self) {
        match &mut self.viewer {
            Some(viewer) => {
                let (rows, cols) = screen::size();
                let shown = viewer.render(rows, cols, Style::stderr());
                self.pinned.show_viewer(&shown);
            }
            None => self.draw(),
        }
    }

    /// The commands and skills that match what's typed, while the first
    /// word is being typed.
    fn menu(&self) -> Vec<Listed> {
        if self.waiting == Waiting::Answer || self.search.is_some() {
            return Vec::new();
        }
        slash::matching(&self.editor.text(), &self.skills)
    }

    /// The skills that match a `/` word being typed after the first, which
    /// loads one with the message: none once Esc closed the list for it.
    fn later_menu(&self) -> Vec<Listed> {
        if self.waiting == Waiting::Answer || self.search.is_some() {
            return Vec::new();
        }
        match self.editor.word('/') {
            Some((start, typed)) if start > 0 && self.dismissed.as_deref() != Some(&format!("/{typed}")) => slash::later(&typed, &self.skills),
            _ => Vec::new(),
        }
    }

    /// The paths that match the `@` word being typed, best first: none
    /// without one, or once Esc closed the list for it.
    fn mention_menu(&self) -> Vec<String> {
        if self.waiting == Waiting::Answer || self.search.is_some() {
            return Vec::new();
        }
        match self.editor.mention() {
            Some((_, typed)) if self.dismissed.as_ref() != Some(&typed) => fuzzy::best(&typed, &self.paths, 50).into_iter().map(str::to_owned).collect(),
            _ => Vec::new(),
        }
    }

    /// Picks the command or skill named exactly, or else the first.
    fn pick_default(&mut self) {
        let later = self.later_menu();
        let (menu, typed) = match self.editor.word('/') {
            Some((_, typed)) if !later.is_empty() => (later, format!("/{typed}")),
            _ => (self.menu(), self.editor.text()),
        };
        self.picked = menu.iter().position(|entry| entry.name == typed).unwrap_or(0);
    }

    fn draw(&mut self) {
        if self.viewer.is_some() {
            return;
        }
        let s = Style::stderr();
        let pad = " ".repeat(GUTTER);
        let mut above = Vec::new();
        if let Some((what, since)) = &self.busy {
            let elapsed = since.elapsed();
            let frame = SPINNER[(elapsed.as_millis() / 100) as usize % SPINNER.len()];
            let hint = format!("({} · esc to stop)", seconds(elapsed));
            // A long command is cut, so the time and the hint stay in view.
            let room = screen::size().1.saturating_sub(GUTTER + 6 + width(&hint));
            let what = fit(what, room.max(10));
            above.push(format!("{pad}{} {what}… {}", s.yellow(&frame.to_string()), s.dim(&hint)));
        }
        for message in &self.steering {
            let first = message.lines().next().unwrap_or_default();
            above.push(format!("{pad}{} {first} {}", s.dim("↳ steering:"), s.dim("· goes in at the next step")));
        }
        for message in &self.queued {
            let message = commands::shown(message);
            let first = message.lines().next().unwrap_or_default();
            above.push(format!("{pad}{} {first}", s.dim("↳ after this turn:")));
        }

        let bottom = match &self.asking {
            Some(asking) if self.waiting == Waiting::Answer => {
                let mut lines = vec![s.bold(&asking.question.title)];
                // Wrapped between words, as what it says, like a server's
                // walls, may be what decides the answer.
                if let Some(detail) = &asking.question.detail {
                    let mut wrap = crate::ui::wrap::Wrap::new(self.pinned.text_width(), 0, 0);
                    let wrapped = wrap.push(detail) + &wrap.finish();
                    let mut detail: Vec<String> = wrapped.lines().map(|line| s.dim(line)).collect();
                    if detail.len() > DETAIL_LINES {
                        detail.truncate(DETAIL_LINES);
                        if let Some(last) = detail.last_mut() {
                            last.push_str(&s.dim(" …"));
                        }
                    }
                    lines.extend(detail);
                }
                if let Some(command) = &asking.question.command {
                    // Wrapped at the box's width now, which a resize changes.
                    let mut shown = command_lines(command, s, self.pinned.text_width());
                    if shown.len() > COMMAND_LINES {
                        let more = shown.len() - (COMMAND_LINES - 1);
                        shown.truncate(COMMAND_LINES - 1);
                        shown.push(s.dim(&format!("  … {more} more lines, shown above")));
                    }
                    lines.extend(shown);
                    lines.push(String::new());
                }
                // The key first, so a narrow box cuts the words, not the key.
                for (i, (key, text)) in asking.question.answers.iter().enumerate() {
                    let key = format!("{key:<3}");
                    lines.push(if i == asking.picked {
                        format!("  {} {}  {}", s.yellow("›"), s.bold(&s.yellow(&key)), s.bold(text))
                    } else {
                        format!("    {}  {text}", s.dim(&key))
                    });
                }
                Bottom {
                    above,
                    tone: Tone::Asking,
                    mark: s.bold(&s.yellow("?")),
                    lines,
                    caret: None,
                    notes: (String::new(), String::new()),
                    below: vec![self.bar.clone()],
                    compact: {
                        let keys: Vec<&str> = asking.question.answers.iter().map(|(key, _)| *key).collect();
                        format!("{} {}", asking.question.title, keys.join("/"))
                    },
                }
            }
            _ => match &self.picking {
                Some(picking) => self.picker(picking, above),
                None => self.composer(above),
            },
        };
        self.pinned.draw(&bottom);
    }

    /// The box with your message in it, or what you're looking for.
    fn composer(&mut self, above: Vec<String>) -> Bottom {
        let s = Style::stderr();
        if let Some(search) = &self.search {
            let (lines, caret) = search.query.layout(self.pinned.text_width());
            let lines = if search.query.is_empty() { vec![s.dim("type words from a message you sent in this folder")] } else { lines };
            let (tone, mark) = if self.waiting == Waiting::Message { (Tone::Typing, s.bold(&s.blue("›"))) } else { (Tone::Busy, s.dim("›")) };
            let notes = ("finding a message you sent".to_owned(), "enter picks · esc goes back".to_owned());
            let above = [above, self.list()].concat();
            return Bottom { above, tone, mark, lines, caret: Some(caret), notes, below: self.under(), compact: String::new() };
        }
        if self.secret && !self.editor.is_empty() {
            let width = self.pinned.text_width().max(2);
            let dots = self.editor.text().chars().count().min(width - 1);
            let notes = (String::new(), "enter saves it · esc cancels".to_owned());
            return Bottom {
                above,
                tone: Tone::Asking,
                mark: s.bold(&s.yellow("?")),
                lines: vec!["•".repeat(dots)],
                caret: Some((0, dots)),
                notes,
                below: self.under(),
                compact: String::new(),
            };
        }
        let (mut lines, (line, column)) = self.editor.layout(self.pinned.text_width());
        // Keep the line with the cursor in view.
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + BOX_LINES {
            self.scroll = line + 1 - BOX_LINES;
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(BOX_LINES));
        let hidden_below = lines.len().saturating_sub(self.scroll + BOX_LINES);
        let notes = (more(self.scroll, "↑"), more(hidden_below, "↓"));
        lines = lines.into_iter().skip(self.scroll).take(BOX_LINES).collect();
        let tone = match self.waiting {
            Waiting::Message => Tone::Typing,
            Waiting::Answer => Tone::Asking,
            Waiting::Nothing => Tone::Busy,
        };
        if self.editor.is_empty() {
            let placeholder = match self.waiting {
                Waiting::Message => "type a message · / for commands · shift-enter or ctrl-j for a new line",
                Waiting::Answer => self.prompt.as_str(),
                Waiting::Nothing => "type to steer rein · enter sends it at the next step · tab after this turn",
            };
            lines = vec![s.dim(placeholder)];
        }
        let mark = match (self.scroll, tone) {
            (1.., _) => " ".to_owned(),
            (_, Tone::Typing) => s.bold(&s.blue("›")),
            (_, Tone::Asking) => s.bold(&s.yellow("?")),
            (_, Tone::Busy) => s.dim("›"),
        };
        let above = [above, self.list()].concat();
        Bottom { above, tone, mark, lines, caret: Some((line - self.scroll, column)), notes, below: self.under(), compact: String::new() }
    }

    /// Over the box: the messages found, the paths that match an `@` word,
    /// or the commands that match what's typed; nothing else.
    fn list(&self) -> Vec<String> {
        let s = Style::stderr();
        if let Some(search) = &self.search {
            return self.found(search);
        }
        let found = self.mention_menu();
        if !found.is_empty() {
            return upward(found.len(), self.picked, MENU_LINES, s, |i| {
                if i == self.picked { format!(" {} {}", s.blue("›"), s.bold(&s.blue(&found[i]))) } else { format!("   {}", found[i]) }
            });
        }
        let later = self.later_menu();
        let menu = if later.is_empty() { self.menu() } else { later };
        // A long skill's name doesn't push every description out of view.
        let names = menu.iter().map(|entry| width(&entry.name) + 1 + width(&entry.args)).max().unwrap_or(0).min(40);
        let room = screen::size().1.saturating_sub(names + 6).max(10);
        upward(menu.len(), self.picked, MENU_LINES, s, |i| {
            let Listed { name, args, what, .. } = &menu[i];
            let usage = format!("{name} {args}");
            let pad = " ".repeat(names.saturating_sub(width(&usage)));
            let what = fit(what, room);
            if i == self.picked {
                format!(" {} {}{}{pad}  {what}", s.blue("›"), s.bold(&s.blue(name)), s.dim(&format!(" {args}")))
            } else {
                format!("   {name}{}{pad}  {}", s.dim(&format!(" {args}")), s.dim(&what))
            }
        })
    }

    /// Under the box: a note, or the bar.
    fn under(&self) -> Vec<String> {
        match &self.notice {
            Some((note, _)) => vec![format!(" {}", Style::stderr().yellow(note))],
            None => vec![self.bar.clone()],
        }
    }

    fn found(&self, search: &Search) -> Vec<String> {
        let s = Style::stderr();
        let history = self.editor.history();
        let query = search.query.text();
        if history.is_empty() {
            return vec![s.dim("   nothing sent from this folder yet")];
        }
        let found = history.search(&query);
        if found.is_empty() {
            return vec![s.dim(&format!("   nothing you sent here has “{}” in it", query.trim()))];
        }
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let cols = screen::size().1;
        upward(found.len(), search.picked, MENU_LINES, s, |n| {
            let i = found[n];
            let shown = history.get(i).shown().replace('\t', " ");
            let mut rows = shown.trim().lines();
            let top = rows.next().unwrap_or_default();
            let more = match rows.count() {
                0 => String::new(),
                1 => " (+1 line)".to_owned(),
                n => format!(" (+{n} lines)"),
            };
            let age = history::ago(history.sent(i));
            let room = cols.saturating_sub(3 + width(&more) + 2 + width(&age) + 1);
            let text = fit(top, room);
            let pad = " ".repeat(room.saturating_sub(width(&text)));
            let picked = n == search.picked;
            let text = marked(&text, &words, s, picked);
            let lead = if picked { format!(" {} ", s.blue("›")) } else { "   ".to_owned() };
            format!("{lead}{text}{}{pad}  {}", s.dim(&more), s.dim(&age))
        })
    }

    /// A key while finding a message: the words to find, or picking one.
    fn search_key(&mut self, key: Key) {
        let Some(search) = &mut self.search else { return };
        let found = self.editor.history().search(&search.query.text());
        match key {
            Key::Esc | Key::CtrlC => {
                let draft = std::mem::take(&mut search.draft);
                self.search = None;
                self.editor.restore(&draft);
            }
            // It goes in the box, to change or send: nothing is sent
            // that you haven't seen there.
            Key::Enter | Key::Tab => {
                let entry = match found.get(search.picked) {
                    Some(&i) => self.editor.history().get(i).clone(),
                    None => std::mem::take(&mut search.draft),
                };
                self.search = None;
                self.editor.restore(&entry);
            }
            // Older messages are further up the list, over the box.
            Key::Up | Key::CtrlR => search.picked = (search.picked + 1).min(found.len().saturating_sub(1)),
            Key::Down | Key::BackTab => search.picked = search.picked.saturating_sub(1),
            Key::Newline => {}
            Key::Paste(text) => {
                search.query.key(&Key::Paste(text.replace(['\n', '\r'], " ")));
                search.picked = 0;
            }
            key => {
                search.query.key(&key);
                search.picked = 0;
            }
        }
    }

    /// A key while a menu waits for a choice.
    fn pick_key(&mut self, key: Key) {
        let Some(picking) = &mut self.picking else { return };
        let matching = picking.matching();
        let last = matching.len().saturating_sub(1);
        let chosen = match key {
            Key::Esc | Key::CtrlC => Some(Read::Interrupted),
            // Nothing matches: Enter waits for typing that does.
            Key::Enter => matching.get(picking.picked).map(|&i| Read::Picked(i)),
            // The list goes up from the box: up is on down it, to the end
            // and round again.
            Key::Up | Key::Tab => {
                picking.picked = if picking.picked >= last { 0 } else { picking.picked + 1 };
                None
            }
            Key::Down | Key::BackTab => {
                picking.picked = if picking.picked == 0 { last } else { picking.picked - 1 };
                None
            }
            Key::PageUp => {
                picking.picked = (picking.picked + PICK_LINES).min(last);
                None
            }
            Key::PageDown => {
                picking.picked = picking.picked.saturating_sub(PICK_LINES);
                None
            }
            Key::Newline => None,
            Key::Paste(text) => {
                picking.filter.key(&Key::Paste(text.replace(['\n', '\r'], " ")));
                picking.picked = 0;
                None
            }
            key => {
                picking.filter.key(&key);
                picking.picked = 0;
                None
            }
        };
        if let Some(read) = chosen {
            self.picking = None;
            self.reply(read);
        }
    }

    /// The box with a menu: what's typed to narrow it in the box, the
    /// matching items under it.
    fn picker(&self, picking: &Picking, above: Vec<String>) -> Bottom {
        let s = Style::stderr();
        let (lines, caret) = picking.filter.layout(self.pinned.text_width());
        let lines = if picking.filter.is_empty() { vec![s.dim("type to narrow the list, by name or description · ↑↓ to move")] } else { lines };
        let matching = picking.matching();
        let mut list = Vec::new();
        if matching.is_empty() {
            list.push(s.dim(&format!("   nothing matches “{}”", picking.filter.text().trim())));
        }
        let first = (picking.picked + 1).saturating_sub(PICK_LINES);
        let cols = screen::size().1;
        // A long name, like a plugin's skill, doesn't push every other
        // item's description out of a narrow window: only its own.
        let names = matching.iter().skip(first).take(PICK_LINES).map(|&i| width(&picking.menu.items[i].0)).max().unwrap_or(0).min(64).min(cols / 2);
        let query = picking.query();
        list.extend(upward(matching.len(), picking.picked, PICK_LINES, s, |n| {
            let (name, about) = &picking.menu.items[matching[n]];
            let mark = if picking.menu.current == Some(matching[n]) { "•" } else { " " };
            let pad = " ".repeat(names.saturating_sub(width(name)));
            // What matched shows: the name's letters marked, and what's
            // said of it from where it matched, when that's past the room.
            let marks = query.marks(name, about);
            let room = cols.saturating_sub(width(name).max(names) + 7);
            let (about, about_marks) = narrow::around(about, &marks.about, room);
            let line = if n == picking.picked {
                let name = narrow::paint(name, &marks.name, |text| s.bold(&s.blue(text)), |text| s.bold(&s.blue(&s.underline(text))));
                format!(" {} {name}{mark} {pad} {}", s.blue("›"), narrow::paint(&about, &about_marks, str::to_owned, |text| s.bold(text)))
            } else {
                let name = narrow::paint(name, &marks.name, str::to_owned, |text| s.bold(&s.blue(text)));
                format!("   {name}{mark} {pad} {}", narrow::paint(&about, &about_marks, |text| s.dim(text), |text| s.bold(text)))
            };
            fit(&line, cols.saturating_sub(1))
        }));
        let shown = if query.is_empty() { String::new() } else { format!("{} of {} · ", matching.len(), picking.menu.items.len()) };
        let notes = (picking.menu.title.clone(), format!("{shown}enter picks · esc cancels"));
        let above = [above, list].concat();
        Bottom { above, tone: Tone::Typing, mark: s.bold(&s.blue("›")), lines, caret: Some(caret), notes, below: vec![self.bar.clone()], compact: picking.menu.title.clone() }
    }

    /// You're not looking: the terminal says its window is behind another,
    /// or, where it doesn't say, you haven't touched a key for a while.
    fn away(&self) -> bool {
        match self.focused {
            Some(focused) => !focused,
            None => self.touched.elapsed() >= QUIET,
        }
    }

    /// rein needs you: the bell rings if you're away. Says whether it rang.
    fn call(&mut self) -> bool {
        let ring = self.bell && self.away();
        if ring {
            screen::bell();
        }
        ring
    }

    fn key(&mut self, key: Key) {
        if let Key::Focus(focused) = key {
            self.focused = Some(focused);
            return;
        }
        self.touched = Instant::now();
        // The wheel scrolls the viewer's text, when it's open.
        let key = match key {
            Key::Mouse { button, .. } if self.viewer.is_some() && button & 64 != 0 => {
                if button & 1 == 0 {
                    Key::Up
                } else {
                    Key::Down
                }
            }
            Key::Mouse { .. } if self.viewer.is_some() => return,
            key => key,
        };
        if let Some(viewer) = &mut self.viewer {
            let (rows, cols) = screen::size();
            if viewer.key(&key) {
                let shown = viewer.render(rows, cols, Style::stderr());
                self.pinned.show_viewer(&shown);
            } else {
                self.viewer = None;
                self.pinned.close_viewer();
                self.draw();
            }
            return;
        }
        if key == Key::CtrlO && !self.outputs.is_empty() {
            let mut viewer = Viewer::new(self.outputs.clone());
            let (rows, cols) = screen::size();
            let shown = viewer.render(rows, cols, Style::stderr());
            self.pinned.open_viewer(&shown);
            self.viewer = Some(viewer);
            return;
        }
        // A menu takes the keys, but the wheel still scrolls the conversation.
        if self.picking.is_some() && !matches!(key, Key::Mouse { .. } | Key::ScrollUp | Key::ScrollDown | Key::Redraw) {
            self.pick_key(key);
            self.draw();
            return;
        }
        // The conversation scrolls; a drag selects and copies.
        match key {
            Key::Mouse { button, x, y, up } => {
                self.mouse(button, x, y, up);
                return;
            }
            Key::PageUp | Key::PageDown => {
                screen::scroll(if key == Key::PageUp { -screen::page() } else { screen::page() });
                return;
            }
            Key::ScrollUp | Key::ScrollDown => {
                screen::scroll(if key == Key::ScrollUp { -1 } else { 1 });
                return;
            }
            _ => {}
        }
        let paused = std::mem::replace(&mut self.last_key, Instant::now()).elapsed() >= GRACE;
        if key == Key::Redraw {
            // Whatever state the screen is in, print it all again.
            self.pinned.repaint();
            self.redraw();
            return;
        }
        let answering =
            self.waiting == Waiting::Answer && self.asking.as_ref().is_some_and(|asking| (paused && asking.since.elapsed() >= GRACE) || matches!(key, Key::Esc | Key::CtrlC));
        if answering {
            self.answer(&key);
            self.draw();
            return;
        }
        if self.search.is_some() {
            self.search_key(key);
            self.pick_default();
            self.draw();
            return;
        }
        if key == Key::CtrlR && self.asking.is_none() && self.waiting != Waiting::Answer {
            self.search = Some(Search { query: Editor::default(), picked: 0, draft: self.editor.entry() });
            self.notice = None;
            self.draw();
            return;
        }
        // A question too new to take keys: they go to the box, as typing ahead.
        let waiting = if self.waiting == Waiting::Answer && self.asking.is_some() { Waiting::Nothing } else { self.waiting };
        let menu = self.menu();
        let later = self.later_menu();
        let later_pick = later.get(self.picked.min(later.len().saturating_sub(1))).map(|entry| entry.name.clone());
        let later_typed = self.editor.word('/').map(|(_, typed)| format!("/{typed}"));
        let found = self.mention_menu();
        let pick = found.get(self.picked.min(found.len().saturating_sub(1))).cloned();
        let typed = self.editor.mention().map(|(_, typed)| typed);
        // A key that changes the box takes away the note shown before it;
        // a note the key itself brings stays.
        let before = self.notice.take();
        let mut edited = true;
        match (waiting, &key) {
            // The paths that match an `@` word: Esc closes the list, the
            // arrows pick, Tab puts the path in; so does Enter, unless it's
            // typed out already, and then Enter sends.
            (_, Key::Esc) if !found.is_empty() => {
                self.dismissed = typed;
                edited = false;
            }
            (_, Key::Up) if !found.is_empty() => {
                self.picked = (self.picked + 1) % found.len();
                edited = false;
            }
            (_, Key::Down | Key::BackTab) if !found.is_empty() => {
                self.picked = (self.picked + found.len() - 1) % found.len();
                edited = false;
            }
            (_, Key::Tab) if !found.is_empty() => self.editor.complete_mention(&pick.unwrap_or_default()),
            (_, Key::Enter) if !found.is_empty() && pick != typed => self.editor.complete_mention(&pick.unwrap_or_default()),
            // So do the skills that match a later `/` word.
            (_, Key::Esc) if !later.is_empty() => {
                self.dismissed = later_typed;
                edited = false;
            }
            (_, Key::Up) if !later.is_empty() => {
                self.picked = (self.picked + 1) % later.len();
                edited = false;
            }
            (_, Key::Down | Key::BackTab) if !later.is_empty() => {
                self.picked = (self.picked + later.len() - 1) % later.len();
                edited = false;
            }
            (_, Key::Tab) if !later.is_empty() => self.editor.complete_slash(later_pick.unwrap_or_default().trim_start_matches('/')),
            (_, Key::Enter) if !later.is_empty() && later_pick != later_typed => self.editor.complete_slash(later_pick.unwrap_or_default().trim_start_matches('/')),
            (Waiting::Nothing, Key::CtrlC | Key::Esc) => {
                // What was waiting comes back to the box, to change or send.
                let mut waiting: Vec<String> = std::mem::take(&mut self.steering);
                waiting.append(&mut self.queued);
                if !waiting.is_empty() {
                    let mut text = waiting.join("\n");
                    if !self.editor.is_empty() {
                        text = format!("{text}\n{}", self.editor.text());
                    }
                    self.editor.replace(&text);
                }
                // Only what's waiting to be stopped hears it: a stop kept
                // for later (`notify_one` keeps one) would end the next turn
                // as soon as it started.
                if self.busy.is_some() {
                    self.interrupted.notify_waiters();
                }
            }
            // Ctrl-C clears what you typed; on an empty box it's for leaving.
            (Waiting::Message, Key::CtrlC) if !self.editor.is_empty() => {
                self.editor.take();
            }
            (_, Key::CtrlC) => self.reply(Read::Interrupted),
            (Waiting::Answer, Key::Esc) if self.secret => self.reply(Read::Interrupted),
            (Waiting::Message, Key::CtrlD) if self.editor.is_empty() => self.reply(Read::Eof),
            (Waiting::Message, Key::BackTab) if menu.is_empty() => self.reply(Read::NextMode),
            (Waiting::Message, Key::CtrlV) => self.reply(Read::Paste),
            (_, Key::CtrlV) => {
                self.notice = Some(("ctrl-v pastes an image while rein waits for your message".into(), Instant::now()));
                edited = false;
            }
            // The arrows pick among the matching commands, up the list.
            (_, Key::Up) if !menu.is_empty() => {
                self.picked = (self.picked + 1) % menu.len();
                edited = false;
            }
            (_, Key::Down | Key::BackTab) if !menu.is_empty() => {
                self.picked = (self.picked + menu.len() - 1) % menu.len();
                edited = false;
            }
            // Tab while rein works: after this turn, not into it.
            (Waiting::Nothing, Key::Tab) if menu.is_empty() && !self.editor.is_empty() => self.submit_later(),
            // Tab completes the picked command's name, or skill's, with a
            // space to go on if it takes something: a skill, what to do.
            (_, Key::Tab) if !menu.is_empty() => {
                let entry = &menu[self.picked.min(menu.len() - 1)];
                self.editor.replace(&format!("{}{}", entry.name, if entry.args.is_empty() && !entry.skill { "" } else { " " }));
            }
            // Enter sends the picked command, or calls the picked skill.
            (_, Key::Enter) if !menu.is_empty() => {
                let name = menu[self.picked.min(menu.len() - 1)].name.clone();
                self.editor.replace(&name);
                self.submit(waiting);
            }
            _ => {
                if self.editor.key(&key) == Action::Submit {
                    self.submit(waiting);
                }
            }
        }
        if edited {
            self.pick_default();
        } else if self.notice.is_none() {
            self.notice = before;
        }
        self.draw();
    }

    /// What the mouse did: the wheel scrolls three rows at a time; a press,
    /// drag and let go select text, which is copied.
    fn mouse(&mut self, button: u16, x: usize, y: usize, up: bool) {
        // Shift, Alt and Ctrl add 4, 8 and 16.
        let button = button & !(4 | 8 | 16);
        match (button, up) {
            (64, _) => screen::scroll(-3),
            (65, _) => screen::scroll(3),
            (0, false) => drop(screen::select(x, y, Drag::Start)),
            (32, false) => drop(screen::select(x, y, Drag::Move)),
            (0, true) => {
                if let Some(text) = screen::select(x, y, Drag::End) {
                    let lines = text.lines().count();
                    let what = if lines > 1 { format!("{lines} lines") } else { format!("{} characters", text.chars().count()) };
                    let note = match copy(&text) {
                        Copied::Clipboard => format!("copied {what}"),
                        Copied::AskedTerminal => format!("asked the terminal to copy {what}"),
                    };
                    self.notice = Some((note, Instant::now()));
                    self.draw();
                }
            }
            _ => {}
        }
    }

    /// Tab while rein works: the message waits for the end of the turn.
    fn submit_later(&mut self) {
        let text = self.take_message(Waiting::Nothing);
        if !text.trim().is_empty() && !self.turned_down(&text) {
            self.queued.push(text);
        }
    }

    /// Enter: the message goes; while rein works, into the turn, at its
    /// next step. A command is for rein, not the model, so it waits for
    /// the end of the turn instead.
    fn submit(&mut self, waiting: Waiting) {
        let text = self.take_message(waiting);
        if waiting == Waiting::Nothing && self.turned_down(&text) {
            return;
        }
        match waiting {
            Waiting::Nothing if text.trim().is_empty() => {}
            Waiting::Nothing if commands::parse(&text).is_some() => self.queued.push(text),
            Waiting::Nothing => self.steering.push(text),
            _ => self.reply(Read::Line(text)),
        }
    }

    /// While rein works, a line that may hold a key doesn't wait above the
    /// box, where it would show: why it can't go is said at once, in place
    /// of the bar. `true` when it was such a line.
    fn turned_down(&mut self, text: &str) -> bool {
        let Some(Err(refused)) = commands::parse(text) else { return false };
        if !refused.hides_the_line() {
            return false;
        }
        self.notice = Some((refused.to_string(), Instant::now()));
        true
    }

    /// Takes the message out of the box: kept for ↑, unless it may hold a
    /// key, and how the box showed it kept for the conversation.
    fn take_message(&mut self, waiting: Waiting) -> String {
        self.dismissed = None;
        screen::follow();
        let entry = self.editor.entry();
        let shown = entry.shown();
        let text = self.editor.take();
        if shown != text && !self.secret {
            self.shown.push((text.clone(), shown));
        }
        if waiting != Waiting::Answer && !commands::hides(&text) {
            self.editor.remember(entry);
        }
        text
    }

    /// A key while a question waits for its answer.
    fn answer(&mut self, key: &Key) {
        let Some(asking) = &mut self.asking else { return };
        let count = asking.question.answers.len();
        let pick = |key: &str| asking.question.answers.iter().position(|(k, _)| *k == key);
        let chosen = match key {
            Key::Up | Key::Left | Key::BackTab => {
                asking.picked = (asking.picked + count - 1) % count;
                None
            }
            Key::Down | Key::Right | Key::Tab => {
                asking.picked = (asking.picked + 1) % count;
                None
            }
            Key::Enter => Some(asking.picked),
            Key::Esc | Key::CtrlC => pick("esc").or(pick("n")),
            Key::Char(c) => match c.to_digit(10) {
                Some(n @ 1..) if (n as usize) <= count => Some(n as usize - 1),
                _ => pick(&c.to_lowercase().to_string()),
            },
            _ => None,
        };
        if let Some(i) = chosen {
            let key = asking.question.answers[i].0.to_owned();
            self.asking = None;
            self.ring = false;
            self.reply(Read::Line(key));
        }
    }

    fn reply(&mut self, read: Read) {
        self.waiting = Waiting::Nothing;
        let _ = self.replies.send(read);
    }

    /// The clock's tick: the spinner moves, old notes go, and the bell rings
    /// for a question that's still waiting once you're away.
    fn tick(&mut self) {
        if self.ring && self.asking.is_some() && self.call() {
            self.ring = false;
        }
        let expired = self.notice.as_ref().is_some_and(|(_, since)| since.elapsed() > NOTICE);
        if expired {
            self.notice = None;
        }
        if expired || self.busy.is_some() {
            self.draw();
        }
    }
}

/// The most lines of a command a question shows; a longer one is shown
/// above the box too.
pub const COMMAND_LINES: usize = 8;

/// The most lines of a question's detail shown in the box.
const DETAIL_LINES: usize = 4;

/// A command as a question shows it: whole, wrapped to `width`, after a
/// `$`. Never cut short at the side, as what isn't seen can't be judged.
pub fn command_lines(command: &str, s: Style, width: usize) -> Vec<String> {
    let room = width.saturating_sub(2).max(10);
    let mut lines = Vec::new();
    for line in command.trim_end().lines() {
        let mut wrap = Wrap::new(room, 0, 0);
        let wrapped = wrap.push(&line.replace('\t', "    ")) + &wrap.finish();
        // A word wider than the box is broken, not cut.
        lines.extend(wrapped.lines().flat_map(|part| hard_wrap(part, room)));
    }
    lines.iter().enumerate().map(|(i, line)| format!("{}{line}", if i == 0 { s.dim("$ ") } else { "  ".to_owned() })).collect()
}

/// `line` in pieces no wider than `width`.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut used = 0;
    for c in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width && used > 0 {
            pieces.push(String::new());
            used = 0;
        }
        pieces.last_mut().expect("never empty").push(c);
        used += w;
    }
    pieces
}

/// How `copy` copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copied {
    /// With the system's own command: it's on the clipboard.
    Clipboard,
    /// By asking the terminal (OSC 52), which works over ssh too, but only
    /// in terminals that allow it; rein can't tell whether it did.
    AskedTerminal,
}

/// Puts `text` on the clipboard: with the system's own command where there
/// is one, and otherwise by asking the terminal.
pub fn copy(text: &str) -> Copied {
    use std::io::Write as _;
    let commands: &[&[&str]] = if cfg!(target_os = "macos") { &[&["pbcopy"]] } else { &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "--clipboard", "--input"]] };
    let local = std::env::var_os("SSH_CONNECTION").is_none();
    for command in commands.iter().filter(|_| local) {
        let child = std::process::Command::new(command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
            if child.wait().is_ok_and(|status| status.success()) && written {
                return Copied::Clipboard;
            }
        }
    }
    screen::clipboard(&base64(text.as_bytes()));
    Copied::AskedTerminal
}

/// Standard base64, for OSC 52.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// A list that opens over the box, as its rows are drawn, top to bottom:
/// the first of `count` items, the best match, right over the box, and the
/// rest going up from it, as many as `rows` around `picked`, with how many
/// more there are at each end that's out of view. `row` draws an item.
fn upward(count: usize, picked: usize, rows: usize, s: Style, row: impl Fn(usize) -> String) -> Vec<String> {
    let first = (picked + 1).saturating_sub(rows);
    let end = (first + rows).min(count);
    let mut lines = Vec::new();
    if count > end {
        lines.push(s.dim(&format!("   … {} more", count - end)));
    }
    lines.extend((first..end).rev().map(&row));
    if first > 0 {
        lines.push(s.dim(&format!("   … {first} more")));
    }
    lines
}

/// `text` with each of `words` (lowercase) in it picked out: in blue, and
/// the rest dim unless it's the `picked` line.
fn marked(text: &str, words: &[String], s: Style, picked: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let mut found = vec![false; chars.len()];
    for word in words {
        let word: Vec<char> = word.chars().collect();
        if word.is_empty() || word.len() > lower.len() {
            continue;
        }
        for start in 0..=lower.len() - word.len() {
            if lower[start..start + word.len()] == word[..] {
                found[start..start + word.len()].fill(true);
            }
        }
    }
    let mut out = String::new();
    let mut at = 0;
    while at < chars.len() {
        let end = (at..chars.len()).find(|&i| found[i] != found[at]).unwrap_or(chars.len());
        let run: String = chars[at..end].iter().collect();
        out += &match (found[at], picked) {
            (true, _) => s.bold(&s.blue(&run)),
            (false, true) => run,
            (false, false) => s.dim(&run),
        };
        at = end;
    }
    out
}

/// Lists what `@` can point at in `root`, on a thread of its own, as a big
/// project takes a moment. What the sandbox hides isn't listed: rein's
/// file tools wouldn't read it anyway.
fn look_for_paths(boxed: std::sync::Weak<Mutex<Boxed>>, root: PathBuf, sandbox: Arc<Sandbox>) {
    std::thread::spawn(move || {
        let paths = rein_agent::search::paths(&root, &|path: &Path| sandbox.places().hides(path));
        if let Some(boxed) = boxed.upgrade() {
            locked(&boxed).paths = paths;
        }
    });
}

/// `↑ 3 more lines`, or nothing for none.
fn more(count: usize, arrow: &str) -> String {
    match count {
        0 => String::new(),
        1 => format!("{arrow} 1 more line"),
        n => format!("{arrow} {n} more lines"),
    }
}

/// `4s`, `1m 05s`.
fn seconds(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

impl Input {
    /// `pinned` asks for the input box, which needs a terminal all round.
    /// `history` is what you sent before, for ↑ and Ctrl-R, `bell` says
    /// whether to ring when rein needs you and you're away, and `root` is
    /// the project, whose files `@` points at, but for what `sandbox` hides.
    pub fn start(pinned: bool, interrupted: Arc<tokio::sync::Notify>, history: History, bell: bool, root: PathBuf, sandbox: Arc<Sandbox>) -> Self {
        let (requests, asked) = std::sync::mpsc::channel::<String>();
        let (tx, replies) = tokio::sync::mpsc::unbounded_channel();
        let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && std::io::stderr().is_terminal();
        if pinned
            && terminal
            && let Some(raw) = Raw::enter()
        {
            screen::restore_on_panic();
            let boxed = Arc::new(Mutex::new(Boxed {
                pinned: Pinned::start(Style::stderr()),
                editor: Editor::with_history(history),
                waiting: Waiting::Nothing,
                prompt: String::new(),
                secret: false,
                picking: None,
                asking: None,
                busy: None,
                steering: Vec::new(),
                queued: Vec::new(),
                shown: Vec::new(),
                notice: None,
                scroll: 0,
                picked: 0,
                last_key: Instant::now(),
                outputs: Vec::new(),
                viewer: None,
                search: None,
                paths: Vec::new(),
                dismissed: None,
                skills: Vec::new(),
                focused: None,
                touched: Instant::now(),
                bell,
                ring: false,
                worked: false,
                bar: String::new(),
                replies: tx,
                interrupted,
            }));
            // A window that changes size gets the box drawn again right
            // away, not only at the next key.
            if let Ok(mut resized) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                let boxed = boxed.clone();
                tokio::spawn(async move {
                    // Laid out again for each size the window goes through.
                    while resized.recv().await.is_some() {
                        let mut boxed = locked(&boxed);
                        boxed.pinned.resized();
                        boxed.redraw();
                    }
                });
            }
            look_for_paths(Arc::downgrade(&boxed), root.clone(), sandbox.clone());
            let clock = Arc::downgrade(&boxed);
            std::thread::spawn(move || {
                while let Some(boxed) = clock.upgrade() {
                    locked(&boxed).tick();
                    drop(boxed);
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
            let keyboard = boxed.clone();
            std::thread::spawn(move || {
                let mut keys = Keys::default();
                let mut buffer = [0u8; 1024];
                let mut stdin = std::io::stdin();
                while let Ok(n @ 1..) = std::io::Read::read(&mut stdin, &mut buffer) {
                    for key in keys.feed(&buffer[..n]) {
                        locked(&keyboard).key(key);
                    }
                }
            });
            return Input { requests, replies, pending: false, boxed: Some(boxed), _raw: Some(raw), root, sandbox };
        }
        std::thread::spawn(move || {
            for prompt in asked {
                eprint!("{prompt}");
                let mut line = String::new();
                let read = match std::io::stdin().read_line(&mut line) {
                    Ok(0) | Err(_) => Read::Eof,
                    Ok(_) => Read::Line(line.trim_end_matches(['\n', '\r']).to_owned()),
                };
                if tx.send(read).is_err() {
                    break;
                }
            }
        });
        Input { requests, replies, pending: false, boxed: None, _raw: None, root, sandbox }
    }

    /// Looks again at what `@` can point at: a turn may have made or
    /// removed files.
    pub fn look_for_paths(&self) {
        if let Some(boxed) = &self.boxed {
            look_for_paths(Arc::downgrade(boxed), self.root.clone(), self.sandbox.clone());
        }
    }

    pub fn pinned(&self) -> bool {
        self.boxed.is_some()
    }

    /// Whether you're away: the terminal says its window is behind another,
    /// or you haven't touched a key for a while. Without a box, never.
    pub fn away(&self) -> bool {
        self.boxed.as_ref().is_some_and(|boxed| locked(boxed).away())
    }

    /// How wide a line in the box can be.
    pub fn text_width(&self) -> usize {
        self.boxed.as_ref().map_or(80, |boxed| locked(boxed).pinned.text_width())
    }

    fn with(&self, change: impl FnOnce(&mut Boxed)) {
        if let Some(boxed) = &self.boxed {
            let mut boxed = locked(boxed);
            change(&mut boxed);
            boxed.draw();
        }
    }

    /// Reads one line: a message (`message`, kept for ↑) or a typed answer
    /// to `prompt`. Plain lines show `prompt` first. A message queued while
    /// rein worked comes back at once.
    pub async fn read(&mut self, prompt: &str, message: bool) -> Read {
        if !self.pending {
            match &self.boxed {
                Some(boxed) => {
                    let mut boxed = locked(boxed);
                    // Steering the turn missed, if it ended first, then what
                    // was meant for after it.
                    if message {
                        let next = if !boxed.steering.is_empty() {
                            Some(boxed.steering.remove(0))
                        } else if !boxed.queued.is_empty() {
                            Some(boxed.queued.remove(0))
                        } else {
                            None
                        };
                        if let Some(next) = next {
                            boxed.draw();
                            return Read::Line(next);
                        }
                    }
                    // Done, and nothing more to do: it's your turn.
                    if message && std::mem::take(&mut boxed.worked) {
                        boxed.call();
                    }
                    boxed.waiting = if message { Waiting::Message } else { Waiting::Answer };
                    boxed.prompt = prompt.trim().to_owned();
                    boxed.pick_default();
                    boxed.draw();
                }
                None => {
                    if self.requests.send(prompt.to_owned()).is_err() {
                        return Read::Eof;
                    }
                }
            }
            self.pending = true;
        }
        let read = self.replies.recv().await.unwrap_or(Read::Eof);
        self.pending = false;
        read
    }

    /// Stops waiting for your message: rein starts on something else, like
    /// another rein's. What's typed stays in the box, and a message sent
    /// just as it started steers the turn instead. Without the box the
    /// line asked for is still read, and the next read gets it.
    pub fn stop_reading(&mut self) {
        let Some(boxed) = &self.boxed else { return };
        if !self.pending {
            return;
        }
        self.pending = false;
        let mut boxed = locked(boxed);
        if boxed.waiting == Waiting::Message {
            boxed.waiting = Waiting::Nothing;
        }
        while let Ok(read) = self.replies.try_recv() {
            if let Read::Line(line) = read {
                boxed.steering.push(line);
            }
        }
        boxed.draw();
    }

    /// The question on screen was answered in another tab: it goes.
    pub fn drop_question(&mut self) {
        let Some(boxed) = &self.boxed else { return };
        self.pending = false;
        let mut boxed = locked(boxed);
        boxed.asking = None;
        boxed.waiting = Waiting::Nothing;
        // A key pressed here at the same moment answers nothing now.
        while self.replies.try_recv().is_ok() {}
        boxed.draw();
    }

    /// Reads a secret, like an API key, in the box: it shows as dots and
    /// isn't kept, and Esc cancels. `None` without a box, where it would
    /// show as it's typed.
    pub async fn read_secret(&mut self, prompt: &str) -> Option<Read> {
        self.boxed.as_ref()?;
        self.with(|boxed| boxed.secret = true);
        let read = self.read(prompt, false).await;
        self.with(|boxed| {
            boxed.secret = false;
            // A cancelled one mustn't stay in the box, half typed.
            boxed.editor.take();
        });
        Some(read)
    }

    /// Shows `menu` and waits for a choice: the index of the item picked,
    /// or `None` if you change your mind. Without a box, the items are
    /// numbered and a number is typed.
    pub async fn choose(&mut self, menu: Menu) -> Option<usize> {
        if menu.items.is_empty() {
            return None;
        }
        if self.boxed.is_none() {
            let pad = " ".repeat(GUTTER);
            for (n, (name, about)) in menu.items.iter().enumerate() {
                let mark = if menu.current == Some(n) { "•" } else { " " };
                say!("{pad}{:>3}  {name}{mark} {about}", n + 1);
            }
            loop {
                match self.read(&format!("{} (number, or Enter to cancel) ", menu.title), false).await {
                    Read::Line(line) if line.trim().is_empty() => return None,
                    Read::Line(line) => match line.trim().parse::<usize>() {
                        Ok(n) if (1..=menu.items.len()).contains(&n) => return Some(n - 1),
                        _ => say!("{pad}Type a number from 1 to {}.", menu.items.len()),
                    },
                    _ => return None,
                }
            }
        }
        let picked = menu.at.or(menu.current).filter(|at| *at < menu.items.len()).unwrap_or(0);
        self.with(|boxed| {
            boxed.waiting = Waiting::Answer;
            boxed.picking = Some(Picking { menu, filter: Editor::default(), picked });
        });
        self.pending = true;
        let read = self.replies.recv().await.unwrap_or(Read::Eof);
        self.pending = false;
        match read {
            Read::Picked(i) => Some(i),
            _ => None,
        }
    }

    /// Asks `question` in the box and waits for the key of the answer.
    pub async fn ask(&mut self, question: Question) -> Read {
        if !self.pending {
            self.with(|boxed| {
                boxed.waiting = Waiting::Answer;
                boxed.asking = Some(Asking { question, picked: 0, since: Instant::now() });
                // Or later, if you go away while it waits.
                boxed.ring = boxed.bell && !boxed.call();
            });
            self.pending = true;
        }
        let read = self.replies.recv().await.unwrap_or(Read::Eof);
        self.pending = false;
        read
    }

    /// Says what rein is doing, over the box, with a spinner and the time
    /// since `since`; `None` once it's done.
    pub fn busy(&self, what: Option<String>, since: Instant) {
        self.with(|boxed| {
            boxed.worked |= what.is_some();
            boxed.busy = what.map(|what| (what, since));
        });
    }

    /// The messages sent to steer the turn, taken out, oldest first.
    pub fn steering(&self) -> Vec<String> {
        let Some(boxed) = &self.boxed else { return Vec::new() };
        let mut boxed = locked(boxed);
        let taken = std::mem::take(&mut boxed.steering);
        if !taken.is_empty() {
            boxed.draw();
        }
        taken
    }

    /// A short note under the box for a moment, in place of the bar.
    pub fn notice(&self, note: &str) {
        self.with(|boxed| boxed.notice = Some((note.to_owned(), Instant::now())));
    }

    /// `message` as the box showed it: big pastes as their labels.
    pub fn shown(&self, message: &str) -> String {
        let Some(boxed) = &self.boxed else { return message.to_owned() };
        let mut boxed = locked(boxed);
        match boxed.shown.iter().position(|(full, _)| full.trim() == message.trim()) {
            Some(i) => boxed.shown.remove(i).1,
            None => message.to_owned(),
        }
    }

    /// Keeps a step's full output for the viewer.
    pub fn keep_output(&self, title: String, output: &str) {
        if let Some(boxed) = &self.boxed {
            locked(boxed).outputs.push(crate::ui::viewer::Entry::new(title, output));
        }
    }

    /// Shows `bar` under the box.
    pub fn set_bar(&self, bar: String) {
        self.with(|boxed| boxed.bar = bar);
    }

    /// The skills you may call, offered when you type `/`.
    pub fn set_skills(&self, skills: Vec<Offer>) {
        self.with(|boxed| boxed.skills = skills);
    }

    /// Puts `text` in the box at the cursor, as if typed.
    pub fn insert(&self, text: &str) {
        self.with(|boxed| {
            boxed.editor.insert(text);
            boxed.draw();
        });
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        if let Some(boxed) = &self.boxed {
            locked(boxed).pinned.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::style::strip;

    #[test]
    fn shows_a_command_whole() {
        let lines: Vec<String> = command_lines("echo one two three\nls /a/very/long/path/without/spaces", Style::plain(), 22).iter().map(|l| strip(l)).collect();
        assert_eq!(lines, ["$ echo one two three", "  ls", "  /a/very/long/path/wi", "  thout/spaces"]);
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("ü".as_bytes()), "w7w=");
    }

    #[test]
    fn counts_what_doesnt_fit() {
        assert_eq!(more(0, "↑"), "");
        assert_eq!(more(1, "↓"), "↓ 1 more line");
        assert_eq!(more(3, "↑"), "↑ 3 more lines");
        assert_eq!(seconds(Duration::from_secs(65)), "1m 05s");
    }
}
