//! The page's words and the parts that hold no state: sentences that wrap
//! round their values, lists, the keys, and how values are said.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::config::{Config, Key};

use crate::look::Look;
use crate::parts::{hairline, lip};

/// A sentence that flows round its values, at the page's size.
pub(super) fn sentence(look: &Look) -> Div {
    div().flex().flex_wrap().items_center().gap_x(px(4.)).text_size(px(15.)).line_height(px(27.)).text_color(look.ink)
}

/// Each word of `text` on its own, so a sentence wraps between them. A
/// stop that starts it goes with what came before.
pub(super) fn words(text: &str) -> Vec<Div> {
    text.split(' ')
        .filter(|word| !word.is_empty())
        .enumerate()
        .map(|(index, word)| {
            let leans_back = index == 0 && word.starts_with([',', '.', ':', ';']);
            div().when(leans_back, |word| word.ml(px(-4.))).child(word.to_owned())
        })
        .collect()
}

pub(super) fn mono(text: &str, look: &Look) -> Div {
    div().px(px(4.)).rounded(px(4.)).bg(look.raised).text_size(px(13.5)).child(text.to_owned())
}

pub(super) fn lines(look: &Look) -> Div {
    div().flex().flex_col().border_b_1().border_color(look.line)
}

pub(super) fn line(look: &Look) -> Div {
    div().flex().items_center().gap(px(12.)).min_h(px(36.)).border_t_1().border_color(look.line)
}

pub(super) fn hooks(config: &Config, look: &Look) -> Div {
    let said = sentence(look).children(words("Yours run outside the sandbox. A project's waits for your yes, and asks again when it changes."));
    let rows = config.hooks.iter().map(|hook| {
        let (state, tone) = match hook.state.as_str() {
            "on" => ("on".to_owned(), look.ink_3),
            "waiting" => ("this project, waiting for you".to_owned(), look.amber_text),
            other => (other.to_owned(), look.ink_3),
        };
        let whose = if hook.project { "this project" } else { "yours" };
        line(look)
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.5)).child(hook.command.clone()))
            .child(div().flex_none().text_size(px(12.)).text_color(look.ink_3).child(format!("{} · {whose}", hook.event)))
            .child(div().flex_none().text_size(px(12.)).text_color(tone).child(state))
    });
    let rows: Vec<Div> = rows.collect();
    let list = (!rows.is_empty()).then(|| lines(look).children(rows));
    let none = config
        .hooks
        .is_empty()
        .then(|| div().text_size(px(13.)).text_color(look.ink_3).child("None here: yours go in ~/.claude/settings.json or rein's hooks file, a project's in its .claude folder."));
    div().flex().flex_col().gap(px(12.)).child(said).children(list).children(none)
}

pub(super) fn servers(config: &Config, look: &Look) -> Div {
    let said = sentence(look).children(words("They run outside the sandbox unless one has walls of its own."));
    let rows = config.servers.iter().map(|server| {
        let (state, tone) = match server.state.as_str() {
            "ready" => (count(usize::try_from(server.tools).unwrap_or(usize::MAX), "tool", "tools"), look.ink_3),
            "starting" => ("starting".to_owned(), look.ink_3),
            "stopped" => ("stopped: it starts again when it's next called".to_owned(), look.ink_3),
            why => (why.to_owned(), look.amber_text),
        };
        line(look)
            .child(div().flex_none().text_size(px(12.5)).child(server.name.clone()))
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(tone).child(state))
    });
    let rows: Vec<Div> = rows.collect();
    let list = (!rows.is_empty()).then(|| lines(look).children(rows));
    let none = config.servers.is_empty().then(|| div().text_size(px(13.)).text_color(look.ink_3).child("None yet: /mcp add connects one."));
    div().flex().flex_col().gap(px(12.)).child(said).children(list).children(none)
}

pub(super) fn keys(look: &Look) -> Div {
    let all = [
        ("⌘K", "Go to, or run"),
        ("⌘N", "New agent"),
        ("⌘,", "Settings"),
        ("⇧⇥", "Next mode"),
        ("⏎", "Send, or steer"),
        ("⇥", "Send after this turn"),
        ("esc", "Stop the turn"),
        ("y", "Allow"),
        ("a", "Allow, and don't ask"),
        ("n", "Reject"),
    ];
    let cells = all.iter().map(|(key, what)| {
        let cap = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .min_w(px(28.))
            .h(px(28.))
            .px(px(7.))
            .rounded(px(6.))
            .bg(look.bg)
            .shadow(vec![hairline(look.ring), lip(look.lip)])
            .text_size(px(13.))
            .text_color(look.ink_2)
            .child(*key);
        div().w(relative(1. / 3.)).flex().items_center().gap(px(10.)).pb(px(12.)).child(cap).child(div().min_w_0().truncate().text_size(px(13.)).child(*what))
    });
    div().flex().flex_wrap().children(cells.collect::<Vec<_>>())
}

/// A choice, as it's pressed: the modes that hold nothing back carry ⚠.
pub(super) fn shown_choice(key: Key, choice: &str) -> String {
    match (key, choice) {
        (Key::Look, "follow") => "Follow the Mac".to_owned(),
        (Key::Mode, "trust") => "⚠ trust".to_owned(),
        _ => choice.to_owned(),
    }
}

/// What taking `key` out of the file leaves it at.
pub(super) const fn what_unset_means(key: Key) -> &'static str {
    match key {
        Key::Model => "the first model you're signed in to",
        Key::Effort => "each model's own default",
        Key::Search => "auto",
        Key::Mode => "trust, as rein starts",
        Key::Sandbox => "off, as rein starts",
        Key::CompactAt => "150,000, or less for a small window",
        Key::MaxSteps => "200",
        Key::Check => "the check rein finds in a project",
        Key::Peers => "accept",
        Key::Bell | Key::Memory => "on",
        Key::Look => "following the Mac",
    }
}

pub(super) fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub(super) fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect())
}

/// `150000` as `150,000`; what isn't a number, as it is.
pub(super) fn thousands(text: &str) -> String {
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
        return text.to_owned();
    }
    let digits: Vec<char> = text.chars().collect();
    let mut shown = String::new();
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            shown.push(',');
        }
        shown.push(*digit);
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::thousands;

    #[test]
    fn a_number_of_tokens_is_said_in_thousands() {
        assert_eq!([thousands("150000"), thousands("900"), thousands("1000"), thousands("off")], ["150,000", "900", "1,000", "off"]);
    }
}
