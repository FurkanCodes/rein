//! What the window's panels and sheets show, as `rein serve` says it, read
//! here once: what's taking room, the commands in the background, the plan,
//! and the turns to undo.

use serde_json::Value;

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

fn texts(value: &Value) -> Vec<String> {
    value.as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// `tokens` as the bar says them: `14k`, or `900` under a thousand.
pub fn tokens(tokens: u64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    let tenths = (tokens + 50) / 100;
    if tenths < 100 && !tenths.is_multiple_of(10) { format!("{}.{}k", tenths / 10, tenths % 10) } else { format!("{}k", (tokens + 500) / 1000) }
}

/// One kind of thing in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub about: String,
    pub tokens: u64,
}

/// One big output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Big {
    pub what: String,
    pub tokens: u64,
    /// Out of date, so it's the first to go.
    pub stale: bool,
}

/// What's taking room in a conversation, as far as anyone can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub used: u64,
    pub limit: u64,
    pub parts: Vec<Part>,
    pub biggest: Vec<Big>,
    /// How much of the last request came from the cache, if one says.
    pub cache: Option<u64>,
    /// What retiring what's out of date would free.
    pub retiring: u64,
}

impl Room {
    pub fn of(said: &Value) -> Option<Room> {
        let parts = said["parts"].as_array()?.iter().map(|part| Part { name: text(&part["name"]), about: text(&part["about"]), tokens: part["tokens"].as_u64().unwrap_or(0) });
        let biggest = said["biggest"].as_array()?.iter().map(|big| Big { what: text(&big["what"]), tokens: big["tokens"].as_u64().unwrap_or(0), stale: big["stale"] == true });
        Some(Room {
            used: said["used"].as_u64()?,
            limit: said["limit"].as_u64()?,
            parts: parts.filter(|part| part.tokens > 0).collect(),
            biggest: biggest.collect(),
            cache: said["cache"].as_u64(),
            retiring: said["retiring"].as_u64().unwrap_or(0),
        })
    }

    /// How much of the room is used, up to all of it.
    pub fn share(&self) -> f32 {
        if self.limit == 0 { 0. } else { (self.used as f32 / self.limit as f32).min(1.) }
    }

    /// `part`'s share of the room, by how the bar is drawn.
    pub fn share_of(&self, part: &Part) -> f32 {
        if self.limit == 0 { 0. } else { (part.tokens as f32 / self.limit as f32).min(1.) }
    }
}

/// A command in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub command: String,
    pub seconds: u64,
    /// How it ended, if it has.
    pub ended: Option<String>,
    pub failed: bool,
    /// The last thing it printed.
    pub last: Option<String>,
}

impl Task {
    pub fn listed(said: &Value) -> Vec<Task> {
        let listed = said["tasks"].as_array().map(Vec::as_slice).unwrap_or_default().iter();
        listed
            .filter_map(|task| {
                Some(Task {
                    id: task["id"].as_str()?.to_owned(),
                    command: text(&task["command"]),
                    seconds: task["seconds"].as_u64().unwrap_or(0),
                    ended: task["ended"].as_str().map(str::to_owned),
                    failed: task["failed"] == true,
                    last: task["last"].as_str().map(str::to_owned),
                })
            })
            .collect()
    }

    /// How long it's been running, or ran.
    pub fn took(&self) -> String {
        let seconds = self.seconds;
        if seconds < 60 { format!("{seconds}s") } else { format!("{}m", seconds / 60) }
    }
}

/// How far a step of the plan has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanState {
    Waiting,
    Running,
    Done,
}

/// A step of the model's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub text: String,
    pub state: PlanState,
}

impl Planned {
    /// Read from ACP's `plan` update.
    pub fn listed(update: &Value) -> Vec<Planned> {
        let entries = update["entries"].as_array().map(Vec::as_slice).unwrap_or_default().iter();
        entries
            .map(|entry| {
                let state = match entry["status"].as_str() {
                    Some("completed") => PlanState::Done,
                    Some("in_progress") => PlanState::Running,
                    _ => PlanState::Waiting,
                };
                Planned { text: text(&entry["content"]), state }
            })
            .collect()
    }
}

/// A turn there is to undo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoTurn {
    pub turn: usize,
    pub said: String,
    /// When it started, in seconds since 1970.
    pub at: u64,
    pub changed: usize,
    pub made: usize,
    pub removed: usize,
}

impl UndoTurn {
    /// Latest first, as rein says them.
    pub fn listed(said: &Value) -> Vec<UndoTurn> {
        let listed = said["turns"].as_array().map(Vec::as_slice).unwrap_or_default().iter();
        let count = |value: &Value| value.as_u64().and_then(|count| usize::try_from(count).ok()).unwrap_or(0);
        listed
            .filter_map(|turn| {
                Some(UndoTurn {
                    turn: usize::try_from(turn["turn"].as_u64()?).ok()?,
                    said: text(&turn["said"]),
                    at: turn["at"].as_u64().unwrap_or(0),
                    changed: count(&turn["changed"]),
                    made: count(&turn["made"]),
                    removed: count(&turn["removed"]),
                })
            })
            .collect()
    }

    /// What it did, in a few words: `2 changed, 1 made`.
    pub fn counts(&self) -> String {
        let counts = [(self.changed, "changed"), (self.made, "made"), (self.removed, "removed")];
        let shown: Vec<String> = counts.iter().filter(|(count, _)| *count > 0).map(|(count, what)| format!("{count} {what}")).collect();
        shown.join(", ")
    }
}

/// What undoing back to before a turn does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPlan {
    pub put_back: Vec<String>,
    pub take_away: Vec<String>,
    pub left: Vec<String>,
    pub lost: Vec<String>,
    /// How many turns it undoes.
    pub turns: usize,
}

impl UndoPlan {
    pub fn of(said: &Value) -> Option<UndoPlan> {
        let plan = said.get("plan")?;
        Some(UndoPlan {
            put_back: texts(&plan["putBack"]),
            take_away: texts(&plan["takeAway"]),
            left: texts(&plan["left"]),
            lost: texts(&plan["lost"]),
            turns: plan["turns"].as_u64().and_then(|turns| usize::try_from(turns).ok()).unwrap_or(1),
        })
    }

    /// It would change something.
    pub fn does_anything(&self) -> bool {
        !self.put_back.is_empty() || !self.take_away.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn tokens_read_as_the_bar_says_them() {
        assert_eq!(tokens(900), "900");
        assert_eq!(tokens(1_400), "1.4k");
        assert_eq!(tokens(24_100), "24k");
        assert_eq!(tokens(150_000), "150k");
    }

    #[test]
    fn a_part_with_nothing_in_it_isnt_listed() {
        let room = Room::of(&json!({ "used": 42_000, "limit": 150_000, "parts": [{ "name": "Tools", "about": "17", "tokens": 6200 }, { "name": "Your messages", "about": "", "tokens": 0 }], "biggest": [{ "what": "a.rs", "tokens": 6100, "stale": true }], "cache": 97, "retiring": 16_000 })).unwrap();
        assert_eq!(room.parts.len(), 1);
        assert!(room.biggest[0].stale);
        assert_eq!(room.cache, Some(97));
        assert!((room.share() - 0.28).abs() < 0.001);
    }

    #[test]
    fn a_room_nobody_measured_is_no_room() {
        assert_eq!(Room::of(&json!({})), None);
        assert_eq!(Room::of(&json!({ "used": 1, "limit": 0, "parts": [], "biggest": [] })).unwrap().share(), 0.);
    }

    #[test]
    fn a_task_that_ended_says_how() {
        let tasks = Task::listed(
            &json!({ "tasks": [{ "id": "b1", "command": "pnpm dev", "seconds": 725, "ended": null, "last": "ready" }, { "id": "b2", "command": "x", "seconds": 3, "ended": "exit 1", "failed": true }, { "command": "no id" }] }),
        );
        assert_eq!(tasks.len(), 2);
        assert_eq!((tasks[0].took(), tasks[0].ended.as_deref()), ("12m".to_owned(), None));
        assert_eq!((tasks[1].took(), tasks[1].ended.as_deref(), tasks[1].failed), ("3s".to_owned(), Some("exit 1"), true));
    }

    #[test]
    fn a_plan_says_how_far_each_step_has_got() {
        let plan = Planned::listed(
            &json!({ "entries": [{ "content": "a", "status": "completed" }, { "content": "b", "status": "in_progress" }, { "content": "c", "status": "pending" }, { "content": "d" }] }),
        );
        let states: Vec<PlanState> = plan.iter().map(|step| step.state).collect();
        assert_eq!(states, [PlanState::Done, PlanState::Running, PlanState::Waiting, PlanState::Waiting]);
    }

    #[test]
    fn a_turn_says_only_what_it_did() {
        let turns = UndoTurn::listed(&json!({ "turns": [{ "turn": 3, "said": "x", "at": 9, "changed": 2, "made": 0, "removed": 1 }, { "said": "no number" }] }));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].counts(), "2 changed, 1 removed");
    }

    #[test]
    fn a_plan_that_puts_nothing_back_does_nothing() {
        let plan = UndoPlan::of(&json!({ "plan": { "putBack": [], "takeAway": [], "left": ["a"], "lost": [], "turns": 1 } })).unwrap();
        assert!(!plan.does_anything());
        assert!(UndoPlan::of(&json!({ "plan": { "takeAway": ["a"] } })).unwrap().does_anything());
        assert_eq!(UndoPlan::of(&json!({})), None);
    }
}
