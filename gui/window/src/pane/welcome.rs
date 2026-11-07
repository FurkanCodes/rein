//! A pane before anything's been said in it, on a desk with nothing else:
//! what the agent should work on, in a box with the folder, model, effort
//! and mode it would start with. Signed in to nothing that runs the model,
//! it's the ways there are to sign in.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rein_transcript::signing::{Offered, Way};

use super::{Pane, Sent, Told};
use crate::look::Look;
use crate::parts::Press;

/// What the box says before anything's in it.
pub(super) const DESCRIBE: &str = "Describe the task, or @ a file";

/// Signed in to nothing: the ways there are, the one under way, and why
/// the last one didn't work.
pub(super) struct Signing {
    ways: Vec<Offered>,
    trying: Option<Way>,
    why: Option<String>,
}

impl Signing {
    pub(super) const fn new(ways: Vec<Offered>) -> Signing {
        Signing { ways, trying: None, why: None }
    }
}

/// What the ways to sign in say under them.
const OFFICIAL: &str = "Official sign-ins and API keys only. A key is kept in a file only you can read, and never shown again.";

fn heading(text: &'static str) -> Div {
    div().text_size(px(24.)).line_height(px(30.)).font_weight(FontWeight::SEMIBOLD).child(text)
}

impl Pane {
    /// Whether it's showing the ways to sign in.
    pub(crate) const fn is_signing(&self) -> bool {
        self.signing.is_some()
    }

    /// The first run's heading and box, or the ways to sign in.
    pub(super) fn welcome(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        if self.signing.is_some() {
            return self.ways_in(cx);
        }
        let foot = self.foot(window, cx);
        let ended = self.agent.is_none().then(|| {
            let look = Look::of(cx);
            let why = self.failure.clone().unwrap_or_else(|| "rein couldn't start here.".to_owned());
            div().text_color(look.red).child(why)
        });
        div().flex().flex_col().gap(px(24.)).w_full().child(heading("What should this agent work on?")).child(foot).children(ended)
    }

    /// Each way to sign in, as a row with its button.
    fn ways_in(&self, cx: &mut Context<Self>) -> Div {
        let look = Look::of(cx);
        let Some(signing) = &self.signing else { return div() };
        let rows = signing.ways.iter().enumerate().map(|(index, offered)| {
            let way = offered.way;
            let trying = signing.trying == Some(way);
            let press = Press::new(("way", index)).label(if trying { "Waiting…" } else { offered.action }).on_click(cx.listener(move |pane, _, _, cx| pane.sign_in(way, cx)));
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(14.))
                .py(px(10.))
                .when(index > 0, |row| row.border_t_1().border_color(look.line))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_weight(FontWeight::MEDIUM).child(offered.name))
                        .child(div().text_size(px(12.)).line_height(px(16.)).text_color(look.ink_3).child(offered.says.clone())),
                )
                .child(press)
        });
        let list = div().flex().flex_col().rounded(px(10.)).bg(look.bg).border_1().border_color(look.line).children(rows);
        let why = signing.why.clone().map(|why| div().text_color(look.red).child(why));
        div()
            .debug_selector(|| "ways-in".to_owned())
            .flex()
            .flex_col()
            .gap(px(16.))
            .w_full()
            .child(heading("Sign in to something that runs the model"))
            .child(list)
            .children(why)
            .child(div().text_size(px(13.)).line_height(px(19.)).text_color(look.ink_3).child(OFFICIAL))
    }

    /// Signs in `way`. What it asks along the way comes up as any command's
    /// questions do.
    pub(super) fn sign_in(&mut self, way: Way, cx: &mut Context<Self>) {
        let Some(signing) = self.signing.as_mut().filter(|signing| signing.trying.is_none()) else { return };
        let Some(agent) = &mut self.agent else { return };
        signing.trying = Some(way);
        signing.why = None;
        self.next_request += 1;
        agent.sign_in(self.next_request, way);
        self.sent.insert(self.next_request, Sent::SignIn);
        cx.notify();
    }

    /// What signing in came to: signed in, the conversation opens.
    pub(super) fn signed_in(&mut self, came_to: Result<bool, String>, cx: &mut Context<Self>) {
        let Some(signing) = &mut self.signing else { return };
        signing.trying = None;
        match came_to {
            Ok(true) => {
                if let Some(agent) = &mut self.agent {
                    agent.open_again();
                }
            }
            Ok(false) => {}
            Err(why) => signing.why = Some(why),
        }
        cx.notify();
    }

    /// The folder cell's press: another folder, for the window to start in.
    pub(super) fn choose_folder(&mut self, cx: &mut Context<Self>) {
        cx.emit(Told::Folder);
    }

    /// It's no longer the pane to start with: what you wrote went.
    pub(super) fn begun(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.draft) {
            cx.emit(Told::Begun);
        }
    }
}
