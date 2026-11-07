//! The box: what you write, what it lists as you do, and what waits to go.
//! Its rules are the terminal's, read in `rein_transcript::composing`: `↑`
//! brings back what you sent, `@` points at a file, Tab sends after the turn.
//!
//! The text box takes `↑`, `↓`, Enter and Esc as actions before any key
//! reaches a listener, so these are caught on their way in, and pass through
//! when they're the box's own: a key listener here never heard an arrow.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use gpui_kit::component::input::{Enter, Escape, MoveLeft};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::composing::{Recall, mention, still_in, waiting_shown, word_at};
use rein_transcript::{Command, Menu, fuzzy, says_back};

use super::{Pane, Sent};
use crate::agent::Attached;
use crate::look::Look;

/// How many rows the list over the box shows.
const LISTED: usize = 8;
/// How many of what you sent can be searched at once.
const FOUND_MOST: usize = 200;

/// What's listed over the box, for the word being typed.
#[derive(Debug, PartialEq, Eq)]
enum Over {
    /// Commands, for the first word.
    Commands(Vec<usize>),
    /// Paths, for an `@` word that starts here.
    Paths { start: usize, fits: Vec<String> },
    /// Skills, for a `/` word that starts here, later in a message.
    Skills { start: usize, fits: Vec<usize> },
}

impl Over {
    fn len(&self) -> usize {
        match self {
            Over::Commands(fits) | Over::Skills { fits, .. } => fits.len(),
            Over::Paths { fits, .. } => fits.len(),
        }
    }
}

/// A message waiting for the end of the turn.
pub(super) struct Later {
    text: String,
    images: Vec<Attached>,
}

/// The rest of what the box holds.
#[derive(Default)]
pub(super) struct Writing {
    pub(super) recall: Recall,
    over: Option<Over>,
    /// Which of what's listed is picked.
    at: usize,
    /// Esc put the list away, for the word it had: another word lists again.
    left: Option<String>,
    /// What `@` can point at, once rein has said. A turn may change it.
    paths: Option<Arc<Vec<String>>>,
    asked_paths: bool,
    /// What went as a message, and is on its way as a steer.
    shown_already: Option<String>,
    /// The images pasted, until they go or their label's taken out.
    attached: Vec<Attached>,
    pasted: usize,
    later: Vec<Later>,
    /// What you said to the turn, until it reaches it.
    steering: Vec<String>,
    /// What comes back to the box, which takes a window.
    to_box: Option<String>,
}

impl Writing {
    /// A turn has ended, so what `@` finds may have changed, and it won't
    /// reach anything it hasn't by now.
    pub(super) fn turn_over(&mut self) {
        self.paths = None;
        self.asked_paths = false;
        self.steering.clear();
    }

    /// `text` shows as yours already, and may yet come back as a steer.
    pub(super) fn shown_already(&mut self, text: &str) {
        self.shown_already = Some(text.to_owned());
    }

    /// Whether `text`, coming back, is what shows already. Once.
    pub(super) fn was_shown_already(&mut self, text: &str) -> bool {
        let was = self.shown_already.as_deref() == Some(text);
        if was {
            self.shown_already = None;
        }
        was
    }

    /// `text` reached the turn.
    pub(super) fn reached(&mut self, text: &str) {
        if let Some(at) = self.steering.iter().position(|said| said == text) {
            self.steering.remove(at);
        }
    }

    pub(super) fn steering(&mut self, text: &str) {
        self.steering.push(text.to_owned());
    }

    /// What was waiting goes back to the box: what stopped the turn should
    /// be yours to change. Oldest first, under what's there.
    pub(super) fn bring_back(&mut self) {
        let later = std::mem::take(&mut self.later);
        if later.is_empty() {
            return;
        }
        let mut text: Vec<String> = Vec::with_capacity(later.len());
        for one in later {
            text.push(one.text);
            self.attached.extend(one.images);
        }
        self.to_box = Some(text.join("\n"));
    }

    /// The next message, if one's waiting.
    pub(super) fn next_later(&mut self) -> Option<(String, Vec<Attached>)> {
        (!self.later.is_empty()).then(|| self.later.remove(0)).map(|later| (later.text, later.images))
    }

    pub(super) fn waits(&mut self, text: String, images: Vec<Attached>) {
        self.later.push(Later { text, images });
    }

    /// The images that go with `text`: those whose labels are still there.
    /// The others were taken out of the box.
    fn going_with(&mut self, text: &str) -> Vec<Attached> {
        let attached = std::mem::take(&mut self.attached);
        let kept: Vec<String> = still_in(text, attached.iter().map(|image| image.label.as_str())).into_iter().map(str::to_owned).collect();
        attached.into_iter().filter(|image| kept.contains(&image.label)).collect()
    }
}

/// What a file's name says it is, of what rein can show.
fn image_kind(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// What's copied as `format`, as rein can show it: a screenshot and what a
/// browser copies can be TIFF alone, which no provider takes.
fn as_shown(format: ImageFormat, bytes: &[u8]) -> Result<(Vec<u8>, &'static str), String> {
    let as_it_is = |mime| Ok((bytes.to_vec(), mime));
    match format {
        ImageFormat::Png => as_it_is("image/png"),
        ImageFormat::Jpeg => as_it_is("image/jpeg"),
        ImageFormat::Gif => as_it_is("image/gif"),
        ImageFormat::Webp => as_it_is("image/webp"),
        ImageFormat::Tiff => {
            let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Tiff).map_err(|e| format!("that TIFF can't be read: {e}"))?;
            let mut png = Vec::new();
            image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| format!("that TIFF can't be made a PNG: {e}"))?;
            Ok((png, "image/png"))
        }
        _ => Err("rein shows PNG, JPEG, GIF and WebP".to_owned()),
    }
}

impl Pane {
    /// What's in the box, and where the caret is in it.
    fn in_box(&self, cx: &App) -> (String, usize) {
        let input = self.input.read(cx);
        (input.value().to_string(), input.cursor())
    }

    /// Puts `text` in the box, with the caret at its end. The box says
    /// nothing of a value it's given, and leaves the caret at the start.
    pub(super) fn set_box(&mut self, text: impl Into<SharedString>, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.into();
        let end = text.len();
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.set_selected_range(end..end, cx);
        });
        self.box_changed(cx);
    }

    /// What's in the box has changed: the list may follow it.
    pub(super) fn box_changed(&mut self, cx: &mut Context<Self>) {
        let (value, caret) = self.in_box(cx);
        self.writing.recall.box_is(&value);
        if value.is_empty() {
            self.writing.left = None;
        }
        self.list_for(&value, caret, cx);
        cx.notify();
    }

    /// Works out what's listed for what's typed. An `@` first asks rein what
    /// it can point at, and lists once it's said.
    fn list_for(&mut self, value: &str, caret: usize, cx: &mut Context<Self>) {
        let at_word = word_at(value, caret, '@');
        if at_word.is_some() && self.writing.paths.is_none() && !self.writing.asked_paths {
            self.next_request += 1;
            if self.agent.as_mut().is_some_and(|agent| agent.ask_paths(self.next_request)) {
                self.sent.insert(self.next_request, Sent::Paths);
                self.writing.asked_paths = true;
            }
        }
        let typed = |word: &str| self.writing.left.as_deref() != Some(word);
        let over = if let Some((start, word)) = at_word.filter(|(_, word)| typed(word)) {
            let paths = self.writing.paths.clone();
            paths.map(|paths| Over::Paths { start, fits: fuzzy::best(word, &paths, LISTED).into_iter().map(str::to_owned).collect() })
        } else if let Some(word) = value.strip_prefix('/').filter(|word| typed(word) && !word.contains(char::is_whitespace)) {
            let mut fits = Command::matching(self.transcript.commands(), word);
            fits.truncate(LISTED);
            Some(Over::Commands(fits))
        } else if let Some((start, word)) = word_at(value, caret, '/').filter(|(start, word)| *start > 0 && typed(word)) {
            let mut fits = Command::skills_matching(self.transcript.commands(), word);
            fits.truncate(LISTED);
            Some(Over::Skills { start, fits })
        } else {
            None
        };
        self.writing.over = over.filter(|over| over.len() > 0);
        self.writing.at = 0;
        cx.notify();
    }

    /// The paths came, so an `@` being typed lists them.
    pub(super) fn paths_came(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        self.writing.paths = Some(Arc::new(paths));
        self.writing.asked_paths = false;
        let (value, caret) = self.in_box(cx);
        self.list_for(&value, caret, cx);
    }

    /// The name that's picked, and what's typed of it.
    fn picked(&self, value: &str, caret: usize) -> Option<(String, String, usize)> {
        let over = self.writing.over.as_ref()?;
        let at = self.writing.at.min(over.len().saturating_sub(1));
        let commands = self.transcript.commands();
        match over {
            Over::Commands(fits) => Some((commands.get(*fits.get(at)?)?.name.clone(), value.get(1..)?.to_owned(), 0)),
            Over::Paths { start, fits } => Some((fits.get(at)?.clone(), value.get(start + 1..caret)?.to_owned(), *start)),
            Over::Skills { start, fits } => Some((commands.get(*fits.get(at)?)?.name.clone(), value.get(start + 1..caret)?.to_owned(), *start)),
        }
    }

    /// Replaces what's typed from `start` to the caret, and goes on from it.
    fn put_in(&mut self, start: usize, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let (_, caret) = self.in_box(cx);
        self.input.update(cx, |input, cx| {
            input.set_selected_range(start..caret, cx);
            input.replace(text, window, cx);
        });
        self.box_changed(cx);
    }

    /// What's picked goes in the box, to go on with the message.
    fn complete_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (value, caret) = self.in_box(cx);
        let Some((name, _, start)) = self.picked(&value, caret) else { return false };
        let text = match &self.writing.over {
            Some(Over::Paths { .. }) => mention(&name),
            _ => format!("/{name} "),
        };
        self.put_in(start, text, window, cx);
        // Finished with: the word it's left is what the list was for.
        if !name.ends_with('/') {
            self.writing.over = None;
        }
        true
    }

    /// `⇥`: what's picked goes in the box. While a turn runs, with nothing
    /// listed, what's in the box goes after it.
    pub(super) fn tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.complete_pick(window, cx) || !self.running || self.asking.is_some() {
            return;
        }
        let value = self.input.read(cx).value().to_string();
        let text = super::to_send(&value).to_owned();
        if text.trim().is_empty() {
            return;
        }
        self.set_box("", window, cx);
        let images = self.writing.going_with(&text);
        self.writing.waits(text, images);
        cx.notify();
    }

    /// `↑` or `↓`, on its way to the box: the list has it if there's one,
    /// and otherwise, once the caret can go no further, what you sent. Past
    /// the first line it's the box's own.
    pub(super) fn arrow(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(over) = &self.writing.over {
            let count = over.len();
            self.writing.at = if up { self.writing.at.checked_sub(1).unwrap_or(count - 1) } else { (self.writing.at + 1) % count };
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let (value, before) = self.in_box(cx);
        // It takes the key on its way down, so what it did shows after.
        cx.defer_in(window, move |pane, window, cx| {
            let after = pane.input.read(cx).cursor();
            if after != before && !(if up { after == 0 } else { after == value.len() }) {
                return;
            }
            let found = if up { pane.writing.recall.older(&value) } else { pane.writing.recall.newer() };
            if let Some(text) = found {
                pane.set_box(text, window, cx);
            }
        });
    }

    /// Enter, on its way to the box: what's listed takes it, unless it's
    /// typed out already. A line ending in `\` goes on to the next.
    pub(super) fn enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if action.shift {
            return;
        }
        let (value, caret) = self.in_box(cx);
        if caret == value.len() && value.ends_with('\\') {
            cx.stop_propagation();
            self.put_in(caret - 1, "\n".to_owned(), window, cx);
            return;
        }
        let Some((name, typed, _)) = self.picked(&value, caret) else { return };
        if matches!(self.writing.over, Some(Over::Commands(_))) {
            cx.stop_propagation();
            self.set_box("", window, cx);
            self.run(&format!("/{name}"), Vec::new(), cx);
        } else if name != typed {
            cx.stop_propagation();
            self.complete_pick(window, cx);
        }
    }

    /// Esc, on its way to the box: it puts a list away, for this word.
    pub(super) fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        let (value, caret) = self.in_box(cx);
        if let Some((_, typed, _)) = self.picked(&value, caret) {
            self.writing.left = Some(typed);
            self.writing.over = None;
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// `^B`: with nothing typed, the command that's running goes on in the
    /// background. Otherwise it's the caret, a letter left.
    pub(super) fn send_to_background(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running && self.input.read(cx).value().is_empty() {
            self.next_request += 1;
            if let Some(agent) = &mut self.agent {
                agent.background(self.next_request);
            }
        } else {
            window.dispatch_action(Box::new(MoveLeft), cx);
        }
    }

    /// `^R`: what you sent, to find by its words.
    pub(super) fn find_sent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking.is_some() {
            return;
        }
        let sent: Vec<String> = self.writing.recall.newest_first().take(FOUND_MOST).map(str::to_owned).collect();
        if sent.is_empty() {
            return;
        }
        let items = sent.iter().map(|text| rein_transcript::Item { name: text.replace('\n', " "), about: String::new() }).collect();
        let menu = Menu { title: "find what you sent".to_owned(), items, current: None, at: None };
        self.open_finding(menu, sent, window, cx);
    }

    /// `^O`: every step's output, or none.
    pub(super) fn all_output(&mut self, cx: &mut Context<Self>) {
        self.all_open = !self.all_open;
        let rows = self.transcript.rows();
        self.scroller.update(cx, |scroller, cx| scroller.remeasure_items(0..rows, cx));
        cx.notify();
    }

    /// What's waiting to go back in the box, if anything is.
    pub(super) fn settle_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(back) = self.writing.to_box.take() else { return };
        cx.defer_in(window, move |pane, window, cx| {
            let (value, _) = pane.in_box(cx);
            let text = if value.is_empty() { back } else { format!("{back}\n{value}") };
            pane.set_box(text, window, cx);
        });
    }
}

/// What goes with a message: images, files.
impl Pane {
    /// `image`, which goes with your message, under a label in the box.
    fn attach(&mut self, bytes: &[u8], mime: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.writing.pasted += 1;
        let label = format!("[Image #{}]", self.writing.pasted);
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.writing.attached.push(Attached { label: label.clone(), mime, data });
        self.input.update(cx, |input, cx| input.insert(format!("{label} "), window, cx));
        self.box_changed(cx);
    }

    /// What was pasted, if the box shouldn't take it as text. Images go with
    /// the message; files are named, as a drop would.
    pub(super) fn pasted(&mut self, item: &ClipboardItem, window: &mut Window, cx: &mut Context<Self>) -> bool {
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    match as_shown(image.format, &image.bytes) {
                        Ok((bytes, mime)) => self.attach(&bytes, mime, window, cx),
                        Err(why) => {
                            let update = self.transcript.note(&format!("Nothing pasted: {why}"));
                            self.show(update.added, update.touched, cx);
                        }
                    }
                    return true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.add_paths(paths.paths().to_vec(), window, cx);
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// `+`: files to go with the message, chosen by you.
    pub(super) fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = PathPromptOptions { files: true, directories: true, multiple: true, prompt: None };
        let chosen = cx.prompt_for_paths(options);
        cx.spawn_in(window, async move |pane, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await {
                let _ = pane.update_in(cx, |pane, window, cx| pane.add_paths(paths, window, cx));
            }
        })
        .detach();
    }

    /// `paths` go with the message: an image as an image, wherever it is,
    /// and the rest named with `@`, as if typed.
    pub(super) fn add_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        for path in paths {
            if let Some(mime) = image_kind(&path) {
                let read = cx.background_spawn(async move { std::fs::read(path) });
                cx.spawn_in(window, async move |pane, cx| {
                    if let Ok(bytes) = read.await {
                        let _ = pane.update_in(cx, |pane, window, cx| pane.attach(&bytes, mime, window, cx));
                    }
                })
                .detach();
                continue;
            }
            let shown = path.strip_prefix(&self.about.cwd).unwrap_or(&path).display().to_string();
            // Nothing in `@` can hold a space, in the terminal either.
            if shown.contains(char::is_whitespace) {
                let update = self.transcript.note(&format!("rein can't point at {shown}: a space ends an @ word."));
                self.show(update.added, update.touched, cx);
                continue;
            }
            let folder = if path.is_dir() && !shown.ends_with('/') { "/" } else { "" };
            self.input.update(cx, |input, cx| input.insert(mention(&format!("{shown}{folder}")), window, cx));
            self.box_changed(cx);
        }
    }
}

/// What's drawn over the box.
impl Pane {
    /// What's listed for the word being typed.
    pub(super) fn listing(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let over = self.writing.over.as_ref()?;
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let at = self.writing.at.min(over.len().saturating_sub(1));
        let commands = self.transcript.commands();
        let rows: Vec<(String, String)> = match over {
            Over::Commands(fits) | Over::Skills { fits, .. } => fits.iter().filter_map(|&at| commands.get(at)).map(|one| (format!("/{}", one.name), one.about.clone())).collect(),
            Over::Paths { fits, .. } => fits.iter().map(|path| (path.clone(), String::new())).collect(),
        };
        let paths = matches!(over, Over::Paths { .. });
        let rows: Vec<Stateful<Div>> = rows
            .into_iter()
            .enumerate()
            .map(|(place, (name, about))| {
                let picked = place == at;
                div()
                    .id(("listed", place))
                    .flex()
                    .items_baseline()
                    .gap(px(10.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .when(picked, |row| row.bg(look.teal_tint))
                    .when(!picked, |row| row.hover(|style| style.bg(look.hover)))
                    .on_click(cx.listener(move |pane, _, window, cx| pane.pressed_listed(place, window, cx)))
                    .child(div().flex_none().font_weight(FontWeight::MEDIUM).when(paths, |row| row.font_family(mono.clone()).text_size(px(12.5))).child(name))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(if picked { look.ink_2 } else { look.ink_3 }).child(about))
            })
            .collect();
        Some(div().flex_none().ml(px(44.)).mr(px(28.)).mb(px(8.)).p(px(4.)).rounded(px(10.)).bg(look.float).border_1().border_color(look.line_2).children(rows))
    }

    /// A press on what's listed does what Enter would.
    fn pressed_listed(&mut self, place: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.writing.at = place;
        if matches!(self.writing.over, Some(Over::Commands(_))) {
            self.enter(&Enter { secondary: false, shift: false }, window, cx);
        } else {
            self.complete_pick(window, cx);
        }
    }

    /// What's on its way: what you said to the turn, and what goes after
    /// it. Pressing one of those brings it back to the box.
    pub(super) fn waiting(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.writing.steering.is_empty() && self.writing.later.is_empty() {
            return None;
        }
        let look = Look::of(cx);
        let line = |lead: &str, text: &str, then: &str| {
            let words = div().min_w_0().truncate().text_color(look.ink_2).child(text.to_owned());
            div().flex().items_baseline().gap(px(6.)).child(div().flex_none().child(lead.to_owned())).child(words).child(div().flex_none().child(then.to_owned()))
        };
        let steering = self.writing.steering.iter().map(|text| line("↳ steering:", waiting_shown(text), "· goes in at the next step").into_any_element());
        let later: Vec<AnyElement> = self
            .writing
            .later
            .iter()
            .enumerate()
            .map(|(at, later)| {
                let row = line("↳ after this turn:", waiting_shown(&later.text), "");
                row.id(("later", at)).cursor_pointer().on_click(cx.listener(move |pane, _, _, cx| pane.take_back(at, cx))).into_any_element()
            })
            .collect();
        Some(div().flex_none().flex().flex_col().gap(px(2.)).pl(px(56.)).pr(px(28.)).pb(px(8.)).text_size(px(12.5)).text_color(look.ink_3).children(steering).children(later))
    }

    /// The message waiting at `at` comes back to the box.
    fn take_back(&mut self, at: usize, cx: &mut Context<Self>) {
        if at < self.writing.later.len() {
            let later = self.writing.later.remove(at);
            self.writing.attached.extend(later.images);
            self.writing.to_box = Some(later.text);
            cx.notify();
        }
    }

    /// What's written, on its way to rein: a command or what can't steer a
    /// turn goes after it. `None` if nothing's in the box.
    pub(super) fn take_box(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<(String, Vec<Attached>)> {
        let value = self.input.read(cx).value().to_string();
        let text = super::to_send(&value).to_owned();
        if text.trim().is_empty() {
            return None;
        }
        self.set_box("", window, cx);
        // Rein keeps what may hold a key from ↑, and only it can tell.
        if !says_back(&text) {
            self.writing.recall.keep(&text);
        }
        let images = self.writing.going_with(&text);
        Some((text, images))
    }
}

#[cfg(test)]
impl Pane {
    pub(crate) fn listed_for_test(&self, cx: &App) -> Option<String> {
        let (value, caret) = self.in_box(cx);
        self.picked(&value, caret).map(|(name, ..)| name)
    }

    pub(crate) fn set_box_for_test(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.set_box(text.to_owned(), window, cx);
    }

    pub(crate) fn attach_for_test(&mut self, bytes: &[u8], window: &mut Window, cx: &mut Context<Self>) {
        self.attach(bytes, "image/png", window, cx);
    }

    pub(crate) fn attached_for_test(&self) -> usize {
        self.writing.attached.len()
    }

    pub(crate) fn waiting_for_test(&self) -> Vec<String> {
        self.writing.later.iter().map(|later| later.text.clone()).collect()
    }

    pub(crate) fn steering_for_test(&self) -> Vec<String> {
        self.writing.steering.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageFormat, Path, as_shown, image_kind};

    #[test]
    fn a_files_name_says_if_its_an_image_rein_can_show() {
        assert_eq!(image_kind(Path::new("/a/shot.PNG")), Some("image/png"));
        assert_eq!(image_kind(Path::new("photo.jpeg")), Some("image/jpeg"));
        assert_eq!(image_kind(Path::new("main.rs")), None);
        assert_eq!(image_kind(Path::new("noext")), None);
    }

    #[test]
    fn what_only_a_mac_copies_comes_out_a_png() {
        let image = image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]));
        let mut tiff = Vec::new();
        image::DynamicImage::ImageRgba8(image).write_to(&mut std::io::Cursor::new(&mut tiff), image::ImageFormat::Tiff).unwrap();
        let (png, mime) = as_shown(ImageFormat::Tiff, &tiff).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(&png[1..4], b"PNG");
        assert!(as_shown(ImageFormat::Tiff, b"not one").is_err());
        assert!(as_shown(ImageFormat::Svg, b"<svg/>").is_err(), "and what it can't show says so");
    }
}
