//! What a command asks of you, as `rein serve` says it, and what can be run:
//! a window draws these as the terminal's box would, so a command works the
//! same wherever you are.

use serde_json::Value;

use crate::narrow::Query;

/// One thing in a list to choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub about: String,
}

/// A list to choose one from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub title: String,
    pub items: Vec<Item>,
    /// The one in use.
    pub current: Option<usize>,
    /// Where the list starts, if it's not at the one in use.
    pub at: Option<usize>,
}

impl Menu {
    /// Where the list starts.
    pub fn first(&self) -> usize {
        self.at.or(self.current).filter(|at| *at < self.items.len()).unwrap_or(0)
    }

    /// The items that fit what's typed, best first, or all of them in order
    /// before anything is.
    pub fn matching(&self, typed: &str) -> Vec<usize> {
        matching(&self.items, typed, |item| (&item.name, &item.about))
    }
}

/// What a question offers, under the key that picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub key: String,
    pub says: String,
}

/// What a command is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asking {
    Choose(Menu),
    /// A line to type. One that's `secret` is never shown or kept.
    Read {
        prompt: String,
        secret: bool,
    },
    Yes {
        title: String,
        detail: Option<String>,
        command: Option<String>,
        answers: Vec<Offered>,
    },
}

const CHOOSE: &str = "_io.github.furkancodes/choose";
const READ: &str = "_io.github.furkancodes/read";
const YES: &str = "_io.github.furkancodes/yes";

impl Asking {
    /// Read from a request of `method`: `None` if it's not one of these, or
    /// makes no sense.
    pub fn of(method: &str, params: &Value) -> Option<Asking> {
        let text = |key: &str| params[key].as_str().map(str::to_owned);
        let place = |key: &str| params[key].as_u64().and_then(|at| usize::try_from(at).ok());
        match method {
            CHOOSE => {
                let items = params["items"]
                    .as_array()?
                    .iter()
                    .map(|item| Some(Item { name: item["name"].as_str()?.to_owned(), about: item["about"].as_str().unwrap_or_default().to_owned() }));
                Some(Asking::Choose(Menu { title: text("title")?, items: items.collect::<Option<_>>()?, current: place("current"), at: place("at") }))
            }
            READ => Some(Asking::Read { prompt: text("prompt")?, secret: params["secret"] == true }),
            YES => {
                let answers =
                    params["answers"].as_array()?.iter().map(|answer| Some(Offered { key: answer["key"].as_str()?.to_owned(), says: answer["says"].as_str()?.to_owned() }));
                Some(Asking::Yes { title: text("title")?, detail: text("detail"), command: text("command"), answers: answers.collect::<Option<_>>()? })
            }
            _ => None,
        }
    }
}

/// Something that can be run from the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// With no `/`.
    pub name: String,
    pub about: String,
    /// What it takes, if it takes anything.
    pub hint: Option<String>,
    /// It's a skill, which also loads from a `/` word later in a message.
    pub skill: bool,
}

/// Marks a command as a skill.
const SKILL: &str = "io.github.furkancodes/skill";

impl Command {
    /// Read from an `available_commands_update`.
    pub fn listed(update: &Value) -> Vec<Command> {
        let listed = update["availableCommands"].as_array().map(Vec::as_slice).unwrap_or_default().iter();
        listed
            .filter_map(|one| {
                Some(Command {
                    name: one["name"].as_str()?.to_owned(),
                    about: one["description"].as_str().unwrap_or_default().to_owned(),
                    hint: one["input"]["hint"].as_str().map(str::to_owned),
                    skill: one["_meta"][SKILL] == true,
                })
            })
            .collect()
    }

    /// Those of `commands` that fit what's typed after the `/`.
    pub fn matching(commands: &[Command], typed: &str) -> Vec<usize> {
        matching(commands, typed, |command| (&command.name, &command.about))
    }

    /// The same, of the skills alone: a `/` word later in a message.
    pub fn skills_matching(commands: &[Command], typed: &str) -> Vec<usize> {
        Command::matching(commands, typed).into_iter().filter(|&at| commands[at].skill).collect()
    }
}

fn matching<T>(all: &[T], typed: &str, words: impl Fn(&T) -> (&String, &String)) -> Vec<usize> {
    let query = Query::new(typed);
    if query.is_empty() {
        return (0..all.len()).collect();
    }
    let mut ranked: Vec<_> = all
        .iter()
        .enumerate()
        .filter_map(|(index, one)| {
            let (name, about) = words(one);
            Some((query.rank(name, about)?, index))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|(_, index)| index).collect()
}

/// What the window does itself, which rein has no way to: a process holds
/// one conversation, and a pane is the window's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnCommand {
    New,
    Resume,
    Exit,
    Copy,
    /// Undo has a sheet of its own, and the tasks a panel.
    Undo,
    Tasks,
}

impl OwnCommand {
    /// What `line` asks for, if it's one of these. Only the word alone:
    /// anything after it isn't what it looks like.
    pub fn of(line: &str) -> Option<OwnCommand> {
        match line.trim() {
            "/new" | "/clear" => Some(OwnCommand::New),
            "/resume" => Some(OwnCommand::Resume),
            "/exit" | "/quit" => Some(OwnCommand::Exit),
            "/copy" => Some(OwnCommand::Copy),
            "/undo" => Some(OwnCommand::Undo),
            "/tasks" => Some(OwnCommand::Tasks),
            _ => None,
        }
    }
}

/// What a line sets of what an agent offers to be set. Picked or typed it's
/// the same choice, and goes as one: nobody wrote anything to the agent, so
/// nothing's written in the conversation. The word alone asks which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sets<'a> {
    /// As the agent offers it.
    pub option: &'static str,
    pub value: &'a str,
}

impl Sets<'_> {
    pub fn of(line: &str) -> Option<Sets<'_>> {
        let (word, value) = line.trim().strip_prefix('/')?.split_once(char::is_whitespace)?;
        let option = ["model", "mode", "effort", "sandbox"].into_iter().find(|option| *option == word)?;
        Some(Sets { option, value: value.trim_start() })
    }
}

/// `line` starts with a `/`, so rein says it back as it runs it, if it's a
/// command: it may have a key in it, and only rein knows.
pub fn says_back(line: &str) -> bool {
    line.trim_start().starts_with('/')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn menu() -> Menu {
        let params = json!({ "title": "choose a model", "items": [{ "name": "gpt-5", "about": "OpenAI" }, { "name": "opus", "about": "Anthropic · 200,000 tokens" }, { "name": "sonnet", "about": "Anthropic" }], "current": 1, "at": null });
        match Asking::of(CHOOSE, &params) {
            Some(Asking::Choose(menu)) => menu,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_menu_starts_at_the_one_in_use() {
        assert_eq!(menu().first(), 1);
        let later = Menu { at: Some(2), ..menu() };
        assert_eq!(later.first(), 2, "unless it says otherwise");
        assert_eq!(Menu { current: Some(9), at: None, ..menu() }.first(), 0, "and never off its end");
    }

    #[test]
    fn typing_narrows_a_menu_by_the_same_rule_as_the_terminal() {
        assert_eq!(menu().matching(""), [0, 1, 2], "all of it, in order, before anything's typed");
        assert_eq!(menu().matching("anthropic"), [1, 2], "a word in what's said of it counts");
        assert_eq!(menu().matching("opus 200"), [1], "and every word has to be there");
        assert!(menu().matching("zzz").is_empty());
    }

    #[test]
    fn what_cant_be_read_is_asked_of_nobody() {
        assert_eq!(Asking::of(CHOOSE, &json!({ "items": [] })), None, "a list with no title");
        assert_eq!(Asking::of(CHOOSE, &json!({ "title": "t", "items": [{ "about": "no name" }] })), None);
        assert_eq!(Asking::of(YES, &json!({ "title": "t", "answers": [{ "says": "no key" }] })), None);
        assert_eq!(Asking::of("_other", &json!({})), None);
    }

    #[test]
    fn a_line_is_secret_only_if_it_says_so() {
        let asked = |params| Asking::of(READ, &params);
        assert_eq!(asked(json!({ "prompt": "name" })), Some(Asking::Read { prompt: "name".to_owned(), secret: false }));
        assert_eq!(asked(json!({ "prompt": "key", "secret": true })), Some(Asking::Read { prompt: "key".to_owned(), secret: true }));
    }

    #[test]
    fn commands_are_listed_with_what_they_take() {
        let listed = Command::listed(
            &json!({ "availableCommands": [{ "name": "compact", "description": "make room", "input": { "hint": "[focus]" } }, { "name": "status", "description": "how it stands" }] }),
        );
        assert_eq!(listed[0].hint.as_deref(), Some("[focus]"));
        assert_eq!(listed[1].hint, None);
        assert_eq!(Command::matching(&listed, "sta"), [1]);
        assert_eq!(Command::matching(&listed, ""), [0, 1]);
    }

    #[test]
    fn only_a_skill_loads_from_later_in_a_message() {
        let listed = Command::listed(
            &json!({ "availableCommands": [{ "name": "status", "description": "how it stands" }, { "name": "stats", "description": "numbers", "_meta": { SKILL: true } }] }),
        );
        assert_eq!(Command::matching(&listed, "sta"), [0, 1]);
        assert_eq!(Command::skills_matching(&listed, "sta"), [1]);
    }

    #[test]
    fn only_the_word_alone_is_the_windows_own() {
        assert_eq!(OwnCommand::of("/new"), Some(OwnCommand::New));
        assert_eq!(OwnCommand::of("/clear"), Some(OwnCommand::New));
        assert_eq!(OwnCommand::of("  /copy  "), Some(OwnCommand::Copy));
        assert_eq!(OwnCommand::of("/quit"), Some(OwnCommand::Exit));
        assert_eq!(OwnCommand::of("/new thing"), None);
        assert_eq!(OwnCommand::of("/undo"), Some(OwnCommand::Undo));
        assert_eq!(OwnCommand::of("/model"), None);
    }

    #[test]
    fn a_line_that_sets_an_option_says_which_and_to_what() {
        assert_eq!(Sets::of("/model groq/llama"), Some(Sets { option: "model", value: "groq/llama" }));
        assert_eq!(Sets::of("  /effort   high "), Some(Sets { option: "effort", value: "high" }));
        assert_eq!(Sets::of("/mode ask"), Some(Sets { option: "mode", value: "ask" }));
        assert_eq!(Sets::of("/sandbox on"), Some(Sets { option: "sandbox", value: "on" }));
        assert_eq!(Sets::of("/model"), None, "the word alone asks which");
        assert_eq!(Sets::of("/models groq"), None);
        assert_eq!(Sets::of("/login groq a-key"), None);
        assert_eq!(Sets::of("use /model groq"), None);
    }

    #[test]
    fn only_what_starts_with_a_slash_is_said_back() {
        assert!(says_back("/status"));
        assert!(says_back("  /login groq a-key"));
        assert!(!says_back("what about /status"));
    }
}
