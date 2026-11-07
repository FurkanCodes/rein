//! Your settings, as `rein serve` reads them out for a settings page: what
//! new conversations start with, what the file says, and what the
//! conversation found (rules, skills, hooks, memory, MCP servers). Read here
//! once, into types.

use serde_json::Value;

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

fn texts(value: &Value) -> Vec<String> {
    value.as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(Value::as_str).map(str::to_owned).collect()
}

fn each(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

/// A setting the page can change: one of your settings file's keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Model,
    Effort,
    Search,
    Mode,
    Sandbox,
    CompactAt,
    MaxSteps,
    Check,
    Peers,
    Bell,
    Memory,
    Look,
}

impl Key {
    pub const ALL: [Key; 12] =
        [Key::Model, Key::Effort, Key::Search, Key::Mode, Key::Sandbox, Key::CompactAt, Key::MaxSteps, Key::Check, Key::Peers, Key::Bell, Key::Memory, Key::Look];

    /// As the file names it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Key::Model => "model",
            Key::Effort => "effort",
            Key::Search => "search",
            Key::Mode => "mode",
            Key::Sandbox => "sandbox",
            Key::CompactAt => "compact_at",
            Key::MaxSteps => "max_steps",
            Key::Check => "check",
            Key::Peers => "peers",
            Key::Bell => "bell",
            Key::Memory => "memory",
            Key::Look => "look",
        }
    }

    /// It takes what you type, not one of a few choices.
    pub const fn typed(self) -> bool {
        matches!(self, Key::CompactAt | Key::MaxSteps | Key::Check)
    }
}

/// A rule: what asks, what's refused, what runs without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// As it's written: `ask git push --force`.
    pub rule: String,
    /// `allow`, `ask` or `deny`.
    pub kind: String,
    /// What it names: `git push --force`.
    pub what: String,
    /// Yours, for every project, or else the project's.
    pub yours: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    /// `yours`, `project` or `plugin`.
    pub whose: String,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub command: String,
    /// When it runs, as Claude Code names it: `PreToolUse`.
    pub event: String,
    /// It came with the project.
    pub project: bool,
    /// Where it's set, in a few words.
    pub from: String,
    /// `on`, `off`, `waiting`, or off and why.
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub name: String,
    pub description: String,
    /// rein's own, or Claude Code's, which rein only reads.
    pub reins: bool,
    /// The day it was last saved.
    pub modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    /// `ready`, `starting`, `stopped`, or why it didn't start.
    pub state: String,
    pub tools: u64,
}

/// A mode, and what it does, in a few words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeChoice {
    pub id: String,
    pub about: String,
}

/// All a settings page shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Where the file is: `~/.config/rein/config`.
    pub file: String,
    /// What new conversations start with, by key: `None` lets rein choose.
    starts: Vec<(Key, Option<String>)>,
    /// What the file itself says, by key, and the environment variable
    /// that wins over it, if one is set.
    in_file: Vec<(Key, Option<String>, Option<String>)>,
    pub efforts: Vec<String>,
    pub modes: Vec<ModeChoice>,
    pub searches: Vec<String>,
    pub peers: Vec<String>,
    pub looks: Vec<String>,
    pub rules: Vec<Rule>,
    pub skills: Vec<Skill>,
    pub hooks: Vec<Hook>,
    pub notes: Vec<Note>,
    pub servers: Vec<Server>,
}

impl Config {
    /// Read from what `_io.github.furkancodes/config` answered; `None` if it
    /// isn't a page at all.
    pub fn of(said: &Value) -> Option<Config> {
        let starts = said["starts"].as_object()?;
        let keyed = |key: &str| Key::ALL.into_iter().find(|one| one.as_str() == key);
        let starts = Key::ALL.into_iter().map(|key| (key, starts.get(key.as_str()).and_then(Value::as_str).map(str::to_owned))).collect();
        let in_file = each(&said["keys"])
            .iter()
            .filter_map(|one| Some((keyed(one["key"].as_str()?)?, one["file"].as_str().map(str::to_owned), one["over"].as_str().map(str::to_owned))))
            .collect();
        let choices = &said["choices"];
        Some(Config {
            file: text(&said["file"]),
            starts,
            in_file,
            efforts: texts(&choices["effort"]),
            modes: each(&choices["mode"]).iter().map(|one| ModeChoice { id: text(&one["id"]), about: text(&one["about"]) }).collect(),
            searches: texts(&choices["search"]),
            peers: texts(&choices["peers"]),
            looks: texts(&choices["look"]),
            rules: each(&said["rules"])
                .iter()
                .map(|one| Rule { rule: text(&one["rule"]), kind: text(&one["kind"]), what: text(&one["what"]), yours: one["yours"] == true })
                .collect(),
            skills: each(&said["skills"]).iter().map(|one| Skill { name: text(&one["name"]), whose: text(&one["whose"]), on: one["on"] != false }).collect(),
            hooks: each(&said["hooks"])
                .iter()
                .map(|one| Hook {
                    command: text(&one["command"]),
                    event: text(&one["event"]),
                    project: one["project"] == true,
                    from: text(&one["from"]),
                    state: text(&one["state"]),
                })
                .collect(),
            notes: each(&said["notes"])
                .iter()
                .map(|one| Note {
                    name: text(&one["name"]),
                    description: text(&one["description"]),
                    reins: one["reins"] == true,
                    modified: one["modified"].as_str().map(str::to_owned),
                })
                .collect(),
            servers: each(&said["servers"]).iter().map(|one| Server { name: text(&one["name"]), state: text(&one["state"]), tools: one["tools"].as_u64().unwrap_or(0) }).collect(),
        })
    }

    /// What new conversations start with for `key`; `None` lets rein choose.
    pub fn starts(&self, key: Key) -> Option<&str> {
        self.starts.iter().find(|(one, _)| *one == key).and_then(|(_, value)| value.as_deref())
    }

    /// What the file says of `key`, if it says anything.
    pub fn in_file(&self, key: Key) -> Option<&str> {
        self.in_file.iter().find(|(one, ..)| *one == key).and_then(|(_, value, _)| value.as_deref())
    }

    /// The environment variable that wins over the file for `key`, if one
    /// is set: changing the file then changes nothing until it's unset.
    pub fn over(&self, key: Key) -> Option<&str> {
        self.in_file.iter().find(|(one, ..)| *one == key).and_then(|(.., over)| over.as_deref())
    }

    /// The choices `key` takes, where it takes a few: the models come from
    /// the conversation, which knows which you can use.
    pub fn choices(&self, key: Key) -> Vec<String> {
        let on_off = || vec!["on".to_owned(), "off".to_owned()];
        match key {
            Key::Effort => self.efforts.clone(),
            Key::Mode => self.modes.iter().map(|mode| mode.id.clone()).collect(),
            Key::Search => self.searches.clone(),
            Key::Peers => self.peers.clone(),
            Key::Look => self.looks.clone(),
            Key::Sandbox | Key::Bell | Key::Memory => on_off(),
            Key::Model | Key::CompactAt | Key::MaxSteps | Key::Check => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_page_reads_what_starts_what_the_file_says_and_what_wins_over_it() {
        let said = json!({
            "file": "~/.config/rein/config",
            "keys": [{ "key": "mode", "file": "edits", "over": "REIN_MODE" }, { "key": "colour", "file": "blue" }],
            "starts": { "mode": "ask", "model": null, "look": "follow" },
            "choices": { "effort": ["low", "high"], "mode": [{ "id": "ask", "about": "asks first" }], "search": ["auto", "off"], "peers": ["accept"], "look": ["light", "dark", "follow"] },
            "rules": [{ "rule": "ask rm", "kind": "ask", "what": "rm", "yours": true }],
            "skills": [{ "name": "tdd", "whose": "yours", "on": false }],
            "hooks": [], "notes": [], "servers": [{ "name": "linear", "state": "ready", "tools": 14 }],
        });
        let config = Config::of(&said).unwrap();
        assert_eq!((config.starts(Key::Mode), config.in_file(Key::Mode), config.over(Key::Mode)), (Some("ask"), Some("edits"), Some("REIN_MODE")));
        assert_eq!(config.starts(Key::Model), None, "rein chooses");
        assert_eq!(config.choices(Key::Mode), ["ask"]);
        assert_eq!(config.choices(Key::Bell), ["on", "off"]);
        assert!(config.choices(Key::CompactAt).is_empty() && Key::CompactAt.typed());
        assert_eq!(config.rules[0].what, "rm");
        assert!(!config.skills[0].on);
        assert_eq!(config.servers[0].tools, 14);
        assert_eq!(Config::of(&json!({ "error": "no" })), None);
    }
}
