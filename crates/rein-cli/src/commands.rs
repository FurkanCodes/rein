//! Slash commands: what you can type in a conversation besides a message.
//! The same names as Claude Code and Codex where they have one.

use rein_agent::permissions::Mode;
use rein_llm::Effort;

/// Every command: its name, what it takes, and what it does.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("/help", "", "list these commands"),
    ("/mode", "[ask|edits|read-only|trust]", "show or change what runs without asking"),
    ("/model", "[id]", "list the models, or switch to one"),
    ("/login", "[provider]", "sign in to a provider, or see what pays for each"),
    ("/logout", "[provider]", "forget a provider's key (chatgpt: sign out)"),
    ("/effort", "[level|default]", "change how hard the model thinks"),
    ("/status", "", "the model, mode, context and conversation"),
    ("/compact", "[focus]", "make room in the conversation now"),
    ("/copy", "", "copy rein's last answer, as it wrote it"),
    ("/undo", "", "put back the files rein changed in its last turn"),
    ("/new", "", "start a fresh conversation (also /clear)"),
    ("/resume", "", "continue a saved conversation from this folder"),
    ("/peers", "", "the reins in your other terminals, and what each is doing"),
    ("/send", "[name] [message]", "write to another rein"),
    ("/delegate", "[name] [task]", "hand another rein a task; its result comes back here"),
    ("/spawn", "[name] [-m model] [--edits] [task]", "start a sub-agent in a new tab; its result comes back here"),
    ("/inbox", "", "what other reins sent that waits to be let in"),
    ("/name", "[name]", "what other reins call this one"),
    ("/workspace", "[name]", "the group of reins this one works with"),
    ("/exit", "", "leave (also /quit, Ctrl-D)"),
];

/// Other names for the same commands, as other harnesses spell them.
const ALIASES: &[(&str, &str)] = &[("/clear", "/new"), ("/quit", "/exit"), ("/permissions", "/mode")];

#[derive(Debug, PartialEq, Eq)]
pub enum Slash {
    Help,
    /// `None` shows the mode.
    Mode(Option<Mode>),
    Model(Option<String>),
    /// `None` lists the providers and what pays for each.
    Login(Option<String>),
    /// `None` asks which.
    Logout(Option<String>),
    /// `None` asks which.
    Effort(Option<EffortSetting>),
    Status,
    Compact(Option<String>),
    Copy,
    Undo,
    New,
    Resume,
    Peers,
    /// To whom, and what; either may still be asked for.
    Send(Option<String>, Option<String>),
    Delegate(Option<String>, Option<String>),
    Inbox,
    Spawn(Spawn),
    /// `None` says what it is.
    Name(Option<String>),
    Workspace(Option<String>),
    Exit,
}

/// What `/spawn` asks for: what's not given is asked for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Spawn {
    pub role: Option<String>,
    pub task: Option<String>,
    pub model: Option<String>,
    /// It changes files: a worktree of its own.
    pub edits: bool,
}

impl Spawn {
    /// `tests -m claude-opus-5-5 --edits run the suite`: a name first, then
    /// any of the options, then the task.
    fn parse(rest: &str) -> Result<Spawn, String> {
        let mut spawn = Spawn::default();
        let mut words = rest.split_whitespace().peekable();
        if let Some(role) = words.next_if(|word| !word.starts_with('-')) {
            spawn.role = Some(role.trim_start_matches('@').to_owned());
        }
        while let Some(&word) = words.peek() {
            match word {
                "-m" | "--model" => {
                    words.next();
                    spawn.model = Some(words.next().ok_or("-m needs a model")?.to_owned());
                }
                "--edits" => {
                    words.next();
                    spawn.edits = true;
                }
                _ => break,
            }
        }
        let task = words.collect::<Vec<_>>().join(" ");
        spawn.task = (!task.is_empty()).then_some(task);
        Ok(spawn)
    }
}

/// What `/effort` sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSetting {
    /// Whatever the model does when not told.
    Default,
    Level(Effort),
}

impl EffortSetting {
    pub fn level(self) -> Option<Effort> {
        match self {
            Self::Default => None,
            Self::Level(effort) => Some(effort),
        }
    }
}

/// What `line` asks for: `None` when it's a message, not a command. A
/// line counts as a command when its first word looks like one (`/mode`),
/// so a message that starts with a path, like `/usr/bin is…`, stays a
/// message.
pub fn parse(line: &str) -> Option<Result<Slash, String>> {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let name = word.strip_prefix('/')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
        return None;
    }
    let word = ALIASES.iter().find(|(alias, _)| *alias == word).map_or(word, |(_, name)| *name);
    let rest = rest.trim();
    let arg = (!rest.is_empty()).then(|| rest.to_owned());
    Some(match word {
        "/help" => Ok(Slash::Help),
        "/mode" => match &arg {
            None => Ok(Slash::Mode(None)),
            Some(name) => name.parse().map(|m| Slash::Mode(Some(m))),
        },
        "/model" => Ok(Slash::Model(arg)),
        "/login" => Ok(Slash::Login(arg)),
        "/logout" => Ok(Slash::Logout(arg)),
        "/effort" => match arg.as_deref() {
            None => Ok(Slash::Effort(None)),
            Some("default") => Ok(Slash::Effort(Some(EffortSetting::Default))),
            Some(level) => level.parse().map(|e| Slash::Effort(Some(EffortSetting::Level(e)))).map_err(|e| format!("{e}, or default")),
        },
        "/status" => Ok(Slash::Status),
        "/compact" => Ok(Slash::Compact(arg)),
        "/copy" => Ok(Slash::Copy),
        "/undo" => Ok(Slash::Undo),
        "/new" => Ok(Slash::New),
        "/resume" => Ok(Slash::Resume),
        "/peers" => Ok(Slash::Peers),
        "/send" => {
            let (to, text) = to_and_rest(rest);
            Ok(Slash::Send(to, text))
        }
        "/delegate" => {
            let (to, task) = to_and_rest(rest);
            Ok(Slash::Delegate(to, task))
        }
        "/inbox" => Ok(Slash::Inbox),
        "/spawn" => Spawn::parse(rest).map(Slash::Spawn),
        "/name" => Ok(Slash::Name(arg)),
        "/workspace" => Ok(Slash::Workspace(arg)),
        "/exit" => Ok(Slash::Exit),
        other => Err(format!("there's no {other} command; /help lists them")),
    })
}

/// `api fix the tests` → the rein it's for, and what to tell it.
fn to_and_rest(rest: &str) -> (Option<String>, Option<String>) {
    let (to, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let to = to.trim_start_matches('@');
    ((!to.is_empty()).then(|| to.to_owned()), (!text.trim().is_empty()).then(|| text.trim().to_owned()))
}

/// The commands that start with what's typed so far, for the list under
/// the box. Only while the first word is still being typed.
pub fn matching(typed: &str) -> Vec<(&'static str, &'static str, &'static str)> {
    if !typed.starts_with('/') || typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS.iter().copied().filter(|(name, _, _)| name.starts_with(typed)).collect()
}

/// `/help`'s text.
pub fn help() -> Vec<String> {
    let width = COMMANDS.iter().map(|(name, args, _)| name.len() + 1 + args.len()).max().unwrap_or(0);
    COMMANDS
        .iter()
        .map(|(name, args, what)| {
            let usage = if args.is_empty() { (*name).to_owned() } else { format!("{name} {args}") };
            format!("{usage:<width$}  {what}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_commands_and_their_arguments() {
        assert_eq!(parse("/mode edits"), Some(Ok(Slash::Mode(Some(Mode::Edits)))));
        assert_eq!(parse("  /permissions  "), Some(Ok(Slash::Mode(None))));
        assert_eq!(parse("/model gpt-6.1-terra"), Some(Ok(Slash::Model(Some("gpt-6.1-terra".into())))));
        assert_eq!(parse("/effort default"), Some(Ok(Slash::Effort(Some(EffortSetting::Default)))));
        assert_eq!(parse("/effort xhigh"), Some(Ok(Slash::Effort(Some(EffortSetting::Level(Effort::XHigh))))));
        assert_eq!(parse("/effort extreme"), Some(Err("no effort called extreme; use off, minimal, low, medium, high, xhigh or max, or default".into())));
        assert_eq!(parse("/compact keep the test output"), Some(Ok(Slash::Compact(Some("keep the test output".into())))));
        assert_eq!(parse("/clear"), Some(Ok(Slash::New)));
        assert_eq!(parse("/quit"), Some(Ok(Slash::Exit)));
        assert_eq!(parse("/login"), Some(Ok(Slash::Login(None))));
        assert_eq!(parse("/login anthropic"), Some(Ok(Slash::Login(Some("anthropic".into())))));
        assert_eq!(parse("/logout openrouter"), Some(Ok(Slash::Logout(Some("openrouter".into())))));
        assert_eq!(parse("/logout"), Some(Ok(Slash::Logout(None))));
        assert_eq!(parse("/send api run the tests"), Some(Ok(Slash::Send(Some("api".into()), Some("run the tests".into())))));
        assert_eq!(parse("/delegate @web"), Some(Ok(Slash::Delegate(Some("web".into()), None))));
        assert_eq!(parse("/send"), Some(Ok(Slash::Send(None, None))));
        assert_eq!(parse("/name review"), Some(Ok(Slash::Name(Some("review".into())))));
        let spawn = |role: Option<&str>, model: Option<&str>, edits, task: Option<&str>| {
            Some(Ok(Slash::Spawn(Spawn { role: role.map(Into::into), task: task.map(Into::into), model: model.map(Into::into), edits })))
        };
        assert_eq!(parse("/spawn tests run the suite"), spawn(Some("tests"), None, false, Some("run the suite")));
        assert_eq!(parse("/spawn review -m claude-opus-5-5 --edits fix it"), spawn(Some("review"), Some("claude-opus-5-5"), true, Some("fix it")));
        assert_eq!(parse("/spawn"), spawn(None, None, false, None));
        assert_eq!(parse("/spawn docs"), spawn(Some("docs"), None, false, None));
        assert_eq!(parse("/spawn x -m"), Some(Err("-m needs a model".into())));
    }

    #[test]
    fn says_what_went_wrong() {
        assert_eq!(parse("/mode yolo"), Some(Err("no mode called yolo; use ask, edits, auto, read-only or trust".into())));
        assert_eq!(parse("/frobnicate"), Some(Err("there's no /frobnicate command; /help lists them".into())));
    }

    #[test]
    fn messages_that_start_with_a_slash_stay_messages() {
        assert_eq!(parse("/usr/bin/env is missing"), None);
        assert_eq!(parse("fix /mode please"), None);
        assert_eq!(parse("/"), None);
    }

    #[test]
    fn suggests_while_the_name_is_typed() {
        let names = |typed| matching(typed).into_iter().map(|(name, _, _)| name).collect::<Vec<_>>();
        assert_eq!(names("/mo"), ["/mode", "/model"]);
        assert_eq!(names("/lo"), ["/login", "/logout"]);
        assert_eq!(names("/co"), ["/compact", "/copy"]);
        assert_eq!(names("/"), COMMANDS.iter().map(|c| c.0).collect::<Vec<_>>());
        assert!(names("/mode ").is_empty());
        assert!(names("hello").is_empty());
        assert!(help()[1].starts_with("/mode [ask|edits|read-only|trust]"));
    }
}
