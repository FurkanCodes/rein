//! What a test of a pane reaches for that nothing else needs.

use std::time::Instant;

use gpui_kit::*;
use serde_json::Value;

use super::{Pane, foot};

impl Pane {
    pub(crate) const fn is_running(&self) -> bool {
        self.running
    }

    pub(crate) fn mode_for_test(&self) -> Option<String> {
        self.modes.as_ref().map(|modes| modes.current.clone())
    }

    /// Asks for `mode`, as the bar's menu does, and says under what number.
    pub(crate) fn ask_mode_for_test(&mut self, mode: &str) -> u64 {
        self.set_mode(mode);
        self.next_request
    }

    pub(crate) const fn last_request_for_test(&self) -> u64 {
        self.next_request
    }

    /// As if the question at the front had been up for long enough.
    pub(crate) fn pretend_waited_for_test(&mut self) {
        self.asked_at = Instant::now().checked_sub(foot::KEYS_AFTER).expect("the clock has run this long");
    }

    pub(crate) fn press_for_test(&mut self, drawn_for: &Value, chosen: &str, cx: &mut Context<Self>) {
        self.answer(drawn_for, chosen, cx);
    }

    pub(crate) fn question_count_for_test(&self) -> usize {
        self.asked.len()
    }

    /// Ends its agent, so a test can read all it was sent.
    pub(crate) fn finish_agent_for_test(&mut self) {
        if let Some(agent) = self.agent.take() {
            agent.finish();
        }
    }

    pub(crate) fn send_box_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send_box(window, cx);
    }

    pub(crate) const fn is_beating_for_test(&self) -> bool {
        self.beating.is_some()
    }

    pub(crate) fn run_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.run(text, Vec::new(), cx);
    }

    pub(crate) fn run_with_image_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        let image = crate::agent::Attached { label: "[Image #1]".to_owned(), mime: "image/png", data: "AAAA".to_owned() };
        self.run(text, vec![image], cx);
    }

    pub(crate) const fn all_open_for_test(&self) -> bool {
        self.all_open
    }

    pub(crate) const fn background_for_test(&self) -> u64 {
        self.background
    }

    /// All that's been said, as one text.
    pub(crate) fn conversation_for_test(&self) -> String {
        (0..self.transcript.len()).filter_map(|index| self.transcript.get(index)).map(|message| message.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    pub(crate) fn rows_for_test(&self) -> usize {
        self.transcript.rows()
    }
}
