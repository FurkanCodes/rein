//! The window: sheets on a desk. Every agent is in a list, those that need
//! you first, then by project, each with what it's doing and the steps of
//! its turn; the ones you've chosen lie side by side. How many, and which,
//! is yours: an app that forces a layout is what people ask to be rid of.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Saved, Standing};
use serde_json::Value;

use crate::agent::{Agent, Opening, Start, SubAgent};
use crate::look::{Look, Wearing, wear};
use crate::palette::Palette;
use crate::pane::{About, Pane, Told};
use crate::parts::{Press, gauge, halo, strip, wordmark};
use crate::probe::Probe;

gpui_kit::actions!(rein, [NewAgent, NextMode, Complete, FindSent, AllOutput, ToBackground, OpenPalette, OpenSettings]);

/// The window's keys. Those over a box go over the toolkit's own, which indent
/// text: not what `⇥` is for in a message.
pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(if cfg!(target_os = "macos") { "cmd-n" } else { "ctrl-n" }, NewAgent, None),
        KeyBinding::new(if cfg!(target_os = "macos") { "cmd-k" } else { "ctrl-k" }, OpenPalette, None),
        KeyBinding::new(if cfg!(target_os = "macos") { "cmd-," } else { "ctrl-," }, OpenSettings, None),
        KeyBinding::new("shift-tab", NextMode, Some("Input")),
        KeyBinding::new("tab", Complete, Some("Input")),
        KeyBinding::new("ctrl-r", FindSent, Some("Input")),
        KeyBinding::new("ctrl-o", AllOutput, Some("Input")),
        KeyBinding::new("ctrl-b", ToBackground, Some("Input")),
    ]);
}

/// How wide the list of agents is.
pub(crate) const SIDEBAR: f32 = 256.;
/// How tall the top of the list is, which holds the window's three buttons.
const TOP: f32 = 44.;
/// How often the list's ages are brought up to date.
const AGES: Duration = Duration::from_secs(30);

/// An agent, and whether it's one of those on show.
struct Held {
    pane: Entity<Pane>,
    shown: bool,
}

/// The list of saved conversations, while you're choosing from it.
enum Choosing {
    Loading,
    Listed(Vec<Saved>),
    Failed(String),
}

/// Every pane, and what they cost.
pub(crate) struct Panes {
    held: Vec<Held>,
    probe: Probe,
    /// How a new agent is started.
    serve: Vec<String>,
    choosing: Option<Choosing>,
    /// Holds the keys with no box open, so that shortcuts still reach this.
    pub(crate) focus: FocusHandle,
    /// `⌘K`, while it's up.
    pub(crate) palette: Option<Palette>,
    /// The pane to start with, drawn on a desk with nothing else on it.
    draft: Option<Entity<Pane>>,
    /// The saved conversations, newest first, once they've been listed.
    recent: Option<Vec<Saved>>,
    /// Which look the window wears, as your settings keep it: shared with
    /// what follows the Mac's.
    wearing: std::rc::Rc<std::cell::Cell<Wearing>>,
}

/// How many saved conversations the first run offers to go on with.
const RECENT: usize = 4;

/// Somewhere to take hold of the window by.
pub(crate) fn drag_area(id: &'static str) -> Stateful<Div> {
    div().id(id).on_mouse_down(MouseButton::Left, |event, window, _| {
        if event.click_count == 2 {
            window.titlebar_double_click();
        } else {
            window.start_window_move();
        }
    })
}

/// How long something has stood, as the list says it.
fn age(stood: Duration) -> String {
    match stood.as_secs() {
        ..60 => "now".to_owned(),
        seconds @ 60..3600 => format!("{}m", seconds / 60),
        seconds @ 3600..86_400 => format!("{}h", seconds / 3600),
        seconds => format!("{}d", seconds / 86_400),
    }
}

/// How long ago `at` (seconds since 1970) was, as the list says it.
pub(crate) fn age_since(at: u64) -> String {
    age(Duration::from_secs(now().saturating_sub(at)))
}

/// `path` with your home folder as `~`.
pub(crate) fn tilde(path: &str, home: Option<&str>) -> String {
    match home.and_then(|home| path.strip_prefix(home)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

/// What the button to the saved conversations says: how many, once known.
fn saved_label(recent: Option<&[Saved]>) -> String {
    match recent.map(<[Saved]>::len) {
        Some(1) => "1 saved conversation".to_owned(),
        Some(count) if count > 0 => format!("{count} saved conversations"),
        _ => "Saved conversations".to_owned(),
    }
}

/// The seconds since 1970, now.
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// A list's heading: what it's called, and on the right what's under it.
fn group(name: impl Into<SharedString>, beside: impl Into<SharedString>, look: &Look) -> Div {
    div()
        .flex()
        .justify_between()
        .gap(px(8.))
        .px(px(9.))
        .pt(px(16.))
        .pb(px(6.))
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(look.ink_2)
        .child(div().flex_none().child(name.into()))
        .child(div().min_w_0().truncate().font_weight(FontWeight::NORMAL).text_color(look.ink_3).child(beside.into()))
}

impl Panes {
    pub(crate) fn new(probe: Probe, serve: Vec<String>, wearing: Wearing, window: &mut Window, cx: &mut Context<Self>) -> Panes {
        let wearing = std::rc::Rc::new(std::cell::Cell::new(wearing));
        let following = wearing.clone();
        cx.observe_window_appearance(window, move |_, window, cx| {
            wear(following.get().dark(window.appearance()), cx);
            window.refresh();
        })
        .detach();
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(AGES).await;
                let told = view.update(cx, |view, cx| {
                    if !view.held.is_empty() {
                        cx.notify();
                    }
                });
                if told.is_err() {
                    break;
                }
            }
        })
        .detach();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        // Once whatever the window opens with is open: with an agent on the
        // desk, there's no pane to start with.
        cx.defer_in(window, |view, window, cx| view.ensure_draft(window, cx));
        Panes { held: Vec::new(), probe, serve, choosing: None, focus, palette: None, draft: None, recent: None, wearing }
    }

    /// With nothing on the desk, there's a pane to start with, and the
    /// saved conversations to go on with.
    fn ensure_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.held.iter().any(|held| held.shown) {
            return;
        }
        self.list_recent(window, cx);
        match &self.draft {
            Some(draft) => {
                let draft = draft.clone();
                cx.defer_in(window, move |_, window, cx| draft.update(cx, |pane, cx| pane.take_keys(window, cx)));
            }
            None => {
                let home = std::env::var("HOME").ok();
                // Opened from the Finder, the window starts at `/`.
                let cwd = match (here(), home) {
                    (here, Some(home)) if here == std::path::Path::new("/") => home,
                    (here, _) => here.display().to_string(),
                };
                self.make_draft(&cwd, String::new(), window, cx);
            }
        }
    }

    /// A pane to start with in `cwd`, with `text` in its box.
    fn make_draft(&mut self, cwd: &str, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let about = About { cwd: cwd.to_owned(), draft: true, ..About::default() };
        let start = Start { command: &self.serve, cwd, env: &[], opening: Opening::New };
        let agent = Agent::start(&start);
        let why = agent.as_ref().err().cloned();
        let pane = cx.new(|cx| Pane::new(agent.ok(), about, window, cx));
        if let Some(why) = why {
            pane.update(cx, |pane, cx| pane.couldnt_start(&why, cx));
        }
        cx.subscribe_in(&pane, window, Self::heard).detach();
        pane.update(cx, |pane, cx| {
            pane.set_looking(true, cx);
            if !text.is_empty() {
                pane.input.update(cx, |input, cx| input.set_value(text, window, cx));
            }
        });
        let focus = pane.clone();
        cx.defer_in(window, move |_, window, cx| focus.update(cx, |pane, cx| pane.take_keys(window, cx)));
        self.draft = Some(pane);
        cx.notify();
    }

    /// The pane to start with, while it shows.
    fn shown_draft(&self) -> Option<Entity<Pane>> {
        self.draft.clone().filter(|_| !self.held.iter().any(|held| held.shown))
    }

    /// The pane you're at, or else the one to start with, if it's showing:
    /// what the keys and ⌘K are for. ⌘K once went by the panes alone, and
    /// on the first run listed nothing at all.
    pub(crate) fn at(&self, cx: &App) -> Option<Entity<Pane>> {
        self.looking_at(cx).or_else(|| self.shown_draft())
    }

    /// Asks which folder the pane to start with should work in, and starts
    /// it again there, with what you'd written.
    fn move_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Work here".into()) };
        let chosen = cx.prompt_for_paths(options);
        cx.spawn_in(window, async move |view, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else { return };
            let Some(folder) = paths.into_iter().next() else { return };
            let _ = view.update_in(cx, |view, window, cx| {
                let text = view.draft.take().map(|old| old.read(cx).input.read(cx).value().to_string()).unwrap_or_default();
                view.make_draft(&folder.display().to_string(), text, window, cx);
            });
        })
        .detach();
    }

    /// Lists the saved conversations for the first run, off the UI thread.
    fn list_recent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let serve = self.serve.clone();
        let listing = cx.background_executor().spawn(async move { Agent::saved(&serve) });
        cx.spawn_in(window, async move |view, cx| {
            let listed = listing.await;
            let _ = view.update(cx, |view, cx| {
                // One that can't be listed shows none: there's nothing to do about it here.
                view.recent = Some(listed.unwrap_or_default());
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn panes(&self) -> Vec<Entity<Pane>> {
        self.held.iter().map(|held| held.pane.clone()).collect()
    }

    /// The pane you're at.
    pub(crate) fn looking_at(&self, cx: &App) -> Option<Entity<Pane>> {
        self.held.iter().find(|held| held.pane.read(cx).is_looked_at()).map(|held| held.pane.clone())
    }

    fn hold(&mut self, pane: Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        cx.subscribe_in(&pane, window, Self::heard).detach();
        self.held.push(Held { pane, shown: true });
        cx.notify();
    }

    /// Starts an agent in a pane of its own. One that can't start says why
    /// there, and tells any pane that asked for it.
    pub(crate) fn open(&mut self, start: &Start<'_>, about: About, window: &mut Window, cx: &mut Context<Self>) -> Entity<Pane> {
        let agent = Agent::start(start);
        let why = agent.as_ref().err().cloned();
        let pane = cx.new(|cx| Pane::new(agent.ok(), about, window, cx));
        if let Some(why) = why {
            pane.update(cx, |pane, cx| pane.couldnt_start(&why, cx));
        }
        self.hold(pane.clone(), window, cx);
        pane
    }

    /// A pane with no agent, to type in.
    pub(crate) fn add_empty(&mut self, cwd: &str, window: &mut Window, cx: &mut Context<Self>) {
        let about = About { cwd: cwd.to_owned(), ..About::default() };
        let pane = cx.new(|cx| Pane::new(None, about, window, cx));
        self.hold(pane, window, cx);
    }

    /// Asks which folder, and starts an agent there.
    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Start an agent here".into()) };
        let chosen = cx.prompt_for_paths(options);
        cx.spawn_in(window, async move |view, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else { return };
            let Some(folder) = paths.into_iter().next() else { return };
            let _ = view.update_in(cx, |view, window, cx| view.start_in(&folder, window, cx));
        })
        .detach();
    }

    fn start_in(&mut self, folder: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = folder.display().to_string();
        let serve = self.serve.clone();
        let about = About { cwd: cwd.clone(), ..About::default() };
        let pane = self.open(&Start { command: &serve, cwd: &cwd, env: &[], opening: Opening::New }, about, window, cx);
        self.look_at(&pane, true, window, cx);
    }

    /// Lists the saved conversations, off the UI thread: it starts a process.
    fn choose_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.choosing = Some(Choosing::Loading);
        cx.notify();
        let serve = self.serve.clone();
        let listing = cx.background_executor().spawn(async move { Agent::saved(&serve) });
        cx.spawn_in(window, async move |view, cx| {
            let listed = listing.await;
            let _ = view.update(cx, |view, cx| {
                // Unless you've gone back in the meantime.
                if view.choosing.is_some() {
                    view.choosing = Some(listed.map_or_else(Choosing::Failed, Choosing::Listed));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_saved(&mut self, saved: &Saved, window: &mut Window, cx: &mut Context<Self>) {
        self.choosing = None;
        let serve = self.serve.clone();
        // Open already, you go to it: two agents on one file would each
        // write their own history into it.
        if let Some(held) = self.held.iter().find(|held| held.pane.read(cx).session() == Some(saved.id.as_str())) {
            let pane = held.pane.clone();
            self.look_at(&pane, true, window, cx);
            return;
        }
        let about = About { name: saved.title.clone(), cwd: saved.cwd.clone(), session: Some(saved.id.clone()), ..About::default() };
        let start = Start { command: &serve, cwd: &saved.cwd, env: &[], opening: Opening::Saved(saved.id.clone()) };
        let pane = self.open(&start, about, window, cx);
        self.look_at(&pane, true, window, cx);
    }

    /// `pane` is the one you're at. `focus` puts the caret in its box: a
    /// click inside it has already put the focus where it wanted.
    pub(crate) fn look_at(&mut self, pane: &Entity<Pane>, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        for held in &mut self.held {
            let looking = held.pane == *pane;
            held.shown |= looking;
            held.pane.update(cx, |pane, cx| pane.set_looking(looking, cx));
        }
        if focus {
            pane.update(cx, |pane, cx| pane.take_keys(window, cx));
        }
        cx.notify();
    }

    fn start_sub_agent(&mut self, parent: &Entity<Pane>, id: &Value, asked: &SubAgent, window: &mut Window, cx: &mut Context<Self>) {
        let about = About {
            name: Some(asked.name.clone()),
            cwd: asked.cwd.clone(),
            started_for: Some((parent.downgrade(), id.clone())),
            parent: Some(parent.downgrade()),
            ..About::default()
        };
        self.open(&Start { command: &asked.command, cwd: &asked.cwd, env: &asked.env, opening: Opening::New }, about, window, cx);
    }

    /// A press on what doesn't take keys leaves them here, where nothing
    /// types: what you write next goes to the agent you're at, as in a
    /// terminal, wherever you pressed.
    pub(crate) fn keys_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus.is_focused(window)
            && let Some(pane) = self.at(cx)
        {
            pane.update(cx, |pane, cx| pane.take_keys(window, cx));
        }
    }

    /// `key` came all this way, so what has the keys didn't want it.
    fn typed_elsewhere(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let in_a_box = window.context_stack().iter().any(|context| context.contains("Input"));
        if !in_a_box
            && self.palette.is_none()
            && let Some(pane) = self.looking_at(cx).or_else(|| self.shown_draft())
        {
            pane.update(cx, |pane, cx| pane.typed_elsewhere(key, window, cx));
        }
    }

    fn heard(&mut self, pane: &Entity<Pane>, told: &Told, window: &mut Window, cx: &mut Context<Self>) {
        // The pane to start with is no conversation yet: there's nothing to
        // hide, close, or start again.
        if self.draft.as_ref() == Some(pane) && matches!(told, Told::Hide | Told::Close | Told::New | Told::Looked) {
            return;
        }
        match told {
            Told::Changed => cx.notify(),
            Told::Looked => {
                self.look_at(pane, false, window, cx);
                // Once the press has done what it will.
                let pane = pane.clone();
                cx.defer_in(window, move |_, window, cx| pane.update(cx, |pane, cx| pane.keys_follow(window, cx)));
            }
            Told::SubAgent { id, asked: Some(asked) } => self.start_sub_agent(pane, id, asked, window, cx),
            Told::SubAgent { id, asked: None } => pane.update(cx, |pane, _| pane.refuse(id, "this window can't read that sub-agent")),
            Told::Hide => {
                if let Some(held) = self.held.iter_mut().find(|held| held.pane == *pane) {
                    held.shown = false;
                }
                self.look_past(pane, window, cx);
            }
            // Dropping it closes its agent's input, which ends it.
            Told::Close => self.close(pane, window, cx),
            Told::New => {
                let cwd = pane.read(cx).cwd().to_owned();
                self.close(pane, window, cx);
                self.start_in(std::path::Path::new(&cwd), window, cx);
            }
            Told::Resume => self.choose_saved(window, cx),
            Told::Begun => {
                if self.draft.as_ref() == Some(pane) {
                    self.draft = None;
                    self.held.push(Held { pane: pane.clone(), shown: true });
                    self.look_at(pane, true, window, cx);
                }
            }
            Told::Folder => self.move_draft(window, cx),
            Told::Look(kept) => {
                if let Some(wearing) = Wearing::of(kept) {
                    self.wearing.set(wearing);
                    wear(wearing.dark(window.appearance()), cx);
                    window.refresh();
                }
            }
            Told::OpenSubAgent(name) => {
                let parent = pane.downgrade();
                let children: Vec<Entity<Pane>> = self.held.iter().map(|held| held.pane.clone()).filter(|child| child.read(cx).parent() == Some(&parent)).collect();
                // By its name, or the only one there is.
                let named = children.iter().find(|child| child.read(cx).title() == *name).cloned();
                if let Some(child) = named.or_else(|| (children.len() == 1).then(|| children[0].clone())) {
                    self.look_at(&child, true, window, cx);
                }
            }
        }
    }

    fn close(&mut self, pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        pane.update(cx, |pane, cx| pane.closing(cx));
        self.held.retain(|held| held.pane != *pane);
        self.look_past(pane, window, cx);
    }

    /// `left` has gone from the desk. If you were at it, you go to the last
    /// pane still there, or with none, the keys to the window, for ⌘N.
    fn look_past(&mut self, left: &Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        if left.read(cx).is_looking() {
            left.update(cx, |pane, cx| pane.set_looking(false, cx));
            match self.held.iter().rev().find(|held| held.shown).map(|held| held.pane.clone()) {
                Some(next) => self.look_at(&next, true, window, cx),
                None => self.focus.focus(window, cx),
            }
        }
        self.ensure_draft(window, cx);
        cx.notify();
    }

    /// Those in `rows`, each followed by its sub-agents, which say so.
    fn with_sub_agents(&self, rows: &[usize], cx: &App) -> Vec<(usize, bool)> {
        let parent_of = |index: usize| {
            let parent = self.held[index].pane.read(cx).parent()?.upgrade()?;
            rows.iter().copied().find(|&row| self.held[row].pane == parent)
        };
        let mut ordered = Vec::with_capacity(rows.len());
        for &index in rows.iter().filter(|&&index| parent_of(index).is_none()) {
            ordered.push((index, false));
            ordered.extend(rows.iter().copied().filter(|&row| parent_of(row) == Some(index)).map(|row| (row, true)));
        }
        ordered
    }

    /// The agents that aren't waiting for you, by folder, in the order each
    /// folder first came up.
    fn by_folder(&self, cx: &App) -> Vec<(String, String, Vec<usize>)> {
        let mut folders: Vec<(String, String, Vec<usize>)> = Vec::new();
        for (index, held) in self.held.iter().enumerate() {
            let pane = held.pane.read(cx);
            if pane.standing() == Standing::Asking {
                continue;
            }
            match folders.iter_mut().find(|(_, cwd, _)| cwd == pane.cwd()) {
                Some((_, _, rows)) => rows.push(index),
                None => folders.push((pane.folder().to_owned(), pane.cwd().to_owned(), vec![index])),
            }
        }
        folders
    }

    /// One agent, as a note in a list: its name, then what it's doing and how
    /// long ago, on one left edge. The strip ends at the right.
    fn agent_row(&self, index: usize, child: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        let held = &self.held[index];
        let pane = held.pane.read(cx);
        let standing = pane.standing();
        let looking = pane.is_looking();
        let (word, tone, heavy) = match standing {
            Standing::Asking => (pane.wanting().unwrap_or_else(|| standing.word().to_owned()), look.amber_text, false),
            Standing::Working => (standing.word().to_owned(), look.teal_text, false),
            Standing::Failed => (pane.failure().map_or_else(|| standing.word().to_owned(), |why| format!("failed: {why}")), look.red, false),
            Standing::Unread => (standing.word().to_owned(), look.ink, true),
            Standing::Idle | Standing::Ended => (standing.word().to_owned(), look.ink_3, false),
        };
        // What reads on the desk falls under 4.5:1 on the tint.
        let tone = if looking { look.ink_2 } else { tone };
        let quiet = if looking { look.ink_2 } else { look.ink_3 };
        let target = held.pane.clone();
        let elbow = child.then(|| div().absolute().left(px(-6.)).top(px(-1.)).w(px(6.)).h(px(17.)).border_l_1().border_b_1().border_color(look.line_2).rounded_bl(px(4.)));
        div()
            .id(("agent", index))
            .relative()
            .flex()
            .flex_col()
            .gap(px(1.))
            .pl(px(9.))
            .pr(px(9.))
            .pt(px(7.))
            .pb(px(8.))
            .mb(px(1.))
            .when(child, |row| row.ml(px(14.)))
            .rounded(px(8.))
            .when(looking, |row| row.bg(look.teal_tint))
            .when(!looking, |row| row.hover(|style| style.bg(look.hover)))
            .when(!held.shown, |row| row.opacity(0.6))
            .on_click(cx.listener(move |view, _, window, cx| view.look_at(&target, true, window, cx)))
            .children(elbow)
            .child(div().truncate().when(standing == Standing::Asking, |title| title.font_weight(FontWeight::SEMIBOLD)).child(pane.title()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(16.))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(quiet)
                    .child(div().min_w_0().truncate().text_color(tone).when(heavy, |word| word.font_weight(FontWeight::MEDIUM)).child(word))
                    .child(div().flex_none().px(px(5.)).child("·"))
                    .child(div().flex_none().child(age(pane.age())))
                    .child(div().flex_1().min_w(px(10.)))
                    .child(strip(&pane.ticks(), standing == Standing::Asking, &look)),
            )
    }

    fn agents(&self, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let home = std::env::var("HOME").ok();
        let mut list = div().flex().flex_col();
        let waiting: Vec<usize> = self.held.iter().enumerate().filter(|(_, held)| held.pane.read(cx).standing() == Standing::Asking).map(|(index, _)| index).collect();
        if !waiting.is_empty() {
            list = list.child(group("Needs you", waiting.len().to_string(), &look));
            for index in waiting {
                list = list.child(self.agent_row(index, false, cx));
            }
        }
        for (name, cwd, rows) in self.by_folder(cx) {
            list = list.child(group(name, tilde(&cwd, home.as_deref()), &look));
            for (index, child) in self.with_sub_agents(&rows, cx) {
                list = list.child(self.agent_row(index, child, cx));
            }
        }
        list.child(div().mt(px(10.)).child(
            Press::new("resume").ghost().small().wide().label(saved_label(self.recent.as_deref())).on_click(cx.listener(|view, _, window, cx| view.choose_saved(window, cx))),
        ))
    }

    /// The saved conversations to choose from.
    fn saved_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let look = Look::of(cx);
        let list = div().id("saved").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().px(px(8.)).pb(px(8.));
        let said = |text: String| div().p(px(9.)).text_color(look.ink_3).child(text);
        match &self.choosing {
            Some(Choosing::Listed(saved)) if !saved.is_empty() => {
                let rows = saved.iter().enumerate().map(|(index, one)| {
                    let picked = one.clone();
                    let about = format!("{} · {}", one.folder(), one.day().unwrap_or_default());
                    div()
                        .id(("saved", index))
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .px(px(9.))
                        .pt(px(7.))
                        .pb(px(8.))
                        .rounded(px(8.))
                        .hover(|style| style.bg(look.hover))
                        .on_click(cx.listener(move |view, _, window, cx| view.open_saved(&picked, window, cx)))
                        .child(div().truncate().child(one.title.clone().unwrap_or_else(|| "A conversation".to_owned())))
                        .child(div().truncate().text_size(px(12.)).line_height(px(16.)).text_color(look.ink_3).child(about))
                });
                list.children(rows.collect::<Vec<_>>()).into_any_element()
            }
            Some(Choosing::Listed(_)) => list.child(said("No saved conversations yet.".to_owned())).into_any_element(),
            Some(Choosing::Failed(why)) => list.child(said(why.clone())).into_any_element(),
            _ => list.child(said("Looking…".to_owned())).into_any_element(),
        }
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let top = div().flex().flex_none().items_center().gap(px(4.)).h(px(TOP)).pl(px(14.)).pr(px(8.)).child(drag_area("drag-list").flex_1().h_full());
        let bar = div().flex().flex_col().flex_none().w(px(SIDEBAR)).h_full();
        if self.choosing.is_some() {
            let back = Press::new("back").ghost().small().label("Back").on_click(cx.listener(|view, _, _, cx| {
                view.choosing = None;
                cx.notify();
            }));
            return bar.child(top.child(back)).child(self.saved_list(cx));
        }
        let start = Press::new("new-agent")
            .ghost()
            .square()
            .named("New agent")
            .child(Icon::new(IconName::Plus).size_4())
            .on_click(cx.listener(|view, _, window, cx| view.choose_folder(window, cx)));
        let meters = self.meters(cx);
        let settings = Press::new("settings").ghost().small().wide().label("Settings").key("⌘,").on_click(cx.listener(|view, _, window, cx| view.open_settings(window, cx)));
        let foot = div().flex_none().flex().flex_col().gap(px(4.)).px(px(8.)).py(px(8.)).border_t_1().border_color(Look::of(cx).line).children(meters).child(settings);
        let find = Press::new("palette").ghost().small().label("⌘K").named("Go to, or run").on_click(cx.listener(|view, _, window, cx| view.toggle_palette(window, cx)));
        bar.child(top.child(find).child(start)).child(div().id("agents").flex_1().min_h_0().overflow_y_scroll().px(px(8.)).pb(px(8.)).child(self.agents(cx))).child(foot)
    }

    /// The settings page, from the rein behind the pane you're at, or the
    /// pane to start with.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.looking_at(cx).or_else(|| self.shown_draft()) else { return };
        pane.update(cx, |pane, cx| pane.open_page(window, cx));
        cx.notify();
    }

    /// The pane whose settings page is open, if one is.
    fn page_open(&self, cx: &App) -> Option<Entity<Pane>> {
        self.held.iter().map(|held| &held.pane).chain(self.draft.iter()).find(|pane| pane.read(cx).shows_page()).cloned()
    }

    /// How much of your limits is used, by the agent you're at, as a note at
    /// the foot of the list: a provider that says nothing shows nothing.
    fn meters(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let look = Look::of(cx);
        let at = self.held.iter().find(|held| held.pane.read(cx).is_looking())?;
        let meters = at.pane.read(cx).meters();
        if meters.is_empty() {
            return None;
        }
        let rows: Vec<Div> = meters
            .iter()
            .map(|meter| {
                let fill = if meter.warn { look.amber } else { look.ink_3 };
                let said = meter.resets.as_ref().map_or_else(|| meter.said.clone(), |resets| format!("{} →{resets}", meter.said));
                let row = div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(18.))
                    .text_size(px(11.5))
                    .text_color(if meter.warn { look.amber_text } else { look.ink_3 })
                    .child(div().flex_none().child(meter.label.clone()));
                match meter.used {
                    Some(used) => row.child(gauge(f32::from(used) / 100., None, fill, &look)).child(div().flex_none().child(said)),
                    None => row,
                }
            })
            .collect();
        Some(div().px(px(9.)).pb(px(4.)).children(rows))
    }

    /// What's on the desk with nothing on it: the pane to start with, and
    /// the saved conversations to go on with.
    fn nothing_open(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let look = Look::of(cx);
        let draft = self.draft.clone();
        let signing = draft.as_ref().is_some_and(|draft| draft.read(cx).is_signing());
        let recent = self.recent.as_deref().filter(|recent| !recent.is_empty() && !signing).map(|recent| self.go_on_with(recent, cx));
        let column = div().flex().flex_col().gap(px(24.)).w_full().max_w(px(620.));
        div()
            .id("first-run")
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .overflow_y_scroll()
            .pt(px(72.))
            .px(px(20.))
            .pb(px(20.))
            .rounded(px(10.))
            .bg(look.bg)
            .shadow(vec![halo(look.line, 1.)])
            .child(column.child(wordmark(5.)).children(draft.map(AnyView::from)).children(recent))
    }

    /// The newest saved conversations, each a press away.
    fn go_on_with(&self, recent: &[Saved], cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let now = now();
        let rows = recent.iter().take(RECENT).enumerate().map(|(index, one)| {
            let picked = one.clone();
            div()
                .id(("recent", index))
                .flex()
                .items_center()
                .gap(px(10.))
                .h(px(32.))
                .px(px(8.))
                .rounded(px(7.))
                .hover(|style| style.bg(look.hover))
                .on_click(cx.listener(move |view, _, window, cx| view.open_saved(&picked, window, cx)))
                .child(div().flex_1().min_w_0().truncate().child(one.title.clone().unwrap_or_else(|| "A conversation".to_owned())))
                .child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(one.folder().to_owned()))
                .child(div().flex_none().w(px(36.)).text_right().text_size(px(12.)).text_color(look.ink_3).child(one.age(now).map(age).unwrap_or_default()))
        });
        let heading = div()
            .flex()
            .justify_between()
            .px(px(8.))
            .pb(px(4.))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(look.ink_2)
            .child("Go on with one")
            .child(div().font_weight(FontWeight::NORMAL).text_color(look.ink_3).child(format!("{} saved", recent.len())));
        div().flex().flex_col().gap(px(2.)).child(heading).children(rows.collect::<Vec<_>>())
    }
}

impl Render for Panes {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let look = Look::of(cx);
        // Over the whole window, with nothing beside it.
        if let Some(pane) = self.page_open(cx) {
            return div().size_full().bg(look.desk).text_color(look.ink).text_size(px(13.)).line_height(px(18.)).child(AnyView::from(pane)).into_any_element();
        }
        let probe = self.probe.clone();
        let frames = canvas(|_, _, _| (), move |_, (), _, _| probe.painted()).absolute().size_0();
        let pointer = self.probe.clone();
        // Cached: typing in one pane doesn't draw the others again.
        let shown = self.held.iter().filter(|held| held.shown).map(|held| AnyView::from(held.pane.clone()).cached(StyleRefinement::default().flex_1().min_w_0().h_full()));
        let panes: Vec<_> = shown.collect();
        let empty = panes.is_empty().then(|| self.nothing_open(cx));
        let sidebar = self.sidebar(cx);
        let palette = self.palette_card(cx);
        div()
            .track_focus(&self.focus)
            // Once the press has done what it will: a window can be moved
            // from here, and its release is never heard.
            .on_mouse_down(MouseButton::Left, cx.listener(|_, _, window, cx| cx.defer_in(window, |view, window, cx| view.keys_home(window, cx))))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| view.typed_elsewhere(&event.keystroke, window, cx)))
            .on_action(cx.listener(|view, _: &NewAgent, window, cx| view.choose_folder(window, cx)))
            .on_action(cx.listener(|view, _: &OpenPalette, window, cx| view.toggle_palette(window, cx)))
            .on_action(cx.listener(|view, _: &OpenSettings, window, cx| view.open_settings(window, cx)))
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(look.desk)
            .text_color(look.ink)
            .text_size(px(13.))
            .line_height(px(18.))
            .on_mouse_move(move |_, _, _| pointer.pointer_moved())
            .child(sidebar)
            .child(div().flex().flex_1().min_w_0().h_full().gap(px(6.)).py(px(6.)).pr(px(6.)).children(panes).children(empty))
            .child(frames)
            .children(palette)
            .into_any_element()
    }
}

/// A folder as the window was started in it.
pub(crate) fn here() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// What a test of the window reaches for that nothing else needs.
#[cfg(test)]
impl Panes {
    pub(crate) fn hold_for_test(&mut self, pane: Entity<Pane>, window: &mut Window, cx: &mut Context<Self>) {
        self.hold(pane, window, cx);
    }

    pub(crate) fn draft_for_test(&self) -> Option<Entity<Pane>> {
        self.draft.clone()
    }

    pub(crate) fn open_saved_for_test(&mut self, saved: &Saved, window: &mut Window, cx: &mut Context<Self>) {
        self.open_saved(saved, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration, age, tilde};

    #[test]
    fn an_age_is_said_in_the_largest_unit_that_fits() {
        let said = |seconds| age(Duration::from_secs(seconds));
        assert_eq!([said(0), said(59), said(60), said(3599), said(3600), said(86_399), said(86_400)], ["now", "now", "1m", "59m", "1h", "23h", "1d"]);
    }

    #[test]
    fn only_a_path_in_your_home_folder_goes_by_a_tilde() {
        let home = Some("/Users/you");
        assert_eq!(tilde("/Users/you/projects/rein", home), "~/projects/rein");
        assert_eq!(tilde("/Users/you", home), "~");
        assert_eq!(tilde("/Users/younger/rein", home), "/Users/younger/rein", "a name that starts the same isn't yours");
        assert_eq!(tilde("/tmp", None), "/tmp");
    }
}
