//! The settings page: your settings file, read out as sentences you can
//! change, over the whole window. It's the rein behind this pane that reads
//! and writes it, as it would read it starting: a value it wouldn't start
//! with is turned down, and said so where you set it. What's set is for new
//! conversations; your rules count at once. The window's look is its own,
//! and changes as you pick it.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::config::{Config, Key};

use super::{Pane, Sent, Told};
mod said;

use crate::look::Look;
use crate::parts::{Press, gauge, hairline, halo, lip};
use said::{count, hooks, keys, line, lines, lower_first, mono, sentence, servers, shown_choice, thousands, what_unset_means, words};

/// How wide the page's column is.
const COLUMN: f32 = 660.;
/// The most notes listed before "and N more".
const NOTES_MOST: usize = 12;
/// The most rules listed until you ask for them all: rein starts you with
/// thirty.
const RULES_MOST: usize = 6;
/// The most of the models listed under the model's value.
const MODELS_MOST: f32 = 240.;

/// The page's parts, in order: each is a child of what scrolls, so the
/// list at the top can scroll to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Models,
    Run,
    Skills,
    Hooks,
    Memory,
    Mcp,
    Long,
    Reins,
    Keys,
    Look,
}

impl Part {
    const ALL: [Part; 10] = [Part::Models, Part::Run, Part::Skills, Part::Hooks, Part::Memory, Part::Mcp, Part::Long, Part::Reins, Part::Keys, Part::Look];

    const fn name(self) -> &'static str {
        match self {
            Part::Models => "Models",
            Part::Run => "What may run",
            Part::Skills => "Skills",
            Part::Hooks => "Hooks",
            Part::Memory => "Memory",
            Part::Mcp => "MCP servers",
            Part::Long => "Long turns",
            Part::Reins => "Other reins",
            Part::Keys => "Keys",
            Part::Look => "Look",
        }
    }

    /// Where it is among what scrolls: the heading is first.
    fn at(self) -> usize {
        Part::ALL.iter().position(|part| *part == self).map_or(0, |at| at + 1)
    }

    /// The part a setting is set in, where what it says goes.
    const fn of(key: Key) -> Part {
        match key {
            Key::Model | Key::Effort | Key::Search => Part::Models,
            Key::Mode | Key::Sandbox => Part::Run,
            Key::Memory => Part::Memory,
            Key::CompactAt | Key::MaxSteps | Key::Check => Part::Long,
            Key::Peers | Key::Bell => Part::Reins,
            Key::Look => Part::Look,
        }
    }
}

/// The page, while it's open.
pub(super) struct Page {
    /// `None` until the rein behind this pane has answered.
    config: Option<Config>,
    /// What the rein said instead of the page.
    failed: Option<String>,
    /// The value open to change, under its sentence.
    editing: Option<Key>,
    /// What's typed for a value that takes typing.
    typed: Entity<InputState>,
    /// A rule to add, while its box is open.
    rule: Entity<InputState>,
    adding_rule: bool,
    /// Every rule shows, not only the first few.
    all_rules: bool,
    find: Entity<InputState>,
    /// What came of the last change, in the part it was made in.
    said: Option<(Part, String)>,
    keys: FocusHandle,
    scroll: ScrollHandle,
}

impl Pane {
    /// Whether the settings page is what this pane shows.
    pub(crate) const fn shows_page(&self) -> bool {
        self.page.is_some()
    }

    /// Opens the settings page, with the keys in it, and asks for what it
    /// shows.
    pub(crate) fn open_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page.is_some() {
            return;
        }
        let typed = cx.new(|cx| InputState::new(window, cx));
        let rule = cx.new(|cx| InputState::new(window, cx).placeholder("ask git push, allow cargo test, deny curl"));
        let find = cx.new(|cx| InputState::new(window, cx).placeholder("Find a skill"));
        cx.subscribe_in(&typed, window, |pane, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                pane.set_typed(cx);
            }
        })
        .detach();
        cx.subscribe_in(&rule, window, |pane, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                pane.add_rule(cx);
            }
        })
        .detach();
        cx.subscribe(&find, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        let keys = cx.focus_handle();
        keys.focus(window, cx);
        self.page =
            Some(Page { config: None, failed: None, editing: None, typed, rule, adding_rule: false, all_rules: false, find, said: None, keys, scroll: ScrollHandle::new() });
        self.ask_config();
        cx.emit(Told::Changed);
        cx.notify();
    }

    /// Closes it, and the keys go back to the box.
    pub(crate) fn close_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page.take().is_some() {
            self.input.read(cx).focus_handle(cx).focus(window, cx);
            cx.emit(Told::Changed);
            cx.notify();
        }
    }

    fn ask_config(&mut self) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_config(self.next_request)) {
            self.sent.insert(self.next_request, Sent::Config);
        } else if let Some(page) = &mut self.page {
            page.failed = Some("There's no conversation here yet to read your settings with: sign in first.".to_owned());
        }
    }

    /// What the rein behind it said the page is.
    pub(super) fn config_came(&mut self, said: Result<&serde_json::Value, String>, cx: &mut Context<Self>) {
        let Some(page) = &mut self.page else { return };
        match said {
            Ok(said) => match Config::of(said) {
                Some(config) => {
                    page.config = Some(config);
                    page.failed = None;
                }
                None => page.failed = Some("rein's answer wasn't a settings page.".to_owned()),
            },
            Err(why) => page.failed = Some(why),
        }
        cx.notify();
    }

    /// Sets `key` to `value`, or back to rein's own with `None`.
    fn set_key(&mut self, key: Key, value: Option<String>, cx: &mut Context<Self>) {
        self.next_request += 1;
        let id = self.next_request;
        if self.agent.as_mut().is_some_and(|agent| agent.set_config(id, key.as_str(), value.as_deref())) {
            self.sent.insert(id, Sent::SetConfig(key, value));
        }
        cx.notify();
    }

    /// What came of setting `key`: done, the page is asked for again.
    pub(super) fn key_set(&mut self, key: Key, value: Option<String>, came_to: Result<(), String>, cx: &mut Context<Self>) {
        let Some(page) = &mut self.page else { return };
        match came_to {
            Ok(()) => {
                page.editing = None;
                page.said = None;
                if key == Key::Look {
                    cx.emit(Told::Look(value.unwrap_or_else(|| "follow".to_owned())));
                }
                self.ask_config();
            }
            Err(why) => page.said = Some((Part::of(key), why)),
        }
        cx.notify();
    }

    /// Sets what's typed for the value that's open.
    fn set_typed(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &self.page else { return };
        let Some(key) = page.editing else { return };
        let value = page.typed.read(cx).value().trim().to_owned();
        if !value.is_empty() {
            self.set_key(key, Some(value), cx);
        }
    }

    /// Opens `key` to change under its sentence, or closes it.
    fn edit(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = &mut self.page else { return };
        page.said = None;
        if page.editing == Some(key) {
            page.editing = None;
        } else {
            page.editing = Some(key);
            if key.typed() {
                let now = page.config.as_ref().and_then(|config| config.starts(key)).unwrap_or_default().to_owned();
                page.typed.update(cx, |input, cx| input.set_value(now, window, cx));
                page.typed.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        cx.notify();
    }

    /// Adds what's in the rule's box. It stays there until rein has taken
    /// it: one turned down is there to fix.
    fn add_rule(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &self.page else { return };
        let line = page.rule.read(cx).value().trim().to_owned();
        if !line.is_empty() {
            self.change_rule(line, true, cx);
        }
    }

    fn change_rule(&mut self, rule: String, add: bool, cx: &mut Context<Self>) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.rule(self.next_request, &rule, add)) {
            self.sent.insert(self.next_request, Sent::Rule);
        }
        cx.notify();
    }

    /// What came of adding or taking out a rule.
    pub(super) fn rule_changed(&mut self, came_to: Result<(), String>, cx: &mut Context<Self>) {
        let Some(page) = &mut self.page else { return };
        match came_to {
            Ok(()) => {
                page.said = None;
                page.adding_rule = false;
                self.ask_config();
            }
            Err(why) => page.said = Some((Part::Run, why)),
        }
        cx.notify();
    }

    /// The page, over the whole window.
    pub(super) fn page_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let look = Look::of(cx);
        let Some(page) = &self.page else { return div().into_any_element() };
        let close = Press::new("page-close").label("Close").key("esc").on_click(cx.listener(|pane, _, window, cx| pane.close_page(window, cx)));
        let top = div().flex().flex_none().items_center().h(px(44.)).pl(px(14.)).pr(px(12.)).child(crate::panes::drag_area("drag-page").flex_1().h_full()).child(close);
        let column = |element: AnyElement| div().w_full().max_w(px(COLUMN)).child(element);
        let mut body = div()
            .id("page")
            .flex()
            .flex_col()
            .items_center()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&page.scroll)
            .mx(px(6.))
            .mb(px(6.))
            .px(px(24.))
            .pt(px(56.))
            .pb(px(72.))
            .gap(px(40.))
            .rounded(px(10.))
            .bg(look.bg)
            .shadow(vec![halo(look.line, 1.)])
            .child(column(self.page_heading(page, cx).into_any_element()));
        match (&page.config, &page.failed) {
            (_, Some(why)) => body = body.child(column(div().text_size(px(15.)).text_color(look.red).child(why.clone()).into_any_element())),
            (None, None) => body = body.child(column(div().text_size(px(15.)).text_color(look.ink_3).child("Reading your settings…").into_any_element())),
            (Some(config), None) => {
                for part in Part::ALL {
                    let section = self.part(part, config, page, cx);
                    body = body.child(column(section.into_any_element()));
                }
            }
        }
        div()
            .id("settings")
            .track_focus(&page.keys)
            .on_key_down(cx.listener(|pane, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    let editing = pane.page.as_mut().and_then(|page| page.editing.take().map(|_| ()).or_else(|| std::mem::take(&mut page.adding_rule).then_some(())));
                    if editing.is_some() {
                        cx.notify();
                    } else {
                        pane.close_page(window, cx);
                    }
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(look.desk)
            .text_color(look.ink)
            .child(top)
            .child(body)
            .into_any_element()
    }

    fn page_heading(&self, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let file = page.config.as_ref().map_or_else(|| "~/.config/rein/config".to_owned(), |config| config.file.clone());
        let toc = Part::ALL.iter().enumerate().map(|(index, part)| {
            let part = *part;
            div()
                .id(("toc", index))
                .cursor_pointer()
                .hover(|style| style.text_color(look.ink))
                .on_click(cx.listener(move |pane, _, _, cx| {
                    if let Some(page) = &pane.page {
                        page.scroll.scroll_to_top_of_item(part.at());
                        cx.notify();
                    }
                }))
                .child(part.name())
        });
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(div().text_size(px(28.)).line_height(px(34.)).font_weight(FontWeight::BOLD).child("Settings"))
            .child(
                sentence(&look)
                    .text_color(look.ink_2)
                    .children(words("This page is"))
                    .child(mono(&file, &look))
                    .children(words(", read out as sentences. Change it here or in the file. Underlined words open what they name.")),
            )
            .child(div().flex().flex_wrap().gap_x(px(14.)).gap_y(px(2.)).text_size(px(13.)).text_color(look.ink_3).children(toc.collect::<Vec<_>>()))
    }

    fn part(&self, part: Part, config: &Config, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let said = page.said.as_ref().filter(|(at, _)| *at == part).map(|(_, why)| div().text_size(px(13.)).text_color(look.red).child(why.clone()));
        let body = match part {
            Part::Models => self.models(config, page, cx),
            Part::Run => self.run_part(config, page, cx),
            Part::Skills => self.skills(config, page, cx),
            Part::Hooks => hooks(config, &look),
            Part::Memory => self.memory(config, page, cx),
            Part::Mcp => servers(config, &look),
            Part::Long => self.long_turns(config, page, cx),
            Part::Reins => self.reins(config, page, cx),
            Part::Keys => keys(&look),
            Part::Look => self.looks(config, cx),
        };
        div().flex().flex_col().gap(px(12.)).child(div().text_size(px(17.)).line_height(px(24.)).font_weight(FontWeight::SEMIBOLD).child(part.name())).child(body).children(said)
    }

    /// An underlined value that opens `key` to change.
    fn value(&self, key: Key, shown: impl Into<SharedString>, cx: &mut Context<Self>) -> Stateful<Div> {
        let look = Look::of(cx);
        let open = self.page.as_ref().is_some_and(|page| page.editing == Some(key));
        div()
            .id(("value", Key::ALL.iter().position(|one| *one == key).unwrap_or(0)))
            .px(px(1.))
            .font_family(cx.global::<crate::parts::Monospace>().0.clone())
            .text_size(px(13.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .border_b(px(1.5))
            .border_color(if open { look.ink } else { look.line_2 })
            .hover(|style| style.border_color(look.ink))
            .on_click(cx.listener(move |pane, _, window, cx| pane.edit(key, window, cx)))
            .child(shown.into())
    }

    /// What `key` can be, under its sentence, while it's open: its choices,
    /// or a box to type it in, and back to rein's own.
    fn editor(&self, key: Key, config: &Config, page: &Page, cx: &mut Context<Self>) -> Option<Div> {
        if page.editing != Some(key) {
            return None;
        }
        let look = Look::of(cx);
        let current = config.in_file(key).map(str::to_owned);
        let unset = Press::new(("unset", key as usize)).ghost().small().label("rein's own").tip("Take it out of the file, so rein's default holds").on_click(cx.listener(
            move |pane, _, _, cx| {
                pane.set_key(key, None, cx);
            },
        ));
        let over = config.over(key).map(|env| div().text_size(px(12.5)).text_color(look.amber_text).child(format!("{env} is set, and wins over the file until it's unset.")));
        let body = if key == Key::Model {
            let models = self.setting("model").map(|model| model.choices.clone()).unwrap_or_default();
            let rows = models.into_iter().enumerate().map(|(index, choice)| {
                let on = current.as_deref() == Some(choice.value.as_str());
                let value = choice.value.clone();
                div()
                    .id(("model-row", index))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(30.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|style| style.bg(look.hover))
                    .when(on, |row| row.bg(look.lit))
                    .on_click(cx.listener(move |pane, _, _, cx| pane.set_key(Key::Model, Some(value.clone()), cx)))
                    .child(div().flex_1().min_w_0().truncate().child(choice.name.clone()))
                    .child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(choice.group.clone().unwrap_or_default()))
            });
            div().id("models").flex().flex_col().max_h(px(MODELS_MOST)).overflow_y_scroll().children(rows.collect::<Vec<_>>()).into_any_element()
        } else if key.typed() {
            let set = Press::new(("set", key as usize)).small().label("Set").key("⏎").on_click(cx.listener(|pane, _, _, cx| pane.set_typed(cx)));
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .h(px(30.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .rounded(px(7.))
                        .bg(look.bg)
                        .shadow(vec![hairline(look.ring)])
                        .child(Input::new(&page.typed).appearance(false).text_size(px(13.))),
                )
                .child(set)
                .into_any_element()
        } else {
            let presses = config.choices(key).into_iter().enumerate().map(|(index, choice)| {
                let on = config.starts(key) == Some(choice.as_str());
                let value = choice.clone();
                let press = Press::new(("choice", key as usize * 16 + index)).small().label(shown_choice(key, &choice));
                // What new conversations start with now stands out.
                let press = if on { press.primary() } else { press };
                press.on_click(cx.listener(move |pane, _, _, cx| pane.set_key(key, Some(value.clone()), cx)))
            });
            let about = (key == Key::Mode).then(|| {
                let rows = config.modes.iter().map(|mode| {
                    div()
                        .flex()
                        .gap(px(8.))
                        .text_size(px(12.5))
                        .child(div().w(px(56.)).flex_none().font_weight(FontWeight::MEDIUM).child(mode.id.clone()))
                        .child(div().flex_1().text_color(look.ink_3).child(mode.about.clone()))
                });
                div().flex().flex_col().gap(px(3.)).children(rows.collect::<Vec<_>>())
            });
            div().flex().flex_col().gap(px(8.)).child(div().flex().flex_wrap().gap(px(6.)).children(presses.collect::<Vec<_>>())).children(about).into_any_element()
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(10.))
                .rounded(px(10.))
                .bg(look.pane_2)
                .shadow(vec![hairline(look.line)])
                .child(body)
                .child(div().flex().items_center().gap(px(8.)).child(unset).child(div().text_size(px(12.)).text_color(look.ink_3).child(what_unset_means(key))))
                .children(over),
        )
    }

    fn models(&self, config: &Config, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let model = config.starts(Key::Model).map_or_else(|| "rein's pick".to_owned(), |model| model.rsplit('/').next().unwrap_or(model).to_owned());
        let effort = config.starts(Key::Effort).unwrap_or("default").to_owned();
        let search = config.starts(Key::Search).unwrap_or("auto").to_owned();
        let said = sentence(&look)
            .children(words("New conversations start on"))
            .child(self.value(Key::Model, model, cx))
            .children(words("at"))
            .child(self.value(Key::Effort, effort, cx))
            .children(words("effort. Web search is"))
            .child(self.value(Key::Search, search, cx))
            .children(words(match config.starts(Key::Search) {
                Some("off") => ": the model never searches.",
                Some("auto") | None => ": rein decides who searches, the model's provider or another.",
                Some(_) => ": that model searches for every other.",
            }));
        let meters = (!self.meters.is_empty()).then(|| {
            let rows = self.meters.iter().map(|meter| {
                let fill = if meter.warn { look.amber } else { look.ink_3 };
                let shown = meter.resets.as_ref().map_or_else(|| meter.said.clone(), |resets| format!("{} →{resets}", meter.said));
                let row = div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(20.))
                    .text_size(px(12.))
                    .text_color(if meter.warn { look.amber_text } else { look.ink_3 })
                    .child(div().w(px(24.)).child(meter.label.clone()));
                match meter.used {
                    Some(used) => row.child(gauge(f32::from(used) / 100., Some(160.), fill, &look)).child(div().child(shown)),
                    None => row.child(div().child(shown)),
                }
            });
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(13.)).text_color(look.ink_2).child("How much of your plan this conversation's provider says is used:"))
                .children(rows.collect::<Vec<_>>())
        });
        let editing = [Key::Model, Key::Effort, Key::Search].into_iter().find_map(|key| self.editor(key, config, page, cx));
        div().flex().flex_col().gap(px(12.)).child(said).children(editing).children(meters)
    }

    fn run_part(&self, config: &Config, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let mode = config.starts(Key::Mode).unwrap_or("trust").to_owned();
        let about = config.modes.iter().find(|one| one.id == mode).map(|one| one.about.clone()).unwrap_or_default();
        let sandbox = config.starts(Key::Sandbox) == Some("on");
        let dangerous = mode == "trust" || (mode == "auto" && !sandbox);
        let shown = if dangerous { format!("⚠ {mode}") } else { mode.clone() };
        let said = sentence(&look)
            .children(words("New conversations start in"))
            .child(self.value(Key::Mode, shown, cx))
            .children(words(&format!(": {}", lower_first(&about).trim_end_matches('.'))))
            .children(words(". The sandbox is"))
            .child(self.value(Key::Sandbox, if sandbox { "on" } else { "off" }, cx))
            .children(words(if sandbox { ", so commands run inside walls." } else { ", so nothing holds a command to what it says it needs." }));
        let editing = [Key::Mode, Key::Sandbox].into_iter().find_map(|key| self.editor(key, config, page, cx));
        let most = if page.all_rules { usize::MAX } else { RULES_MOST };
        let more = config.rules.len().saturating_sub(most);
        let more = (more > 0).then(|| {
            line(&look)
                .id("rules-more")
                .cursor_pointer()
                .text_color(look.ink_3)
                .hover(|style| style.text_color(look.ink))
                .on_click(cx.listener(|pane, _, _, cx| {
                    if let Some(page) = &mut pane.page {
                        page.all_rules = true;
                        cx.notify();
                    }
                }))
                .child(div().w(px(40.)).flex_none())
                .child(format!("{more} more"))
        });
        let rows = config.rules.iter().take(most).enumerate().map(|(index, rule)| {
            let tone = match rule.kind.as_str() {
                "deny" => look.red,
                "ask" => look.amber_text,
                _ => look.ink_2,
            };
            let take_out = rule.yours.then(|| {
                let line = rule.rule.clone();
                Press::new(("rule-out", index)).ghost().small().label("Remove").on_click(cx.listener(move |pane, _, _, cx| pane.change_rule(line.clone(), false, cx)))
            });
            line(&look)
                .child(
                    div()
                        .w(px(40.))
                        .flex_none()
                        .font_family(cx.global::<crate::parts::Monospace>().0.clone())
                        .text_size(px(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tone)
                        .child(rule.kind.clone()),
                )
                .child(div().flex_1().min_w_0().truncate().font_family(cx.global::<crate::parts::Monospace>().0.clone()).text_size(px(12.5)).child(rule.what.clone()))
                .child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(if rule.yours { "every project" } else { "this project" }))
                .children(take_out)
        });
        let add = if page.adding_rule {
            line(&look)
                .child(div().w(px(40.)).flex_none().text_color(look.ink_3).child("+"))
                .child(div().flex_1().child(Input::new(&page.rule).appearance(false).text_size(px(12.5))))
                .child(Press::new("rule-add").small().label("Add").key("⏎").on_click(cx.listener(|pane, _, _, cx| pane.add_rule(cx))))
                .into_any_element()
        } else {
            line(&look)
                .id("rule-new")
                .cursor_pointer()
                .text_color(look.ink_3)
                .hover(|style| style.text_color(look.ink))
                .on_click(cx.listener(|pane, _, window, cx| {
                    if let Some(page) = &mut pane.page {
                        page.adding_rule = true;
                        // Empty, as what was in it was added.
                        page.rule.update(cx, |input, cx| input.set_value("", window, cx));
                        page.rule.read(cx).focus_handle(cx).focus(window, cx);
                        cx.notify();
                    }
                }))
                .child(div().w(px(40.)).flex_none().child("+"))
                .child("Add a rule")
                .into_any_element()
        };
        div().flex().flex_col().gap(px(12.)).child(said).children(editing).child(lines(&look).children(rows.collect::<Vec<_>>()).children(more).child(add))
    }

    fn skills(&self, config: &Config, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let wanted: Vec<String> = page.find.read(cx).value().to_lowercase().split_whitespace().map(str::to_owned).collect();
        let fits = |name: &str| wanted.iter().all(|word| name.to_lowercase().contains(word.as_str()));
        let shown: Vec<_> = config.skills.iter().filter(|skill| fits(&skill.name)).collect();
        let found = (!wanted.is_empty()).then(|| div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(format!("{} found", shown.len())));
        let names = shown.iter().map(|skill| {
            div()
                .w(relative(1. / 3.))
                .pr(px(18.))
                .truncate()
                .text_size(px(13.))
                .line_height(px(25.))
                .when(!skill.on, |name| name.text_color(look.ink_3).line_through())
                .child(skill.name.clone())
        });
        let whose = |whose: &str| config.skills.iter().filter(|skill| skill.whose == whose).count();
        let from: Vec<String> = [("yours", "you"), ("plugin", "your plugins"), ("project", "this project")]
            .iter()
            .filter(|(whose_id, _)| whose(whose_id) > 0)
            .map(|(_, name)| (*name).to_owned())
            .collect();
        let from = match from.as_slice() {
            [] => String::new(),
            [one] => format!(", from {one}"),
            [rest @ .., last] => format!(", from {} and {last}", rest.join(", ")),
        };
        if config.skills.is_empty() {
            let none =
                "None here yet. A skill is a folder with a SKILL.md: yours go in ~/.claude/skills or ~/.agents/skills, a project's in its own .agents/skills or .claude/skills.";
            return div().flex().flex_col().child(sentence(&look).text_color(look.ink_2).children(words(none)));
        }
        let said = sentence(&look)
            .children(words(&format!("{}{from}. One that's struck through is turned off. Call any of them with", config.skills.len())))
            .child(mono("/name", &look))
            .children(words("."));
        let field = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(36.))
            .border_t_1()
            .border_color(look.line)
            .child(div().flex_1().child(Input::new(&page.find).appearance(false).text_size(px(13.))))
            .children(found);
        div().flex().flex_col().gap(px(12.)).child(said).child(field).child(div().flex().flex_wrap().children(names.collect::<Vec<_>>()))
    }

    fn memory(&self, config: &Config, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let on = config.starts(Key::Memory) != Some("off");
        let said = sentence(&look).children(words("Memory is")).child(self.value(Key::Memory, if on { "on" } else { "off" }, cx)).children(words(&format!(
            ": {} kept across conversations, and every save is a step you can see. Claude Code's are read, never written.",
            count(config.notes.len(), "note", "notes")
        )));
        let rows = config.notes.iter().take(NOTES_MOST).map(|note| {
            let whose = if note.reins { note.modified.as_ref().map_or_else(|| "rein's".to_owned(), |day| format!("rein's · {day}")) } else { "Claude Code's".to_owned() };
            line(&look)
                .child(div().flex_1().min_w_0().truncate().child(if note.description.is_empty() { note.name.clone() } else { note.description.clone() }))
                .child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(whose))
        });
        let more = config.notes.len().checked_sub(NOTES_MOST).filter(|&more| more > 0).map(|more| line(&look).text_color(look.ink_3).child(format!("and {more} more")));
        let list = (!config.notes.is_empty()).then(|| lines(&look).children(rows.collect::<Vec<_>>()).children(more));
        div().flex().flex_col().gap(px(12.)).child(said).children(self.editor(Key::Memory, config, page, cx)).children(list)
    }

    fn long_turns(&self, config: &Config, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let compact = config.starts(Key::CompactAt).map_or_else(|| "150,000".to_owned(), thousands);
        let steps = config.starts(Key::MaxSteps).unwrap_or("200").to_owned();
        let check = match config.starts(Key::Check).map(str::trim) {
            None | Some("" | "on") => "the project's own check".to_owned(),
            Some("off") => "nothing".to_owned(),
            Some(command) => command.to_owned(),
        };
        let said = sentence(&look)
            .children(words("rein makes room at"))
            .child(self.value(Key::CompactAt, compact, cx))
            .children(words("tokens. A turn nobody watches stops after"))
            .child(self.value(Key::MaxSteps, steps.clone(), cx))
            .children(words(if steps == "off" { "steps: never, that is. After edits it runs" } else { "steps. After edits it runs" }))
            .child(self.value(Key::Check, check, cx))
            .children(words(", and tells the model only what that turn broke."));
        let editing = [Key::CompactAt, Key::MaxSteps, Key::Check].into_iter().find_map(|key| self.editor(key, config, page, cx));
        let hint = editing.is_some().then(|| {
            let says = match page.editing {
                Some(Key::CompactAt) => "A number of tokens. It's never later than 90% of a model's window.",
                Some(Key::MaxSteps) => "A number of steps, or off for no limit.",
                _ => "A command to run, off for none, or on for the one rein finds in the project.",
            };
            div().text_size(px(12.)).text_color(look.ink_3).child(says)
        });
        div().flex().flex_col().gap(px(12.)).child(said).children(editing).children(hint)
    }

    fn reins(&self, config: &Config, page: &Page, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let peers = config.starts(Key::Peers).unwrap_or("accept").to_owned();
        let bell = config.starts(Key::Bell) != Some("off");
        let first = if peers == "off" {
            sentence(&look).children(words("Other reins are")).child(self.value(Key::Peers, "off", cx)).children(words(": this one doesn't join them."))
        } else {
            sentence(&look).children(words("When another rein writes,")).child(self.value(Key::Peers, peers.clone(), cx)).children(words(match peers.as_str() {
                "hold" => "it, for you to read in /inbox.",
                "refuse" => "it.",
                _ => "it: it goes into the conversation.",
            }))
        };
        let bell = sentence(&look)
            .children(words("The bell is"))
            .child(self.value(Key::Bell, if bell { "on" } else { "off" }, cx))
            .children(words("for when it needs you and you're away."));
        let editing = [Key::Peers, Key::Bell].into_iter().find_map(|key| self.editor(key, config, page, cx));
        div().flex().flex_col().gap(px(12.)).child(first).child(bell).children(editing)
    }

    fn looks(&self, config: &Config, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let wearing = config.starts(Key::Look).unwrap_or("follow").to_owned();
        let tiles = [("light", "Light"), ("dark", "Dark"), ("follow", "Follow the Mac")].into_iter().enumerate().map(|(index, (id, name))| {
            let tile = if id == "dark" { Look::dark() } else { Look::light() };
            let on = wearing == id;
            let mini = div()
                .flex()
                .gap(px(3.))
                .w(px(148.))
                .h(px(94.))
                .pt(px(4.))
                .pb(px(4.))
                .pr(px(4.))
                .rounded(px(8.))
                .bg(tile.desk)
                .shadow(vec![hairline(tile.ring), lip(tile.lip)])
                .when(on, |mini| mini.border_2().border_color(look.ink))
                .child(div().flex_none().w(px(34.)))
                .child(div().flex_1().rounded(px(4.)).bg(tile.bg).shadow(vec![halo(tile.line, 1.)]));
            div()
                .id(("look", index))
                .flex()
                .flex_col()
                .gap(px(6.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(if on { look.ink } else { look.ink_2 })
                .on_click(cx.listener(move |pane, _, _, cx| pane.set_key(Key::Look, Some(id.to_owned()), cx)))
                .child(mini)
                .child(name)
        });
        div().flex().flex_wrap().gap(px(16.)).children(tiles.collect::<Vec<_>>())
    }
}

/// What a test of the page reaches for that nothing else needs.
#[cfg(test)]
impl Pane {
    /// Picks `value` for `key`, as its press does.
    pub(crate) fn set_key_for_test(&mut self, key: Key, value: Option<&str>, cx: &mut Context<Self>) {
        self.set_key(key, value.map(str::to_owned), cx);
    }

    /// Writes `text` as a rule to add, and adds it, as Enter does.
    pub(crate) fn add_rule_for_test(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &mut self.page {
            page.adding_rule = true;
            page.rule.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
        self.add_rule(cx);
    }

    /// What the rule's box holds.
    pub(crate) fn rule_box_for_test(&self, cx: &App) -> String {
        self.page.as_ref().map(|page| page.rule.read(cx).value().to_string()).unwrap_or_default()
    }

    /// What the page says came of the last change, if anything.
    pub(crate) fn page_said_for_test(&self) -> Option<String> {
        self.page.as_ref().and_then(|page| page.said.as_ref().map(|(_, why)| why.clone()))
    }

    /// What the page shows new conversations start with for `key`.
    pub(crate) fn page_starts_for_test(&self, key: Key) -> Option<String> {
        self.page.as_ref().and_then(|page| page.config.as_ref()).and_then(|config| config.starts(key).map(str::to_owned))
    }
}

#[cfg(test)]
mod tests {
    use super::Part;
    use rein_transcript::config::Key;

    #[test]
    fn every_setting_is_set_in_a_part_of_the_page() {
        for key in Key::ALL {
            assert!(Part::ALL.contains(&Part::of(key)));
        }
        assert_eq!(Part::Models.at(), 1, "the heading comes first");
    }
}
