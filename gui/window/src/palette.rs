//! `⌘K`: go to an agent, or run a command. It's the same list as typing a `/`,
//! under the names people look for: nothing here runs that the box couldn't.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::{Command, Item, Menu};

use crate::look::Look;
use crate::pane::Pane;
use crate::panes::Panes;

/// How wide it is, how far down the window it sits, and as many rows as show
/// before it scrolls.
const WIDE: f32 = 640.;
const DOWN: f32 = 96.;
const LIST_MOST: f32 = 420.;

/// A command, what it goes by, and what's said of it.
type Named = (&'static str, &'static str, &'static str);

/// What each command goes by here, and the part of the list it's in. A
/// command rein has that isn't named here shows under its own name, in the
/// last part, so a new one is never missing.
const NAMED: [(&str, &[Named]); 4] = [
    (
        "This conversation",
        &[
            ("model", "Change the model", "and how hard it thinks"),
            ("mode", "Change the mode", "what runs without asking"),
            ("sandbox", "Sandbox", "on or off, in any mode"),
            ("compact", "Make room", "compact, with a focus if you like"),
            ("undo", "Undo", "put files back, by turn"),
            ("changes", "Changes", "what's been changed, file by file"),
            ("copy", "Copy the last answer", "as it wrote it"),
            ("status", "Status", "what is in use"),
            ("search", "Web search", "which model searches"),
            ("new", "New conversation", ""),
            ("resume", "Saved conversations", ""),
        ],
    ),
    (
        "Settings",
        &[
            ("skills", "Skills", "use, turn off, or keep for you to call"),
            ("hooks", "Hooks", "what each runs on, how it last did"),
            ("memory", "Memory", "notes kept across conversations"),
            ("mcp", "MCP servers", "add, bring from other apps, change"),
            ("permissions", "Permissions", "what asks first, or is refused"),
            ("check", "Check after edits", "what runs, on or off"),
            ("tasks", "Background commands", "see or stop one"),
        ],
    ),
    (
        "Other reins",
        &[
            ("peers", "Reins on this computer", "and what each is doing"),
            ("send", "Write to a rein", ""),
            ("delegate", "Hand a rein a task", "its result comes back here"),
            ("spawn", "Start a sub-agent", "in a pane of its own"),
            ("inbox", "Inbox", "what waits to be let in"),
            ("name", "Name this rein", ""),
            ("workspace", "Workspace", "the group it works with"),
        ],
    ),
    ("Account", &[("login", "Sign in", "a provider, or see which"), ("logout", "Sign out", "")]),
];

/// A row, and what it does.
#[derive(Clone)]
enum Does {
    GoTo(Entity<Pane>),
    Run(String),
}

struct Row {
    section: &'static str,
    /// What's on the left: `/model`, or nothing for an agent.
    slash: String,
    name: String,
    says: String,
    does: Does,
}

/// What's looked for: the box you type in, and which row's picked.
pub(crate) struct Palette {
    input: Entity<InputState>,
    at: usize,
}

/// The rows for what `commands` holds, in the design's order, then the rest.
fn command_rows(commands: &[Command]) -> Vec<Row> {
    let mut rows = Vec::with_capacity(commands.len());
    let named = |name: &str| NAMED.iter().any(|(_, listed)| listed.iter().any(|(command, ..)| *command == name));
    for (section, listed) in NAMED {
        for (command, name, says) in listed {
            if commands.iter().any(|one| one.name == *command) {
                rows.push(Row { section, slash: format!("/{command}"), name: (*name).to_owned(), says: (*says).to_owned(), does: Does::Run(format!("/{command}")) });
            }
        }
    }
    for command in commands.iter().filter(|command| !named(&command.name)) {
        let section = if command.skill { "Skills" } else { "More" };
        rows.push(Row { section, slash: format!("/{}", command.name), name: command.about.clone(), says: String::new(), does: Does::Run(format!("/{}", command.name)) });
    }
    rows
}

impl Panes {
    /// Everything it lists, in order.
    fn palette_rows(&self, cx: &App) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .panes()
            .into_iter()
            .map(|pane| {
                let read = pane.read(cx);
                Row { section: "Agents", slash: String::new(), name: read.title(), says: read.standing().word().to_owned(), does: Does::GoTo(pane.clone()) }
            })
            .collect();
        if let Some(pane) = self.looking_at(cx) {
            rows.extend(command_rows(pane.read(cx).commands()));
        }
        rows
    }

    /// Those of `rows` that fit what's typed, in their order.
    fn palette_fits(palette: &Palette, rows: &[Row], cx: &App) -> Vec<usize> {
        let items = rows.iter().map(|row| Item { name: row.name.clone(), about: format!("{} {}", row.slash, row.says) }).collect();
        let menu = Menu { title: String::new(), items, current: None, at: None };
        let mut fits = menu.matching(&palette.input.read(cx).value());
        fits.sort_unstable();
        fits
    }

    /// `⌘K`: opens it, or puts it away.
    pub(crate) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Go to an agent, or run a command"));
        cx.subscribe_in(&input, window, |view, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => view.take_palette(window, cx),
            InputEvent::Change => {
                if let Some(palette) = &mut view.palette {
                    palette.at = 0;
                }
                cx.notify();
            }
            _ => {}
        })
        .detach();
        input.update(cx, |input, cx| input.focus(window, cx));
        self.palette = Some(Palette { input, at: 0 });
        cx.notify();
    }

    /// Puts it away, and gives the keys back to where you were.
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        match self.looking_at(cx) {
            Some(pane) => pane.update(cx, |pane, cx| pane.take_keys(window, cx)),
            None => self.focus.focus(window, cx),
        }
        cx.notify();
    }

    fn take_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = &self.palette else { return };
        let rows = self.palette_rows(cx);
        let fits = Self::palette_fits(palette, &rows, cx);
        let Some(does) = fits.get(palette.at).and_then(|&at| rows.get(at)).map(|row| row.does.clone()) else { return };
        self.close_palette(window, cx);
        match does {
            Does::GoTo(pane) => self.look_at(&pane, true, window, cx),
            Does::Run(line) => {
                if let Some(pane) = self.looking_at(cx) {
                    pane.update(cx, |pane, cx| pane.run_chosen(&line, window, cx));
                }
            }
        }
    }

    fn palette_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = &self.palette else { return };
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => self.close_palette(window, cx),
            "up" | "down" => {
                let rows = self.palette_rows(cx);
                let count = Self::palette_fits(palette, &rows, cx).len();
                if let Some(palette) = self.palette.as_mut().filter(|_| count > 0) {
                    palette.at = if key == "up" { palette.at.checked_sub(1).unwrap_or(count - 1) } else { (palette.at + 1) % count };
                }
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// It, if it's up, over everything.
    pub(crate) fn palette_card(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let palette = self.palette.as_ref()?;
        let look = Look::of(cx);
        let mono = cx.global::<crate::parts::Monospace>().0.clone();
        let rows = self.palette_rows(cx);
        let fits = Self::palette_fits(palette, &rows, cx);
        let typed = !palette.input.read(cx).value().is_empty();
        let count = typed.then(|| div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(format!("{} of {}", fits.len(), rows.len())));
        let mut listed: Vec<AnyElement> = Vec::with_capacity(fits.len() + 5);
        let mut section = "";
        for (place, &at) in fits.iter().enumerate() {
            let row = &rows[at];
            if row.section != section {
                section = row.section;
                listed.push(div().px(px(8.)).pt(px(10.)).pb(px(4.)).text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(look.ink_3).child(section).into_any_element());
            }
            let picked = place == palette.at;
            listed.push(
                div()
                    .id(("palette", place))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(30.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(px(7.))
                    .when(picked, |it| it.bg(look.teal_tint))
                    .when(!picked, |it| it.hover(|style| style.bg(look.hover)))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        if let Some(palette) = &mut view.palette {
                            palette.at = place;
                        }
                        view.take_palette(window, cx);
                    }))
                    .when(!row.slash.is_empty(), |it| {
                        it.child(div().flex_none().w(px(96.)).font_family(mono.clone()).text_size(px(12.)).font_weight(FontWeight::MEDIUM).child(row.slash.clone()))
                    })
                    .child(div().min_w_0().truncate().child(row.name.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(if picked { look.ink_2 } else { look.ink_3 }).child(row.says.clone()))
                    .into_any_element(),
            );
        }
        if fits.is_empty() {
            listed.push(div().px(px(8.)).py(px(24.)).text_center().text_color(look.ink_3).child("Nothing matches every word.").into_any_element());
        }
        let key = |shown: &'static str, does: &'static str| div().flex().gap(px(4.)).child(div().font_weight(FontWeight::MEDIUM).child(shown)).child(does);
        let card = div()
            .id("palette")
            .occlude()
            .capture_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| view.palette_key(event, window, cx)))
            .w(px(WIDE))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .overflow_hidden()
            .bg(look.float)
            .shadow(vec![crate::parts::halo(look.line, 1.)])
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(48.))
                    .px(px(14.))
                    .border_b_1()
                    .border_color(look.line)
                    .child(div().flex_1().child(Input::new(&palette.input).appearance(false).text_size(px(15.))))
                    .children(count),
            )
            .child(div().id("palette-list").flex().flex_col().max_h(px(LIST_MOST)).overflow_y_scroll().px(px(6.)).pt(px(4.)).pb(px(8.)).children(listed))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(12.))
                    .px(px(14.))
                    .pt(px(8.))
                    .pb(px(10.))
                    .border_t_1()
                    .border_color(look.line)
                    .text_size(px(12.))
                    .line_height(px(17.))
                    .text_color(look.ink_3)
                    .child(key("↑↓", "move"))
                    .child(key("⏎", "run"))
                    .child(key("esc", "close"))
                    .child(div().flex_1())
                    .child("Typing / in a box lists these too"),
            );
        let close = cx.listener(|view, _: &ClickEvent, window, cx| view.close_palette(window, cx));
        Some(div().id("palette-scrim").absolute().inset_0().flex().items_start().justify_center().pt(px(DOWN)).bg(look.scrim).occlude().on_click(close).child(card))
    }
}

#[cfg(test)]
impl Panes {
    pub(crate) fn palette_open_for_test(&self) -> bool {
        self.palette.is_some()
    }

    pub(crate) fn palette_names_for_test(&self, cx: &App) -> Vec<String> {
        let Some(palette) = &self.palette else { return Vec::new() };
        let rows = self.palette_rows(cx);
        Self::palette_fits(palette, &rows, cx).into_iter().map(|at| rows[at].name.clone()).collect()
    }

    pub(crate) fn type_in_palette_for_test(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = &self.palette {
            palette.input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        }
    }

    pub(crate) fn take_palette_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.take_palette(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, command_rows};

    fn command(name: &str, skill: bool) -> Command {
        Command { name: name.to_owned(), about: format!("what {name} does"), hint: None, skill }
    }

    #[test]
    fn a_command_goes_by_the_name_people_look_for() {
        let rows = command_rows(&[command("logout", false), command("model", false)]);
        assert_eq!(rows[0].name, "Change the model", "in the order they're laid out, not the order rein said");
        assert_eq!(rows[0].section, "This conversation");
        assert_eq!(rows[1].section, "Account");
    }

    #[test]
    fn what_isnt_named_still_shows_under_rein_s_own_words() {
        let rows = command_rows(&[command("brand-new", false), command("a-skill", true)]);
        assert_eq!((rows[0].section, rows[0].name.as_str()), ("More", "what brand-new does"));
        assert_eq!(rows[1].section, "Skills");
    }

    #[test]
    fn what_rein_doesnt_offer_isnt_listed() {
        assert!(command_rows(&[]).is_empty());
    }
}
