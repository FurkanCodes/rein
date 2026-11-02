//! Slash commands: what you can type in a conversation besides a message.
//! The same names as Claude Code and Codex where they have one.

use rein_agent::permissions::Mode;

/// Every command: its name, what it takes, and what it does.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("/help", "", "list these commands"),
    ("/mode", "[ask|edits|read-only|trust]", "show or change what runs without asking"),
    ("/model", "[id]", "list the models, or switch to one"),
    ("/effort", "[low|medium|high|default]", "show or change how hard the model thinks"),
    ("/status", "", "the model, mode, context and conversation"),
    ("/compact", "[focus]", "make room in the conversation now"),
    ("/copy", "", "copy rein's last answer, as it wrote it"),
    ("/undo", "", "put back the files rein changed in its last turn"),
    ("/new", "", "start a fresh conversation (also /clear)"),
    ("/resume", "", "continue a saved conversation from this folder"),
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
    /// `Some(None)` goes back to the model's default.
    Effort(Option<Option<String>>),
    Status,
    Compact(Option<String>),
    Copy,
    Undo,
    New,
    Resume,
    Exit,
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
            Some(name) => Mode::parse(name).map(|m| Slash::Mode(Some(m))).ok_or_else(|| format!("no mode called {name}; use ask, edits, read-only or trust")),
        },
        "/model" => Ok(Slash::Model(arg)),
        "/effort" => match arg.as_deref() {
            None => Ok(Slash::Effort(None)),
            Some("default") => Ok(Slash::Effort(Some(None))),
            Some(level @ ("minimal" | "low" | "medium" | "high" | "xhigh" | "max")) => Ok(Slash::Effort(Some(Some(level.to_owned())))),
            Some(other) => Err(format!("no effort called {other}; use low, medium, high, xhigh, max or default")),
        },
        "/status" => Ok(Slash::Status),
        "/compact" => Ok(Slash::Compact(arg)),
        "/copy" => Ok(Slash::Copy),
        "/undo" => Ok(Slash::Undo),
        "/new" => Ok(Slash::New),
        "/resume" => Ok(Slash::Resume),
        "/exit" => Ok(Slash::Exit),
        other => Err(format!("there's no {other} command; /help lists them")),
    })
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
        assert_eq!(parse("/effort default"), Some(Ok(Slash::Effort(Some(None)))));
        assert_eq!(parse("/compact keep the test output"), Some(Ok(Slash::Compact(Some("keep the test output".into())))));
        assert_eq!(parse("/clear"), Some(Ok(Slash::New)));
        assert_eq!(parse("/quit"), Some(Ok(Slash::Exit)));
    }

    #[test]
    fn says_what_went_wrong() {
        assert_eq!(parse("/mode yolo"), Some(Err("no mode called yolo; use ask, edits, read-only or trust".into())));
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
        assert_eq!(names("/co"), ["/compact", "/copy"]);
        assert_eq!(names("/"), COMMANDS.iter().map(|c| c.0).collect::<Vec<_>>());
        assert!(names("/mode ").is_empty());
        assert!(names("hello").is_empty());
        assert!(help()[1].starts_with("/mode [ask|edits|read-only|trust]"));
    }
}
