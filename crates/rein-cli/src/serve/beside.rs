//! What a client can ask for while a turn runs, and between turns: none of
//! it needs what the turn holds, so it's answered by itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rein_agent::agent::{Agent, Heard, Llm};
use rein_agent::background::Jobs;
use serde_json::{Value, json};

use super::Link;
use super::wire::{self, Refused, Request};
use crate::ui::editor::Entry;
use crate::ui::history::History;

/// What a client can have while a turn runs, which needs none of what the
/// turn holds.
pub(super) struct Beside {
    pub(super) session: String,
    pub(super) root: PathBuf,
    pub(super) sandbox: rein_agent::sandbox::Shared,
    pub(super) jobs: Arc<Jobs>,
    /// What you sent from this folder, shared with the terminal's ↑.
    pub(super) sent: History,
    /// What's taking room, as it stood when the agent was last free.
    pub(super) room: Value,
}

impl Link {
    /// Keeps `text`, as sent from this folder: a line that may hold a key
    /// isn't kept, as a terminal's isn't.
    pub(super) fn remember(&self, text: &str) {
        if let Some(beside) = self.beside.borrow_mut().as_mut().filter(|_| !crate::commands::hides(text)) {
            beside.sent.add(Entry { text: text.trim().to_owned(), pastes: Vec::new() });
        }
    }

    /// `text`, which you said to the turn that's running.
    pub(super) fn steer(&self, text: String) {
        self.remember(&text);
        self.heard.borrow_mut().push(Heard::You(text));
    }

    /// Answers `request`, one of those answered beside a turn. A project's
    /// files take a while to walk, and rein has one thread.
    pub(super) fn answer_beside(&self, id: &Value, request: &Request) {
        let (Request::Sent { session }
        | Request::Paths { session }
        | Request::Background { session }
        | Request::Tasks { session }
        | Request::StopTask { session, .. }
        | Request::Room { session }) = request
        else {
            return;
        };
        let beside = self.beside.borrow();
        let Some(beside) = beside.as_ref().filter(|beside| beside.session == *session) else {
            self.send(wire::refuse(id, &Refused::new(format!("there's no conversation {session} here"))));
            return;
        };
        match request {
            Request::Sent { .. } => {
                let sent: Vec<String> = (0..beside.sent.len()).map(|i| beside.sent.get(i).full()).collect();
                self.send(wire::answer(id, &json!({ "sent": sent })));
            }
            Request::Paths { .. } => {
                let (root, sandbox, out, id) = (beside.root.clone(), beside.sandbox.clone(), self.out.clone(), id.clone());
                tokio::task::spawn_blocking(move || {
                    let paths = rein_agent::search::paths(&root, &|path: &Path| sandbox.places().hides(path));
                    let _ = out.send(wire::answer(&id, &json!({ "paths": paths })));
                });
            }
            Request::Tasks { .. } => self.send(wire::answer(id, &json!({ "tasks": beside.jobs.list().iter().map(task).collect::<Vec<Value>>() }))),
            Request::Room { .. } => self.send(wire::answer(id, &beside.room)),
            Request::StopTask { id: task, .. } => {
                let (jobs, task, out, id) = (beside.jobs.clone(), task.clone(), self.out.clone(), id.clone());
                // It's given two seconds to stop, and rein has one thread.
                tokio::spawn(async move {
                    let said = match jobs.stop_yours(&task).await {
                        Ok(()) => wire::answer(&id, &json!({})),
                        Err(why) => wire::refuse(&id, &Refused::new(why)),
                    };
                    let _ = out.send(said);
                });
            }
            _ => {
                beside.jobs.detach();
                self.send(wire::answer(id, &json!({})));
            }
        }
    }

    /// How many commands are running in the background.
    pub(super) fn in_the_background(&self) -> usize {
        self.beside.borrow().as_ref().map_or(0, |beside| beside.jobs.list().iter().filter(|job| job.ended.is_none()).count())
    }

    /// What's taking room in `agent` now.
    pub(super) fn room_changed(&self, agent: &Agent<Llm>, terminal: &crate::terminal::Terminal) {
        if let Some(beside) = self.beside.borrow_mut().as_mut() {
            beside.room = room(agent, terminal);
        }
    }
}

/// A command in the background, as a client shows it.
fn task(job: &rein_agent::background::Listed) -> Value {
    let ended = job.ended.as_ref();
    let first = rein_agent::background::plain(job.command.lines().next().unwrap_or_default());
    json!({ "id": job.id, "command": first, "seconds": job.took.as_secs(), "ended": ended.map(|exit| exit.said()), "failed": ended.is_some_and(|exit| exit.failed()), "last": job.last })
}

/// What's taking room in the conversation, in rein's words for it.
pub(super) fn room(agent: &Agent<Llm>, terminal: &crate::terminal::Terminal) -> Value {
    let room = agent.room();
    let tools = if room.tools_found > 0 { format!("{}, and {} found", room.tools_shown, room.tools_found) } else { room.tools_shown.to_string() };
    let parts = [
        ("Tool output", "reads, commands".to_owned(), room.tool_output),
        ("Tools", tools, room.tools),
        ("Project instructions", "what the project says".to_owned(), room.project),
        ("rein's words", String::new(), room.reins_words),
        ("Skills and memory", "rein's instructions".to_owned(), room.instructions),
        ("Your messages", String::new(), room.your_messages),
    ];
    let parts: Vec<Value> = parts.iter().map(|(name, about, tokens)| json!({ "name": name, "about": about, "tokens": tokens })).collect();
    let biggest: Vec<Value> = room.biggest.iter().map(|(what, tokens, stale)| json!({ "what": what, "tokens": tokens, "stale": stale })).collect();
    json!({ "used": agent.estimated(), "limit": terminal.context_limit, "parts": parts, "biggest": biggest, "cache": terminal.last_cache, "retiring": room.retiring })
}

/// A turn there is to undo, as a client shows it.
pub(super) fn undo_turn(turn: &rein_agent::undo::Listed) -> Value {
    json!({ "turn": turn.turn, "said": turn.said, "at": turn.at, "changed": turn.changed, "made": turn.made, "removed": turn.removed })
}

/// What undoing does, as a client shows it.
pub(super) fn undo_plan(plan: &rein_agent::undo::Plan) -> Value {
    json!({ "putBack": plan.put_back, "takeAway": plan.take_away, "left": plan.left, "lost": plan.lost, "said": plan.said, "turns": plan.turns })
}
