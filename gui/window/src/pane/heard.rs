//! What a pane does with what its agent says: turns ending, answers coming,
//! and everything that comes of them.

use std::time::Instant;

use gpui_kit::*;
use rein_transcript::composing::waiting_shown;
use rein_transcript::{Choice, Standing, Tick, Update, says_back};
use serde_json::Value;

use super::{Pane, Sent, Told, Waiting};
use crate::agent::{CameTo, Said};

impl Pane {
    /// A turn has ended, as it should.
    fn turn_ended(&mut self) -> Update {
        self.running = false;
        // What it was asking went with the turn.
        self.asked.clear();
        self.drop_asking();
        self.writing.turn_over();
        self.ask_sent();
        self.ask_room();
        self.watch.turn_over(self.looking);
        self.transcript.turn_over()
    }

    /// Asks what you sent from here: rein keeps from it what may hold a key.
    fn ask_sent(&mut self) {
        self.next_request += 1;
        if self.agent.as_mut().is_some_and(|agent| agent.ask_sent(self.next_request)) {
            self.sent.insert(self.next_request, Sent::Recall);
        }
    }

    /// Asks what's taking room, for a panel that's showing it.
    pub(super) fn ask_room(&mut self) {
        if self.panel.is_some() {
            self.next_request += 1;
            if self.agent.as_mut().is_some_and(|agent| agent.ask_room(self.next_request)) {
                self.sent.insert(self.next_request, Sent::Room);
            }
        }
    }

    /// The turn's over, or a setting's done: what was waiting for that
    /// goes, one at a time.
    fn after_turn(&mut self, cx: &mut Context<Self>) {
        if !self.running
            && !self.setting_changing()
            && let Some((text, images)) = self.writing.next_later()
        {
            self.run(&text, images, cx);
        }
    }

    /// A turn has failed, and this is why.
    fn turn_failed(&mut self, why: String, cx: &mut Context<Self>) -> Update {
        self.running = false;
        self.asked.clear();
        self.drop_asking();
        self.writing.turn_over();
        // It may not go as you meant it to, now: it's yours again.
        self.writing.bring_back();
        self.watch.failed();
        self.tell_parent(Err(&why), cx);
        let update = self.transcript.turn_over().and(self.transcript.note(&why));
        self.failure = Some(why);
        update
    }

    /// What came of the request sent under `id`.
    fn came_to(&mut self, id: u64, came_to: CameTo, cx: &mut Context<Self>) -> Update {
        match (self.sent.remove(&id), came_to) {
            // A turn nobody here prompted got there first. A command's line
            // may hold a key, and only rein can say how much of it to show.
            (Some(Sent::Prompt(text, _)), CameTo::Busy(why)) if says_back(&text) => self.transcript.note(&format!("{} didn't run: {why}", waiting_shown(&text))),
            // Images can't go in a turn that's running, and steering without
            // them would leave the words pointing at nothing.
            (Some(Sent::Prompt(text, images)), CameTo::Busy(why)) if !images.is_empty() => {
                self.writing.waits(text, images);
                self.writing.bring_back();
                cx.notify();
                self.transcript.note(&format!("That's back in the box: {why}"))
            }
            // What you wrote goes into it, as it would have if you'd seen it
            // start. It shows already, so its coming back adds nothing.
            (Some(Sent::Prompt(text, _)), CameTo::Busy(_)) => {
                self.next_request += 1;
                if let Some(agent) = &mut self.agent {
                    agent.steer(self.next_request, &text);
                    self.writing.shown_already(&text);
                    self.sent.insert(self.next_request, Sent::Steer(text));
                }
                Update::default()
            }
            (Some(Sent::Prompt(..)), CameTo::Refused(why)) => self.turn_failed(why, cx),
            (Some(Sent::Prompt(..)), CameTo::TurnOver) => self.turn_ended(),
            (Some(Sent::Mode(mode)), CameTo::Done) => {
                if let Some(modes) = &mut self.modes {
                    modes.current = mode;
                }
                Update::default()
            }
            (Some(Sent::Mode(_)), CameTo::Busy(why) | CameTo::Refused(why)) => self.transcript.note(&format!("The mode stays as it was: {why}")),
            (Some(Sent::Recall), CameTo::Told(said)) => {
                self.writing.recall.load(said["sent"].as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(Value::as_str).map(str::to_owned).collect());
                Update::default()
            }
            (Some(Sent::Paths), CameTo::Told(said)) => {
                self.paths_came(said["paths"].as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(Value::as_str).map(str::to_owned).collect(), cx);
                Update::default()
            }
            (Some(Sent::Tasks), CameTo::Told(said)) => {
                self.tasks_came(&said, cx);
                Update::default()
            }
            (Some(Sent::Room), CameTo::Told(said)) => {
                self.room_came(&said, cx);
                Update::default()
            }
            (Some(Sent::Stopping | Sent::UndoTurns(_) | Sent::UndoPlan | Sent::Undoing | Sent::Setting | Sent::Models), CameTo::Busy(why) | CameTo::Refused(why)) => {
                self.undoing = None;
                self.transcript.note(&why)
            }
            (Some(Sent::UndoTurns(wanted)), CameTo::Told(said)) => self.undo_turns_came(wanted, &said, cx),
            (Some(Sent::UndoPlan), CameTo::Told(said)) => {
                self.undo_plan_came(&said, cx);
                Update::default()
            }
            (Some(Sent::Undoing), _) => {
                self.undoing = None;
                self.ask_room();
                cx.notify();
                Update::default()
            }
            // A command takes no steering, and turns have a moment that
            // doesn't either: it goes after, rather than nowhere.
            (Some(Sent::Steer(text)), CameTo::Busy(_)) => {
                self.writing.reached(&text);
                self.writing.waits(text, Vec::new());
                cx.notify();
                Update::default()
            }
            (Some(Sent::Steer(_)), CameTo::Refused(why)) => self.transcript.note(&format!("That didn't reach it: {why}")),
            _ => Update::default(),
        }
    }

    /// Takes in one thing the agent said.
    fn take_one(&mut self, one: Said, cx: &mut Context<Self>) -> Update {
        match one {
            Said::Update(update) => {
                if let Some((_, probe)) = &self.about.settles_at {
                    probe.first_taken();
                }
                if update["sessionUpdate"] == "user_message_chunk"
                    && let Some(text) = update["content"]["text"].as_str()
                {
                    self.writing.reached(text);
                    if self.writing.was_shown_already(text) {
                        return Update::default();
                    }
                }
                self.transcript.apply(&update)
            }
            Said::Heard { from, text } => self.transcript.heard(&from, &text),
            // Said in order, so the step it's about is in by now.
            Said::Asked { id, call, options, words } => {
                let question = self.transcript.question(&call).cloned();
                let choices = Choice::offered(&options, question.is_some());
                if self.asked.is_empty() {
                    self.asked_at = Instant::now();
                }
                self.asked.push_back(Waiting { id, question, words, choices });
                self.watch.asked();
                Update::default()
            }
            Said::SubAgent { id, asked } => {
                cx.emit(Told::SubAgent { id, asked });
                Update::default()
            }
            // A command waits on your answer. It goes up when it's drawn.
            Said::Asking { id, asking } => {
                self.to_ask = Some((id, asking));
                self.watch.asked();
                cx.notify();
                Update::default()
            }
            Said::Modes(modes) => {
                self.modes = Some(modes);
                Update::default()
            }
            Said::Settings(settings) => {
                self.settings_changed(settings);
                cx.notify();
                Update::default()
            }
            Said::State { branch, meters, background } => {
                self.branch = branch;
                self.meters = meters;
                self.background = background;
                cx.emit(Told::Changed);
                cx.notify();
                Update::default()
            }
            Said::Started(session) => {
                self.about.session = Some(session.clone());
                if let Some(agent) = &mut self.agent {
                    agent.started(session);
                }
                self.ask_sent();
                // What a saved conversation did isn't this turn's.
                self.transcript.turn_started();
                self.tell_parent(Ok(()), cx);
                Update::default()
            }
            Said::TurnStarted => {
                self.running = true;
                self.failure = None;
                self.transcript.turn_started();
                self.watch.started();
                Update::default()
            }
            Said::TurnOver => self.turn_ended(),
            Said::Failed(why) => self.turn_failed(why, cx),
            Said::Answer { id, came_to } => self.came_to(id, came_to, cx),
            // Nothing's left to answer what's asked, or to take what you
            // write: kept, it would read as working for good.
            Said::Ended => {
                self.agent = None;
                self.running = false;
                self.asked.clear();
                self.drop_asking();
                self.writing.turn_over();
                // Nothing's left to send it to: it's yours again.
                self.writing.bring_back();
                self.watch.gone();
                self.tell_parent(Err("the sub-agent ended before it started"), cx);
                self.transcript.turn_over().and(self.transcript.note("This agent has ended."))
            }
        }
    }

    /// Takes in what the agent said since the last time.
    pub(crate) fn take(&mut self, said: Vec<Said>, cx: &mut Context<Self>) {
        let (mut added, mut first, mut end) = (0, usize::MAX, 0);
        let before = self.listed();
        for one in said {
            let update = self.take_one(one, cx);
            added += update.added;
            if !update.touched.is_empty() {
                first = first.min(update.touched.start);
                end = end.max(update.touched.end);
            }
        }
        self.show(added, first..end, cx);
        self.after_turn(cx);
        if let Some((count, probe)) = &self.about.settles_at
            && self.transcript.len() >= *count
        {
            probe.settle_on_next_frame();
            self.about.settles_at = None;
        }
        // The list only hears of what it shows. The window draws again with
        // any pane, but it's told to look again only when this has changed.
        let after = self.listed();
        if after.0 != before.0 {
            self.stands_anew(cx);
        } else if after != before {
            cx.emit(Told::Changed);
        }
    }

    /// What the list of agents shows of this one.
    fn listed(&self) -> (Standing, Vec<Tick>, String) {
        (self.standing(), self.ticks(), self.title())
    }
}
