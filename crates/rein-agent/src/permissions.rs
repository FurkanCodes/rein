//! What the model may do without asking. A mode sets the broad policy;
//! rules in a project file refine it for specific commands:
//!
//! ```text
//! # .rein/permissions: one rule per line
//! allow cargo test
//! allow git status
//! ask   git commit
//! deny  git push
//! ```
//!
//! Deny beats ask beats allow. A command only runs without asking if every
//! part of it is allowed: `allow cargo test` doesn't cover
//! `cargo test && rm -rf target`. Anything the splitter can't take apart,
//! like `$(...)`, redirects or globs, is asked about, never allowed.
//!
//! Rules aren't a sandbox: `deny rm` doesn't stop `/bin/rm`, or a script
//! that deletes files itself. They decide when rein asks you.

use rein_llm::ToolCall;
use serde::Deserialize;

use crate::tools::ToolName;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Ask before every command and file change.
    #[default]
    Ask,
    /// File changes inside the project are fine; commands still ask.
    Edits,
    /// Look, never change: file changes and commands are refused.
    ReadOnly,
    /// Run everything without asking, except what deny rules block.
    Trust,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Ask, Mode::Edits, Mode::ReadOnly, Mode::Trust];

    /// Its name, as `--mode` and `/mode` take it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Edits => "edits",
            Mode::ReadOnly => "read-only",
            Mode::Trust => "trust",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    /// A mode by name; what's wrong, with the names there are, if none.
    fn from_str(name: &str) -> Result<Mode, String> {
        Mode::ALL.into_iter().find(|mode| mode.as_str() == name.trim()).ok_or_else(|| format!("no mode called {name}; use ask, edits, read-only or trust"))
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    /// Refused, with the reason the model is told.
    Deny(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    kind: Kind,
    /// The command's first words, e.g. `["cargo", "test"]`.
    prefix: Vec<String>,
}

impl Rule {
    /// Whole words only: `ls` matches `ls -la` but not `lsof`.
    fn matches(&self, command: &[String]) -> bool {
        command.starts_with(&self.prefix)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Permissions {
    pub mode: Mode,
    rules: Vec<Rule>,
    /// Exact commands you said yes to for the rest of this session.
    approved: Vec<Vec<String>>,
    /// Whether you said yes to all file changes for this session.
    approved_edits: bool,
}

#[derive(Deserialize)]
struct Command {
    command: String,
}

impl Permissions {
    /// Reads rules in the format shown at the top of this file. Blank lines
    /// and `#` comments are skipped.
    pub fn new(mode: Mode, rules: &str) -> Result<Self, String> {
        let mut parsed = Vec::new();
        for (n, line) in rules.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let kind = match word {
                "allow" => Kind::Allow,
                "ask" => Kind::Ask,
                "deny" => Kind::Deny,
                _ => return Err(format!("line {}: start with allow, ask or deny", n + 1)),
            };
            let prefix = match split_command(rest).as_deref() {
                Some([one]) => one.clone(),
                _ => return Err(format!("line {}: give one plain command, like `cargo test`", n + 1)),
            };
            parsed.push(Rule { kind, prefix });
        }
        Ok(Permissions { mode, rules: parsed, ..Default::default() })
    }

    /// Forgets what you said "always" to: that was for one conversation.
    pub fn forget_approvals(&mut self) {
        self.approved.clear();
        self.approved_edits = false;
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn decide(&self, call: &ToolCall) -> Decision {
        match ToolName::of(call) {
            // `read`, `grep` and `glob` can only look, and only inside the
            // project; `plan` only writes a checklist on screen.
            Some(tool) if tool.only_looks() || tool == ToolName::Plan => Decision::Allow,
            Some(tool) if tool.changes_files() => match self.mode {
                Mode::ReadOnly => Decision::Deny("File changes are turned off: rein is in read-only mode.".into()),
                Mode::Edits | Mode::Trust => Decision::Allow,
                Mode::Ask if self.approved_edits => Decision::Allow,
                Mode::Ask => Decision::Ask,
            },
            Some(ToolName::Bash) => match serde_json::from_str::<Command>(&call.arguments) {
                Ok(c) => self.decide_command(&c.command),
                Err(_) => Decision::Ask,
            },
            // `recall` never comes here: the agent runs it itself.
            Some(_) | None => Decision::Ask,
        }
    }

    fn decide_command(&self, line: &str) -> Decision {
        let parts = split_command(line);
        // Deny first. When the splitter gives up, check every run of words
        // in the line instead, cut at anything that can start a command
        // and with quotes taken off, so a deny rule still can't be dodged
        // by adding a `$` somewhere or gluing the command to a `;`.
        let words = loose_words(line);
        let fallback: Vec<&[String]> = (0..words.len()).map(|i| &words[i..]).collect();
        let candidates: Vec<&[String]> = match &parts {
            Some(parts) => parts.iter().map(Vec::as_slice).collect(),
            None => fallback,
        };
        let denies = self.rules.iter().filter(|r| r.kind == Kind::Deny);
        if let Some(rule) = denies.clone().find(|r| candidates.iter().any(|c| r.matches(c))) {
            return Decision::Deny(format!("Blocked by a rule: `deny {}`.", rule.prefix.join(" ")));
        }
        match self.mode {
            // What rein couldn't take apart, it can't promise no deny rule
            // covers: it asks, rather than trust a guess.
            Mode::Trust if parts.is_none() && denies.count() > 0 => return Decision::Ask,
            Mode::Trust => return Decision::Allow,
            Mode::ReadOnly => return Decision::Deny("Commands are turned off: rein is in read-only mode.".into()),
            Mode::Ask | Mode::Edits => {}
        }
        let Some(parts) = parts else { return Decision::Ask };
        let rule = |part: &Vec<String>, kind: Kind| self.rules.iter().any(|r| r.kind == kind && r.matches(part));
        if parts.iter().any(|p| rule(p, Kind::Ask)) {
            return Decision::Ask;
        }
        if parts.iter().all(|p| rule(p, Kind::Allow) || self.approved.contains(p)) {
            return Decision::Allow;
        }
        Decision::Ask
    }

    /// "Yes, and don't ask again this session": for a command, exactly these
    /// commands; for a file change, all file changes.
    pub fn remember(&mut self, call: &ToolCall) {
        match ToolName::of(call) {
            Some(ToolName::Write | ToolName::Edit) => self.approved_edits = true,
            Some(ToolName::Bash) => {
                if let Ok(c) = serde_json::from_str::<Command>(&call.arguments)
                    && let Some(parts) = split_command(&c.command)
                {
                    self.approved.extend(parts);
                }
            }
            _ => {}
        }
    }
}

/// The words of a line rein couldn't take apart, for deny rules to look
/// through: split at spaces and at everything that can start or end a
/// command (`;`, `&`, `|`, brackets, backticks, `$`), quotes and escapes
/// taken off. A guess, but one that errs towards finding a command.
fn loose_words(line: &str) -> Vec<String> {
    line.split(|c: char| c.is_whitespace() || ";&|(){}`$<>".contains(c))
        .map(|word| word.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect::<String>())
        .filter(|word| !word.is_empty())
        .collect()
}

/// Splits a shell command line into its commands, each as a list of words,
/// or returns `None` for anything it can't fully understand. It knows plain
/// words, quotes, and `&&`, `||`, `;`, `|` and newlines between commands.
/// Everything that could run or read something else gives up: `$`,
/// backticks, `( )`, `{ }`, `< >`, globs, `~`, a lone `&`, and leading
/// `NAME=value` settings.
pub fn split_command(line: &str) -> Option<Vec<Vec<String>>> {
    let mut commands = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();

    fn end_word(commands: &mut [Vec<String>], word: &mut String, in_word: &mut bool) {
        if *in_word {
            commands.last_mut().expect("never empty").push(std::mem::take(word));
            *in_word = false;
        }
    }

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => end_word(&mut commands, &mut word, &mut in_word),
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        // Expansion and escapes still work inside double quotes.
                        '$' | '`' | '\\' => return None,
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next()? {
                    '\n' => return None,
                    c => word.push(c),
                }
            }
            '&' | '|' | ';' | '\n' => {
                match (c, chars.peek()) {
                    ('&', Some('&')) | ('|', Some('|')) => {
                        chars.next();
                    }
                    ('&', _) => return None, // `&` alone runs in the background
                    _ => {}
                }
                end_word(&mut commands, &mut word, &mut in_word);
                if commands.last().expect("never empty").is_empty() {
                    return None; // `&& ls`, `ls ;; ls`
                }
                commands.push(Vec::new());
            }
            '$' | '`' | '(' | ')' | '{' | '}' | '<' | '>' | '*' | '?' | '[' | ']' | '~' | '!' | '#' => return None,
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut commands, &mut word, &mut in_word);
    // A trailing separator leaves an empty last command: `ls &&` is incomplete.
    if commands.iter().any(Vec::is_empty) {
        return None;
    }
    // `NAME=value cmd` changes the environment `cmd` runs in.
    if commands.iter().any(|c| c[0].contains('=')) {
        return None;
    }
    Some(commands)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn split(line: &str) -> Option<Vec<Vec<String>>> {
        split_command(line)
    }

    fn cmds(commands: &[&[&str]]) -> Option<Vec<Vec<String>>> {
        Some(commands.iter().map(|c| c.iter().map(|w| w.to_string()).collect()).collect())
    }

    #[test]
    fn splits_plain_commands_and_quotes() {
        assert_eq!(split("cargo test"), cmds(&[&["cargo", "test"]]));
        assert_eq!(split("cargo test && rm -rf target"), cmds(&[&["cargo", "test"], &["rm", "-rf", "target"]]));
        assert_eq!(split("git log | head -5; git status"), cmds(&[&["git", "log"], &["head", "-5"], &["git", "status"]]));
        // Quotes keep operators inside one word.
        assert_eq!(split("git commit -m 'fix: a && b'"), cmds(&[&["git", "commit", "-m", "fix: a && b"]]));
        assert_eq!(split(r#"echo "two words" plain\ space"#), cmds(&[&["echo", "two words", "plain space"]]));
        assert_eq!(split("cargo test --features=x"), cmds(&[&["cargo", "test", "--features=x"]]));
    }

    #[test]
    fn gives_up_on_anything_it_cant_understand() {
        for line in [
            "echo $(pwd)",
            "echo `pwd`",
            "echo $HOME",
            r#"echo "hi $USER""#,
            "ls > out.txt",
            "cat < secret",
            "echo hi & echo bye",
            "ls *.rs",
            "cat ~/.ssh/id_rsa",
            "(cd x && ls)",
            "FOO=1 cargo test",
            "cargo test &&",
            "&& ls",
            "echo 'unclosed",
            "ls # comment",
        ] {
            assert_eq!(split_command(line), None, "{line}");
        }
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall { call_id: "c".into(), name: "bash".into(), arguments: json!({ "command": command }).to_string() }
    }

    fn file(tool: &str) -> ToolCall {
        ToolCall { call_id: "c".into(), name: tool.into(), arguments: json!({"path": "a.txt", "content": "x"}).to_string() }
    }

    const RULES: &str = "# rules\nallow cargo test\nallow git status\nallow ls\nask git status --short\ndeny git push\n";

    #[test]
    fn every_part_must_be_allowed() {
        let p = Permissions::new(Mode::Ask, RULES).unwrap();
        assert_eq!(p.decide(&bash("cargo test")), Decision::Allow);
        assert_eq!(p.decide(&bash("cargo test --release && git status")), Decision::Allow);
        assert_eq!(p.decide(&bash("cargo test && rm -rf target")), Decision::Ask);
        assert_eq!(p.decide(&bash("cargo test > log.txt")), Decision::Ask);
        // Whole words: `ls` doesn't cover `lsof`.
        assert_eq!(p.decide(&bash("lsof -i")), Decision::Ask);
    }

    #[test]
    fn deny_beats_ask_beats_allow() {
        let p = Permissions::new(Mode::Ask, RULES).unwrap();
        // `ask git status --short` wins over `allow git status`.
        assert_eq!(p.decide(&bash("git status --short")), Decision::Ask);
        let blocked = Decision::Deny("Blocked by a rule: `deny git push`.".into());
        assert_eq!(p.decide(&bash("cargo test && git push")), blocked);
        // Even when the splitter gives up, and even in trust mode.
        assert_eq!(p.decide(&bash("echo $X; git push origin main")), blocked);
        let trusting = Permissions::new(Mode::Trust, RULES).unwrap();
        assert_eq!(trusting.decide(&bash("git push --force")), blocked);
        assert_eq!(trusting.decide(&bash("rm -rf target")), Decision::Allow);
    }

    #[test]
    fn a_deny_rule_cant_be_dodged_by_what_the_splitter_gives_up_on() {
        let trusting = Permissions::new(Mode::Trust, "deny git push").unwrap();
        // Glued to punctuation, quoted, or after something rein can't read:
        // still the command the rule is about.
        for dodge in ["echo $X;git push", "x=1;git push", "echo $(true) && git 'push'", "(git push)", "true|git push", "`git push`", "echo \"$(git push)\""] {
            assert_ne!(trusting.decide(&bash(dodge)), Decision::Allow, "{dodge}");
        }
        // What can't be read, with a deny rule in place, is asked about.
        assert_eq!(trusting.decide(&bash("echo $(curl example.com)")), Decision::Ask);
        // Without deny rules, trust is trust.
        let trusting = Permissions::new(Mode::Trust, "").unwrap();
        assert_eq!(trusting.decide(&bash("echo $X;git push")), Decision::Allow);
    }

    #[test]
    fn modes_decide_file_changes_and_commands() {
        let ask = Permissions::new(Mode::Ask, "").unwrap();
        assert_eq!(ask.decide(&file("write")), Decision::Ask);
        assert_eq!(ask.decide(&file("read")), Decision::Allow);
        let edits = Permissions::new(Mode::Edits, "").unwrap();
        assert_eq!(edits.decide(&file("edit")), Decision::Allow);
        assert_eq!(edits.decide(&bash("ls")), Decision::Ask);
        let read_only = Permissions::new(Mode::ReadOnly, RULES).unwrap();
        assert!(matches!(read_only.decide(&file("write")), Decision::Deny(_)));
        assert!(matches!(read_only.decide(&bash("cargo test")), Decision::Deny(_)));
        assert_eq!(read_only.decide(&file("read")), Decision::Allow);
        // Searching only looks, so it never asks, in any mode.
        for tool in ["grep", "glob"] {
            assert_eq!(ask.decide(&file(tool)), Decision::Allow);
            assert_eq!(read_only.decide(&file(tool)), Decision::Allow);
        }
    }

    #[test]
    fn remembering_covers_exactly_what_was_approved() {
        let mut p = Permissions::new(Mode::Ask, "").unwrap();
        p.remember(&bash("cargo build && cargo test"));
        assert_eq!(p.decide(&bash("cargo test")), Decision::Allow);
        assert_eq!(p.decide(&bash("cargo test --release")), Decision::Ask);
        p.remember(&file("edit"));
        assert_eq!(p.decide(&file("write")), Decision::Allow);
    }

    #[test]
    fn bad_rules_are_reported_with_their_line() {
        assert_eq!(Permissions::new(Mode::Ask, "allow ls\npermit ls").unwrap_err(), "line 2: start with allow, ask or deny");
        assert_eq!(Permissions::new(Mode::Ask, "allow ls && rm").unwrap_err(), "line 1: give one plain command, like `cargo test`");
    }
}
