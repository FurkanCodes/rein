//! What the model may do without asking. A mode sets the broad policy,
//! the sandbox decides much of it (what runs inside the walls needs no
//! asking), and rules in a project file refine it for specific commands:
//!
//! ```text
//! # .rein/permissions: one rule per line
//! allow cargo test
//! allow git status
//! ask   git commit
//! deny  git push
//! ```
//!
//! Deny beats ask beats allow. A command only runs without asking by an
//! allow rule if every part of it is allowed: `allow cargo test` doesn't
//! cover `cargo test && rm -rf target`. Anything the splitter can't take
//! apart, like `$(...)`, redirects or globs, no allow rule covers, while
//! deny and ask rules still find the commands in it.
//!
//! Rules aren't a sandbox: `deny rm` doesn't stop `/bin/rm`, or a script
//! that deletes files itself. They decide when rein asks you; the sandbox
//! decides what a command can reach.
//!
//! The file comes with the project, which may be anyone's, so an allow
//! rule only spares asking about a command that stays in the sandbox.
//! Outside it a command has your keys: `allow cargo test` in a cloned repo
//! would otherwise run its build script with them. Only you say yes to that.

use rein_llm::ToolCall;
use serde::Deserialize;

use crate::sandbox::{self, Access, Run};
use crate::tools::{BashCall, ERROR, ToolName};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Ask before file changes, and before commands that do more than look.
    #[default]
    Ask,
    /// File changes inside the project are fine, and so are commands that
    /// stay in the sandbox; going past it asks.
    Edits,
    /// Never ask: work inside the sandbox, and refuse what would go past it.
    /// For leaving rein to work on its own.
    Auto,
    /// Look, never change: commands run only with look access, in the
    /// sandbox, and file changes are refused.
    ReadOnly,
    /// Run everything without asking, without the sandbox, except what deny
    /// rules block.
    Trust,
}

impl Mode {
    pub const ALL: [Mode; 5] = [Mode::Ask, Mode::Edits, Mode::Auto, Mode::ReadOnly, Mode::Trust];

    /// Its name, as `--mode` and `/mode` take it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Edits => "edits",
            Mode::Auto => "auto",
            Mode::ReadOnly => "read-only",
            Mode::Trust => "trust",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    /// A mode by name; what's wrong, with the names there are, if none.
    fn from_str(name: &str) -> Result<Mode, String> {
        Mode::ALL.into_iter().find(|mode| mode.as_str() == name.trim()).ok_or_else(|| {
            let names: Vec<&str> = Mode::ALL.iter().map(|m| m.as_str()).collect();
            format!("no mode called {name}; use {} or {}", names[..names.len() - 1].join(", "), names[names.len() - 1])
        })
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
    /// Exact commands you said yes to for the rest of this session, each
    /// with the access it was for.
    approved: Vec<(Vec<String>, Access)>,
    /// Whether you said yes to all file changes for this session.
    approved_edits: bool,
    /// MCP servers' tools that run without asking: those your own
    /// mcp.json allows, by the names the model knows them by.
    allowed_tools: Vec<String>,
    /// Those you said yes to for the rest of this session.
    approved_tools: Vec<String>,
    /// What holds commands in, if anything does, and what's protected.
    sandbox: sandbox::Shared,
}

/// A file tool's call, for the file it changes.
#[derive(Deserialize)]
struct FileCall {
    path: String,
}

/// What the model is told when auto mode refuses a command.
fn auto_refuses(access: Access) -> String {
    format!(
        "rein is in auto mode, where nothing asks: commands run in the sandbox with look or write access only. \
         Do without {access} access, or tell the user what you need it for."
    )
}

const AUTO_ASK_RULE: &str = "rein is in auto mode, where nothing asks, and a rule says to ask before this command. \
                             Do without it, or tell the user what you need it for.";
const AUTO_NO_SANDBOX: &str = "rein is in auto mode, where nothing asks, but there's no sandbox here to hold commands in, \
                               so no command runs. Tell the user what you need to run.";
const AUTO_PROTECTED: &str = "rein is in auto mode, where nothing asks, and this file runs code later or holds rein's rules: \
                              changing it takes the user's approval. Leave it, or tell the user what to change.";
const AUTO_MCP: &str = "rein is in auto mode, where nothing asks, and this MCP tool isn't one the user's own mcp.json allows \
                        (\"allow\": [...]). Do without it, or tell the user what you need it for.";
const READ_ONLY_MCP: &str = "rein is in read-only mode, and an MCP tool may change things wherever its server reaches: \
                             they're turned off.";
const READ_ONLY_CHANGES: &str = "File changes are turned off: rein is in read-only mode.";
const READ_ONLY_COMMANDS: &str = "rein is in read-only mode: commands run with look access only, and change nothing.";
const READ_ONLY_NO_SANDBOX: &str = "Commands are turned off: rein is in read-only mode, and there's no sandbox here \
                                    to keep them from changing things.";

impl Permissions {
    /// Reads rules in the format shown at the top of this file. Blank lines
    /// and `#` comments are skipped. There's no sandbox until
    /// [`Permissions::with_sandbox`] gives one, so commands ask.
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

    /// Decides with `sandbox`: what it can hold in, and what it protects.
    pub fn with_sandbox(mut self, sandbox: sandbox::Shared) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn sandbox(&self) -> &sandbox::Sandbox {
        &self.sandbox
    }

    /// Forgets what you said "always" to: that was for one conversation.
    pub fn forget_approvals(&mut self) {
        self.approved.clear();
        self.approved_edits = false;
        self.approved_tools.clear();
    }

    /// The MCP servers' tools that run without asking from now on, as your
    /// own mcp.json allows them: what was offered before goes.
    pub fn allow_tools(&mut self, names: Vec<String>) {
        self.allowed_tools = names;
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn decide(&self, call: &ToolCall) -> Decision {
        match ToolName::of(call) {
            // `read`, `grep` and `glob` can only look, and only inside the
            // project; `plan` only writes a checklist on screen.
            Some(tool) if tool.only_looks() || tool == ToolName::Plan => Decision::Allow,
            Some(tool) if tool.changes_files() => self.decide_change(call),
            // What doesn't read as a command doesn't run as one either.
            Some(ToolName::Bash) => match serde_json::from_str::<BashCall>(&call.arguments) {
                Ok(c) => self.decide_command(c.command(), c.access()),
                Err(e) => Decision::Deny(format!("{ERROR}bad arguments: {e}")),
            },
            // `recall` never comes here: the agent runs it itself.
            Some(_) => Decision::Ask,
            None if rein_mcp::is_mcp(&call.name) => self.decide_tool(&call.name),
            None => Decision::Ask,
        }
    }

    /// An MCP server's tool: what it does, it does wherever its server
    /// reaches (an issue tracker, a database), beyond any wall of rein's,
    /// and what the server says of it (that it only reads, say) is its own
    /// word. So it asks, but for what you allowed yourself, and a mode that
    /// doesn't ask refuses it instead.
    fn decide_tool(&self, name: &str) -> Decision {
        let allowed = self.allowed_tools.iter().chain(&self.approved_tools).any(|allowed| allowed == name);
        match self.mode {
            Mode::ReadOnly => Decision::Deny(READ_ONLY_MCP.into()),
            Mode::Trust => Decision::Allow,
            _ if allowed => Decision::Allow,
            Mode::Auto => Decision::Deny(AUTO_MCP.into()),
            Mode::Ask | Mode::Edits => Decision::Ask,
        }
    }

    /// How `call` runs once it's allowed: a command in the sandbox, within
    /// the walls its access asks for, unless it asked to run outside, the
    /// mode is trust, or there's no sandbox here. rein's own tools run as
    /// they always do.
    pub fn how(&self, call: &ToolCall) -> Run {
        match BashCall::of(call) {
            Some(c) if self.mode != Mode::Trust && self.sandbox.available() => c.access().walls().map_or(Run::Unsandboxed, Run::Sandboxed),
            _ => Run::Unsandboxed,
        }
    }

    fn decide_change(&self, call: &ToolCall) -> Decision {
        // What runs code later is asked about in every mode that asks at all.
        let protected = serde_json::from_str::<FileCall>(&call.arguments).is_ok_and(|file| self.sandbox.places().protects(&file.path));
        match self.mode {
            Mode::ReadOnly => Decision::Deny(READ_ONLY_CHANGES.into()),
            Mode::Trust => Decision::Allow,
            Mode::Auto if protected => Decision::Deny(AUTO_PROTECTED.into()),
            _ if protected => Decision::Ask,
            Mode::Edits | Mode::Auto => Decision::Allow,
            Mode::Ask if self.approved_edits => Decision::Allow,
            Mode::Ask => Decision::Ask,
        }
    }

    fn decide_command(&self, line: &str, access: Access) -> Decision {
        let parts = split_command(line);
        // Deny first. When the splitter gives up, check every run of words
        // in the line instead, cut at anything that can start a command
        // and with quotes taken off, so a deny rule still can't be dodged
        // by adding a `$` somewhere or gluing the command to a `;`. Ask
        // rules are looked for the same way: the sandbox would otherwise
        // run what you asked to be asked about.
        let words = loose_words(line);
        let fallback: Vec<&[String]> = (0..words.len()).map(|i| &words[i..]).collect();
        let candidates: Vec<&[String]> = match &parts {
            Some(parts) => parts.iter().map(Vec::as_slice).collect(),
            None => fallback,
        };
        let covered = |kind: Kind| self.rules.iter().filter(|r| r.kind == kind).find(|r| candidates.iter().any(|c| r.matches(c)));
        if let Some(rule) = covered(Kind::Deny) {
            return Decision::Deny(format!("Blocked by a rule: `deny {}`.", rule.prefix.join(" ")));
        }
        let denies = self.rules.iter().filter(|r| r.kind == Kind::Deny).count();
        match self.mode {
            // What rein couldn't take apart, it can't promise no deny rule
            // covers: it asks, rather than trust a guess.
            Mode::Trust if parts.is_none() && denies > 0 => return Decision::Ask,
            Mode::Trust => return Decision::Allow,
            _ => {}
        }
        let sandboxed = self.sandbox.available();
        let asked = covered(Kind::Ask).is_some();
        // Inside the walls, nothing needs asking.
        let free = sandboxed
            && match access {
                Access::Look => true,
                Access::Write => matches!(self.mode, Mode::Edits | Mode::Auto),
                Access::Network | Access::Outside => false,
            };
        // The rules come with the project, which may be anyone's: they only
        // spare you asking about what stays in the walls. Outside them a
        // command gets your keys, so only you say yes to that.
        let in_walls = sandboxed && access != Access::Outside;
        let rule = |part: &Vec<String>, kind: Kind| in_walls && self.rules.iter().any(|r| r.kind == kind && r.matches(part));
        // Without a sandbox, the access a command says it needs is only words.
        let approved = |part: &Vec<String>| self.approved.iter().any(|(words, granted)| words == part && (*granted >= access || !sandboxed));
        let allowed = parts.as_ref().is_some_and(|parts| parts.iter().all(|p| rule(p, Kind::Allow) || approved(p)));
        match self.mode {
            Mode::ReadOnly if !sandboxed => Decision::Deny(READ_ONLY_NO_SANDBOX.into()),
            Mode::ReadOnly if access > Access::Look => Decision::Deny(READ_ONLY_COMMANDS.into()),
            Mode::Auto if asked => Decision::Deny(AUTO_ASK_RULE.into()),
            _ if asked => Decision::Ask,
            _ if free || allowed => Decision::Allow,
            Mode::Auto if !sandboxed => Decision::Deny(AUTO_NO_SANDBOX.into()),
            Mode::Auto => Decision::Deny(auto_refuses(access)),
            _ => Decision::Ask,
        }
    }

    /// "Yes, and don't ask again this session": for a command, exactly these
    /// commands, with as much access as this time; for a file change, all
    /// file changes but protected ones; for an MCP server's tool, that tool.
    pub fn remember(&mut self, call: &ToolCall) {
        match ToolName::of(call) {
            Some(ToolName::Write | ToolName::Edit) => self.approved_edits = true,
            Some(ToolName::Bash) => {
                if let Some(c) = BashCall::of(call)
                    && let Some(parts) = split_command(c.command())
                {
                    self.approved.extend(parts.into_iter().map(|part| (part, c.access())));
                }
            }
            None if rein_mcp::is_mcp(&call.name) => self.approved_tools.push(call.name.clone()),
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

    /// A server's tool may do anything wherever its server reaches, so it
    /// asks, but for what your own mcp.json allows; a project's rules
    /// never allow one, and the modes that don't ask refuse it.
    #[test]
    fn an_mcp_tool_asks_but_for_what_you_allowed_and_modes_that_dont_ask_refuse_it() {
        let call = |name: &str| ToolCall { call_id: "1".into(), name: name.into(), arguments: "{}".into() };
        let (issue, merge) = (call("mcp__github__get_issue"), call("mcp__github__merge"));
        let mut p = Permissions::new(Mode::Ask, "allow mcp__github__merge\nallow merge").unwrap();
        p.allow_tools(vec!["mcp__github__get_issue".into()]);
        assert_eq!(p.decide(&issue), Decision::Allow);
        assert_eq!(p.decide(&merge), Decision::Ask, "a project's rules don't allow a server's tool");
        p.remember(&merge);
        assert_eq!(p.decide(&merge), Decision::Allow, "you said always");
        p.forget_approvals();
        assert_eq!(p.decide(&merge), Decision::Ask, "for one conversation");
        p.mode = Mode::Edits;
        assert_eq!(p.decide(&merge), Decision::Ask, "edits are files, not this");
        p.mode = Mode::Auto;
        assert_eq!((p.decide(&issue), p.decide(&merge)), (Decision::Allow, Decision::Deny(AUTO_MCP.into())));
        p.mode = Mode::ReadOnly;
        assert_eq!((p.decide(&issue), p.decide(&merge)), (Decision::Deny(READ_ONLY_MCP.into()), Decision::Deny(READ_ONLY_MCP.into())));
        p.mode = Mode::Trust;
        assert_eq!(p.decide(&merge), Decision::Allow);
        p.allow_tools(Vec::new());
        p.mode = Mode::Ask;
        assert_eq!(p.decide(&issue), Decision::Ask, "what's allowed goes with the servers' tools it was for");
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
        // With the network, so the walls alone don't let it run.
        let p = sandboxed(Mode::Ask, RULES, "parts");
        assert_eq!(p.decide(&command("cargo test", "network")), Decision::Allow);
        assert_eq!(p.decide(&command("cargo test --release && git status", "network")), Decision::Allow);
        assert_eq!(p.decide(&command("cargo test && rm -rf target", "network")), Decision::Ask);
        assert_eq!(p.decide(&command("cargo test > log.txt", "network")), Decision::Ask);
        // Whole words: `ls` doesn't cover `lsof`.
        assert_eq!(p.decide(&command("lsof -i", "network")), Decision::Ask);
    }

    #[test]
    fn a_projects_rules_never_let_a_command_out_of_the_walls() {
        // A cloned repo's own `.rein/permissions`: outside the sandbox, its
        // `cargo test` would run its build script with your keys.
        let p = sandboxed(Mode::Edits, "allow cargo test\n", "out");
        assert_eq!(p.decide(&command("cargo test", "network")), Decision::Allow, "inside the walls, the rule counts");
        assert_eq!(p.decide(&command("cargo test", "outside")), Decision::Ask);
        assert_eq!(p.how(&command("cargo test", "outside")), Run::Unsandboxed, "what it would be, once you say yes");
        let auto = sandboxed(Mode::Auto, "allow cargo test\n", "out-auto");
        assert!(matches!(auto.decide(&command("cargo test", "outside")), Decision::Deny(_)), "auto never asks, so it refuses");
        // Without a sandbox nothing is walled in, so rules let nothing run.
        let bare = Permissions::new(Mode::Ask, "allow cargo test\n").unwrap();
        assert_eq!(bare.decide(&bash("cargo test")), Decision::Ask);
        let bare_auto = Permissions::new(Mode::Auto, "allow cargo test\n").unwrap();
        assert!(matches!(bare_auto.decide(&bash("cargo test")), Decision::Deny(_)));
        // What you said yes to yourself still counts.
        let mut yours = sandboxed(Mode::Ask, "", "out-yours");
        yours.remember(&command("cargo test", "outside"));
        assert_eq!(yours.decide(&command("cargo test", "outside")), Decision::Allow);
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

    fn command(command: &str, access: &str) -> ToolCall {
        ToolCall { call_id: "c".into(), name: "bash".into(), arguments: json!({ "command": command, "access": access, "reason": null }).to_string() }
    }

    /// A project folder of its own, and permissions for it with a sandbox
    /// that says it's there.
    fn sandboxed(mode: Mode, rules: &str, name: &str) -> Permissions {
        let root = std::env::temp_dir().join(format!("rein-permissions-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let sandbox = crate::sandbox::Sandbox::assume(crate::sandbox::Places::new(&root, None, None));
        Permissions::new(mode, rules).unwrap().with_sandbox(std::sync::Arc::new(sandbox))
    }

    #[test]
    fn in_the_sandbox_the_mode_decides_by_what_a_command_needs() {
        use Decision::{Allow, Ask};
        let deny = Decision::Deny(String::new());
        // look, write, network, outside
        let table = [
            (Mode::ReadOnly, [Allow, deny.clone(), deny.clone(), deny.clone()]),
            (Mode::Ask, [Allow, Ask, Ask, Ask]),
            (Mode::Edits, [Allow, Allow, Ask, Ask]),
            (Mode::Auto, [Allow, Allow, deny.clone(), deny.clone()]),
            (Mode::Trust, [Allow, Allow, Allow, Allow]),
        ];
        for (mode, expected) in table {
            let p = sandboxed(mode, "", "table");
            for (access, expected) in ["look", "write", "network", "outside"].iter().zip(expected) {
                let decided = match p.decide(&command("cargo test", access)) {
                    Decision::Deny(_) => Decision::Deny(String::new()),
                    other => other,
                };
                assert_eq!(decided, expected, "{mode} with {access} access");
            }
        }
        // What the splitter can't take apart is held in by the walls too.
        assert_eq!(sandboxed(Mode::Ask, "", "loose").decide(&command("ls $HOME | head", "look")), Decision::Allow);
        // A command that says nothing about access only looks.
        assert_eq!(sandboxed(Mode::Ask, "", "unsaid").decide(&bash("cargo test")), Decision::Allow);
        assert_eq!(sandboxed(Mode::Ask, "", "unsaid").how(&bash("cargo test")), Run::Sandboxed(Access::Look.walls().unwrap()));
    }

    #[test]
    fn rules_still_count_in_the_sandbox() {
        let p = sandboxed(Mode::Edits, "deny git push\nask git commit\nallow cargo fetch\n", "rules");
        assert!(matches!(p.decide(&command("git push", "look")), Decision::Deny(_)));
        // You asked to be asked: the walls don't change that, even where
        // the splitter gives up.
        assert_eq!(p.decide(&command("git commit -m x", "write")), Decision::Ask);
        assert_eq!(p.decide(&command("echo $X && git commit -m x", "write")), Decision::Ask);
        // An allow rule spares asking about the network, inside the walls.
        assert_eq!(p.decide(&command("cargo fetch", "network")), Decision::Allow);
        let auto = sandboxed(Mode::Auto, "ask git commit\nallow cargo fetch\n", "auto-rules");
        assert!(matches!(auto.decide(&command("git commit -m x", "write")), Decision::Deny(_)), "auto never asks");
        assert_eq!(auto.decide(&command("cargo fetch", "network")), Decision::Allow);
        // Read-only changes nothing, whatever a rule allows.
        let read_only = sandboxed(Mode::ReadOnly, "allow cargo fetch\n", "read-only-rules");
        assert!(matches!(read_only.decide(&command("cargo fetch", "network")), Decision::Deny(_)));
    }

    #[test]
    fn always_remembers_the_access_too() {
        let mut p = sandboxed(Mode::Ask, "", "remember");
        p.remember(&command("cargo fetch", "network"));
        assert_eq!(p.decide(&command("cargo fetch", "network")), Decision::Allow);
        assert_eq!(p.decide(&command("cargo fetch", "write")), Decision::Allow);
        assert_eq!(p.decide(&command("cargo fetch", "outside")), Decision::Ask, "yes to the network isn't yes to everything");
        // Without a sandbox, access is only words: yes is yes.
        let mut unsandboxed = Permissions::new(Mode::Ask, "").unwrap();
        unsandboxed.remember(&command("cargo fetch", "look"));
        assert_eq!(unsandboxed.decide(&command("cargo fetch", "outside")), Decision::Allow);
    }

    #[test]
    fn how_a_command_runs() {
        let walls = |access: Access| Run::Sandboxed(access.walls().unwrap());
        let edits = sandboxed(Mode::Edits, "", "how");
        assert_eq!(edits.how(&command("cargo test", "write")), walls(Access::Write));
        assert_eq!(edits.how(&command("cargo fetch", "network")), walls(Access::Network));
        assert_eq!(edits.how(&command("docker ps", "outside")), Run::Unsandboxed);
        assert_eq!(sandboxed(Mode::Trust, "", "how-trust").how(&command("ls", "look")), Run::Unsandboxed);
        // No sandbox here: what's allowed runs as it always did.
        assert_eq!(Permissions::new(Mode::Ask, "").unwrap().how(&command("ls", "look")), Run::Unsandboxed);
        assert_eq!(edits.how(&file("write")), Run::Unsandboxed);
    }

    #[test]
    fn what_runs_code_later_asks_even_in_edits_mode() {
        let change = |path: &str| ToolCall { call_id: "c".into(), name: "write".into(), arguments: json!({ "path": path, "content": "allow rm" }).to_string() };
        let mut edits = sandboxed(Mode::Edits, "", "protected");
        assert_eq!(edits.decide(&change("src/main.rs")), Decision::Allow);
        for path in [".rein/permissions", ".git/hooks/pre-commit", ".git", ".vscode/tasks.json", ".REIN/permissions"] {
            assert_eq!(edits.decide(&change(path)), Decision::Ask, "{path}");
        }
        // "Always" for file changes isn't for these.
        edits.remember(&change("src/main.rs"));
        assert_eq!(edits.decide(&change(".rein/permissions")), Decision::Ask);
        assert!(matches!(sandboxed(Mode::Auto, "", "protected-auto").decide(&change(".rein/permissions")), Decision::Deny(_)));
        assert_eq!(sandboxed(Mode::Trust, "", "protected-trust").decide(&change(".rein/permissions")), Decision::Allow);
    }

    #[test]
    fn a_command_that_doesnt_read_is_refused_with_why() {
        let p = sandboxed(Mode::Edits, "", "bad");
        let Decision::Deny(why) = p.decide(&command("ls", "everything")) else { panic!("allowed a call that doesn't read") };
        assert!(why.starts_with("error: bad arguments") && why.contains("everything"), "{why}");
    }

    #[test]
    fn a_misspelt_mode_names_them_all() {
        assert_eq!("yolo".parse::<Mode>().unwrap_err(), "no mode called yolo; use ask, edits, auto, read-only or trust");
    }

    #[test]
    fn bad_rules_are_reported_with_their_line() {
        assert_eq!(Permissions::new(Mode::Ask, "allow ls\npermit ls").unwrap_err(), "line 2: start with allow, ask or deny");
        assert_eq!(Permissions::new(Mode::Ask, "allow ls && rm").unwrap_err(), "line 1: give one plain command, like `cargo test`");
    }
}
