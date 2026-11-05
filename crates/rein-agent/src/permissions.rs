//! What the model may do without asking. A mode sets the broad policy,
//! the sandbox, when you turn it on, decides much of it (what runs inside
//! the walls needs no asking), and rules refine it for specific commands:
//! yours, in rein's folder, for every project, and a project's own in
//! `.rein/permissions`:
//!
//! ```text
//! # one rule per line
//! allow cargo test
//! allow git status
//! ask   git commit
//! deny  git push
//! ```
//!
//! Deny beats ask beats allow. A command only runs without asking by an
//! allow rule if every part of it starts with the rule's words: `allow
//! cargo test` doesn't cover `cargo test && rm -rf target`. Anything the
//! splitter can't take apart, like `$(...)`, redirects or globs, no allow
//! rule covers. An ask or deny rule is looked for harder: its command
//! anywhere in the line, by its name past any path (`/bin/rm`, after `sudo`
//! or `xargs`, inside `sh -c '…'`), and its other words after it in order,
//! wherever they are (`ask git push --force` finds `git push origin -f`
//! too: see [`Rule::finds`]).
//!
//! Rules aren't a sandbox: a script that deletes files itself, or a
//! command spelt to hide from them, gets past. They decide when rein asks
//! you; the sandbox decides what a command can reach.
//!
//! A project's file comes with the project, which may be anyone's, so its
//! allow rules only spare asking about a command that stays in the
//! sandbox. Outside it a command has your keys: `allow cargo test` in a
//! cloned repo would otherwise run its build script with them. Only you
//! say yes to that, and your own rules are you saying it.

use rein_llm::ToolCall;
use serde::Deserialize;

use crate::sandbox::{self, Access, Run};
use crate::tools::{BashCall, ERROR, ToolName};

/// What runs without asking. Whether commands run in walls is the
/// sandbox's say ([`Sandboxing`]), which is yours to turn on or off in any
/// mode, as in Codex and Claude Code: what each mode does with commands
/// depends on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Run everything without asking, except what ask rules name
    /// (deleting, from the rules you start with) and what deny rules
    /// block. Where rein starts.
    #[default]
    Trust,
    /// Ask before file changes, and before commands that do more than look
    /// in the sandbox (all of them, without it).
    Ask,
    /// File changes inside the project are fine, and so are commands that
    /// stay in the sandbox; going past it asks, and without it every
    /// command does.
    Edits,
    /// Never ask: what would ask is refused. In the sandbox that's what
    /// would go past it; without it, every command runs. For leaving rein
    /// to work on its own.
    Auto,
    /// Look, never change: commands only look (in the sandbox, or after
    /// asking you without it), and file changes are refused.
    ReadOnly,
}

/// Whether commands run in walls: as you chose (`/sandbox`, `sandbox =`
/// in the settings), and as this computer can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandboxing {
    /// On: commands run in the walls their access asks for.
    On,
    /// Off, as you chose: commands run as you'd run them, and the mode
    /// alone decides what asks.
    Off,
    /// On, but there's no sandbox here, or it didn't start: what would
    /// have run in it asks instead, and auto refuses it. Never run
    /// unconfined when it was meant to be confined.
    Missing,
}

impl Mode {
    pub const ALL: [Mode; 5] = [Mode::Trust, Mode::Ask, Mode::Edits, Mode::Auto, Mode::ReadOnly];

    /// The one after this, for Shift-Tab: down from trust, one less at a
    /// time, and back round.
    pub fn next(self) -> Mode {
        let at = Mode::ALL.iter().position(|&mode| mode == self).unwrap_or(0);
        Mode::ALL[(at + 1) % Mode::ALL.len()]
    }

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

/// What a rule does with the commands it's about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Allow,
    Ask,
    Deny,
}

impl Kind {
    /// Its word, as a rules file has it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Allow => "allow",
            Kind::Ask => "ask",
            Kind::Deny => "deny",
        }
    }
}

/// Whose a rule is, which decides how far an allow rule reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    /// Yours, in rein's folder: for every project.
    Yours,
    /// The project's own, in its `.rein/permissions`: it may be anyone's.
    Project,
}

/// What your rules file starts with: deleting what may not come back asks
/// first, in every mode, trust among them. Each line is yours to remove.
pub const FIRST_RULES: &str = "\
# Your rules, for every project: one a line, allow, ask or deny, then a
# command. Deny beats ask beats allow. An ask or deny rule finds its
# command wherever it is in a line (after sudo or xargs, or as /bin/rm),
# and its other words after it: `ask git push --force` covers
# `git push origin --force` too.
#
# These ask before deleting what may not come back. Take a line out to
# stop being asked about it, or add your own; /permissions does both.

# Files
ask rm
ask rmdir
ask unlink
ask shred
ask rimraf
ask find -delete
ask rsync --delete
ask rsync --remove-source-files

# Work and history git can't give back
ask git clean
ask git reset --hard
ask git checkout --
ask git checkout .
ask git restore
ask git stash drop
ask git stash clear
ask git branch -D
ask git push --force
ask git push -f
ask git push --delete
ask git push -d
ask git worktree remove

# Containers, clusters, infrastructure and databases
ask docker rmi
ask docker system prune
ask docker container prune
ask docker image prune
ask docker volume prune
ask kubectl delete
ask terraform destroy
ask terraform apply -destroy
ask dropdb
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    kind: Kind,
    /// The command and the words that follow it: `["cargo", "test"]`.
    words: Vec<String>,
    whose: Whose,
}

impl Rule {
    /// One rule, as a line of a rules file says it: `ask git push`.
    pub fn parse(line: &str, whose: Whose) -> Result<Rule, String> {
        let line = line.trim();
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let kind = match word {
            "allow" => Kind::Allow,
            "ask" => Kind::Ask,
            "deny" => Kind::Deny,
            _ => return Err("start with allow, ask or deny".into()),
        };
        match split_command(rest).as_deref() {
            Some([one]) => Ok(Rule { kind, words: one.clone(), whose }),
            _ => Err("give one plain command, like `cargo test`".into()),
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn whose(&self) -> Whose {
        self.whose
    }

    /// The same rule, whoever's it is.
    pub fn says_the_same(&self, other: &Rule) -> bool {
        self.kind == other.kind && self.words == other.words
    }

    /// For an allow rule: `command` starts with its words, whole words
    /// only, so `allow ls` covers `ls -la` but not `lsof`, nor `sudo ls`.
    fn starts(&self, command: &[String]) -> bool {
        command.starts_with(&self.words)
    }

    /// For an ask or deny rule, looking harder, as it should err towards
    /// finding: its command anywhere in `command`, by its name past any
    /// path and in any letters' case (macOS runs `/BIN/RM` as `rm`), so
    /// after `sudo`, `xargs` or `find -exec` too; then each of its other
    /// words after that, in order, wherever they are. A long flag covers
    /// its longer forms (`--force` covers `--force-with-lease`), and a
    /// short one the clusters it's in (`-f` is in `-fu`). What `sh -c` and
    /// `eval` are given to run is looked through too.
    fn finds(&self, command: &[String]) -> bool {
        let words = run_words(command);
        let Some((name, rest)) = self.words.split_first() else { return false };
        (0..words.len()).filter(|&at| is_named(&words[at], name)).any(|at| {
            let mut from = at + 1;
            rest.iter().all(|wanted| match words[from..].iter().position(|word| covers(wanted, word)) {
                Some(found) => {
                    from += found + 1;
                    true
                }
                None => false,
            })
        })
    }
}

/// `ask git push`, as a rules file would have it.
impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let words: Vec<String> = self.words.iter().map(|word| quoted(word)).collect();
        write!(f, "{} {}", self.kind.as_str(), words.join(" "))
    }
}

/// `word` as the splitter would read it back: in single quotes when it
/// holds anything but plain letters, digits and a few marks.
fn quoted(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c)) { word.to_owned() } else { format!("'{}'", word.replace('\'', "")) }
}

/// Whether `word` runs the command `name`: it, or a path to it, whatever
/// the letters' case.
fn is_named(word: &str, name: &str) -> bool {
    let base = word.rsplit('/').next().unwrap_or(word);
    word.eq_ignore_ascii_case(name) || base.eq_ignore_ascii_case(name)
}

/// Whether `word` says what a rule's `wanted` word does: the same word, a
/// longer form of a long flag (`--force-with-lease`, `--delete=x`), or a
/// cluster of short flags with it in (`-fu` for `-f`).
fn covers(wanted: &str, word: &str) -> bool {
    if wanted == word {
        return true;
    }
    if wanted.starts_with("--") {
        return word.strip_prefix(wanted).is_some_and(|more| more.starts_with(['-', '=']));
    }
    match (short_flags(wanted), short_flags(word)) {
        (Some(letter), Some(cluster)) if letter.len() == 1 => cluster.contains(letter),
        _ => false,
    }
}

/// The letters of `-abc`, a cluster of short flags; `None` for anything
/// else (`--long`, `-5`, a plain word).
fn short_flags(word: &str) -> Option<&str> {
    word.strip_prefix('-').filter(|letters| !letters.is_empty() && letters.chars().all(|c| c.is_ascii_alphabetic()))
}

/// `command`'s words, with what a shell is given to run (`sh -c '…'`,
/// `bash -lc "…"`, `eval '…'`) taken apart into words too.
fn run_words(command: &[String]) -> Vec<String> {
    let mut words = Vec::new();
    let mut runs_next = false;
    for word in command {
        if runs_next {
            words.extend(loose_words(word));
        } else {
            words.push(word.clone());
        }
        runs_next = word == "eval" || short_flags(word).is_some_and(|letters| letters.contains('c'));
    }
    words
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
    /// Whether you want commands in it: off, unless you turn it on.
    sandbox_on: bool,
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
const AUTO_NO_SANDBOX: &str = "rein is in auto mode, where nothing asks, and the user turned the sandbox on, but there's \
                               none here to hold commands in, so no command runs. Tell the user what you need to run.";
const AUTO_PROTECTED: &str = "rein is in auto mode, where nothing asks, and this file runs code later or holds rein's rules: \
                              changing it takes the user's approval. Leave it, or tell the user what to change.";
const AUTO_MCP: &str = "rein is in auto mode, where nothing asks, and this MCP tool isn't one the user's own mcp.json allows \
                        (\"allow\": [...]). Do without it, or tell the user what you need it for.";
const READ_ONLY_CHANGES: &str = "File changes are turned off: rein is in read-only mode.";
const READ_ONLY_COMMANDS: &str = "rein is in read-only mode: commands run with look access only, and change nothing.";

/// The rules in `text`, a file of them, each `whose`. Blank lines and `#`
/// comments are skipped.
pub fn parse_rules(text: &str, whose: Whose) -> Result<Vec<Rule>, String> {
    let lines = text.lines().enumerate().map(|(n, line)| (n, line.trim())).filter(|(_, line)| !line.is_empty() && !line.starts_with('#'));
    lines.map(|(n, line)| Rule::parse(line, whose).map_err(|e| format!("line {}: {e}", n + 1))).collect()
}

impl Permissions {
    /// The project's rules, in the format shown at the top of this file;
    /// yours come with [`Permissions::set_yours`]. The sandbox is off
    /// until [`Permissions::set_sandbox`] turns it on, and there's none to
    /// turn on until [`Permissions::with_sandbox`] gives one.
    pub fn new(mode: Mode, rules: &str) -> Result<Self, String> {
        Ok(Permissions { mode, rules: parse_rules(rules, Whose::Project)?, ..Default::default() })
    }

    /// Your rules, from `text`, in place of those you had.
    pub fn set_yours(&mut self, text: &str) -> Result<(), String> {
        let yours = parse_rules(text, Whose::Yours)?;
        self.rules.retain(|rule| rule.whose != Whose::Yours);
        self.rules.splice(0..0, yours);
        Ok(())
    }

    /// Every rule: yours, then the project's.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Decides with `sandbox`: what it can hold in, and what it protects.
    pub fn with_sandbox(mut self, sandbox: sandbox::Shared) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn sandbox(&self) -> &sandbox::Sandbox {
        &self.sandbox
    }

    /// Commands in the sandbox from now on, or not: yours to choose, in
    /// any mode.
    pub fn set_sandbox(&mut self, on: bool) {
        self.sandbox_on = on;
    }

    /// Whether commands run in walls now.
    pub fn sandboxing(&self) -> Sandboxing {
        match (self.sandbox_on, self.sandbox.available()) {
            (false, _) => Sandboxing::Off,
            (true, true) => Sandboxing::On,
            (true, false) => Sandboxing::Missing,
        }
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

    pub fn rule_count(&self, whose: Whose) -> usize {
        self.rules.iter().filter(|rule| rule.whose == whose).count()
    }

    pub fn decide(&self, call: &ToolCall) -> Decision {
        match ToolName::of(call) {
            // `read`, `grep` and `glob` can only look, and only inside the
            // project; `plan` only writes a checklist on screen; `memory`
            // writes notes in rein's own folder, never the project, and each
            // save is a step you see; `background` reads or stops what was
            // already allowed to run.
            Some(tool) if tool.only_looks() || matches!(tool, ToolName::Plan | ToolName::Memory | ToolName::Background) => Decision::Allow,
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
    /// word. So it asks, but for what you allowed yourself; read-only too,
    /// where turned off, codegraph's lookups were lost to a mode meant for
    /// looking. Auto, which never asks, refuses it instead.
    fn decide_tool(&self, name: &str) -> Decision {
        let allowed = self.allowed_tools.iter().chain(&self.approved_tools).any(|allowed| allowed == name);
        match self.mode {
            Mode::Trust => Decision::Allow,
            _ if allowed => Decision::Allow,
            Mode::Auto => Decision::Deny(AUTO_MCP.into()),
            Mode::Ask | Mode::Edits | Mode::ReadOnly => Decision::Ask,
        }
    }

    /// How `call` runs once it's allowed: with the sandbox on, a command
    /// in it, within the walls its access asks for, unless it asked to run
    /// outside; in every mode, trust too. rein's own tools run as they
    /// always do.
    pub fn how(&self, call: &ToolCall) -> Run {
        match BashCall::of(call) {
            Some(c) if self.sandboxing() == Sandboxing::On => c.access().walls().map_or(Run::Unsandboxed, Run::Sandboxed),
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

    /// The ask rule that has `call` asked about, if one does.
    pub fn asked_by(&self, call: &ToolCall) -> Option<&Rule> {
        BashCall::of(call).and_then(|c| self.found(Kind::Ask, c.command()))
    }

    /// The first rule of `kind` that finds a command in `line`. When the
    /// splitter gives up, it looks through the line's words instead, cut at
    /// anything that can start a command and with quotes taken off, so a
    /// deny rule still can't be dodged by adding a `$` somewhere or gluing
    /// the command to a `;`. Ask rules are looked for the same way: the
    /// sandbox, or trust, would otherwise run what you asked to be asked
    /// about.
    fn found(&self, kind: Kind, line: &str) -> Option<&Rule> {
        let candidates = split_command(line).unwrap_or_else(|| vec![loose_words(line)]);
        self.rules.iter().filter(|r| r.kind == kind).find(|r| candidates.iter().any(|c| r.finds(c)))
    }

    fn decide_command(&self, line: &str, access: Access) -> Decision {
        let parts = split_command(line);
        // Deny first.
        if let Some(rule) = self.found(Kind::Deny, line) {
            return Decision::Deny(format!("Blocked by a rule: `{rule}`."));
        }
        let asked = self.found(Kind::Ask, line).is_some();
        let sandboxing = self.sandboxing();
        let walled = sandboxing == Sandboxing::On;
        // What you said yes to for the rest of the conversation: exactly
        // these commands, with as much access, or with any where there are
        // no walls to hold a command to what it said it needs.
        let said_yes = |part: &Vec<String>| self.approved.iter().any(|(words, granted)| words == part && (*granted >= access || !walled));
        let you_said_yes = parts.as_ref().is_some_and(|parts| parts.iter().all(said_yes));
        // A project's rules come with it, and it may be anyone's: they only
        // spare you asking about what stays in the walls. Outside them a
        // command gets your keys, so only you say yes to that, and your own
        // rules count wherever it runs.
        let in_walls = walled && access != Access::Outside;
        let allows = |part: &Vec<String>| self.rules.iter().any(|r| r.kind == Kind::Allow && (in_walls || r.whose == Whose::Yours) && r.starts(part));
        let allowed = parts.as_ref().is_some_and(|parts| parts.iter().all(|p| allows(p) || said_yes(p)));
        // Meant for walls that aren't here: it asks, never runs unwalled.
        let unwalled = sandboxing == Sandboxing::Missing && access != Access::Outside && !allowed;
        if self.mode == Mode::Trust {
            // What rein couldn't take apart, it can't promise no deny rule
            // covers: it asks, rather than trust a guess. Ask rules are
            // only looked for in it, as they're about mistakes, not about
            // hiding.
            let denies = self.rules.iter().any(|r| r.kind == Kind::Deny);
            return if (parts.is_none() && denies) || (asked && !you_said_yes) || unwalled { Decision::Ask } else { Decision::Allow };
        }
        // What runs without asking: inside the walls, by the mode; without
        // them, as you chose, only in auto, which never asks.
        let free = match sandboxing {
            Sandboxing::On => match access {
                Access::Look => true,
                Access::Write => matches!(self.mode, Mode::Edits | Mode::Auto),
                Access::Network | Access::Outside => false,
            },
            Sandboxing::Off => self.mode == Mode::Auto,
            Sandboxing::Missing => false,
        };
        match self.mode {
            // Whatever the walls: it says it would change something.
            Mode::ReadOnly if access > Access::Look => Decision::Deny(READ_ONLY_COMMANDS.into()),
            Mode::Auto if asked => Decision::Deny(AUTO_ASK_RULE.into()),
            // You asked to be asked, unless you've said yes to just this.
            _ if asked && !you_said_yes => Decision::Ask,
            _ if free || allowed => Decision::Allow,
            Mode::Auto if sandboxing == Sandboxing::Missing => Decision::Deny(AUTO_NO_SANDBOX.into()),
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

/// The words of a line rein couldn't take apart, for ask and deny rules to
/// look through: split at spaces and at everything that can start or end a
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
    /// asks, but for what your own mcp.json allows, in read-only too; a
    /// project's rules never allow one, and auto, which never asks,
    /// refuses it.
    #[test]
    fn an_mcp_tool_asks_but_for_what_you_allowed_and_auto_refuses_it() {
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
        assert_eq!((p.decide(&issue), p.decide(&merge)), (Decision::Allow, Decision::Ask), "read-only asks, not refuses");
        p.remember(&merge);
        assert_eq!(p.decide(&merge), Decision::Allow, "and a yes for always holds there too");
        p.forget_approvals();
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
        // Without walls nothing is walled in, so a project's rules let
        // nothing run: with the sandbox off, or on with none here.
        let bare = Permissions::new(Mode::Ask, "allow cargo test\n").unwrap();
        assert_eq!(bare.decide(&bash("cargo test")), Decision::Ask);
        assert_eq!(missing(Mode::Ask, "allow cargo test\n").decide(&bash("cargo test")), Decision::Ask);
        assert!(matches!(missing(Mode::Auto, "allow cargo test\n").decide(&bash("cargo test")), Decision::Deny(_)));
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
        assert!(matches!(read_only.decide(&command("cargo test", "write")), Decision::Deny(_)));
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
        let mut permissions = Permissions::new(mode, rules).unwrap().with_sandbox(std::sync::Arc::new(sandbox));
        permissions.set_sandbox(true);
        permissions
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

    /// Permissions with the sandbox turned on, where there's none.
    fn missing(mode: Mode, rules: &str) -> Permissions {
        let mut permissions = Permissions::new(mode, rules).unwrap();
        permissions.set_sandbox(true);
        permissions
    }

    /// Each mode with `permissions` made for it, decided for a command
    /// with look, write, network and outside access, against `table`.
    fn decides(table: &[(Mode, [Decision; 4])], permissions: impl Fn(Mode) -> Permissions, walls: &str) {
        for (mode, expected) in table {
            let p = permissions(*mode);
            for (access, expected) in ["look", "write", "network", "outside"].iter().zip(expected) {
                let decided = match p.decide(&command("cargo test", access)) {
                    Decision::Deny(_) => Decision::Deny(String::new()),
                    other => other,
                };
                assert_eq!(&decided, expected, "{mode} with {access} access, {walls}");
                assert_eq!(p.how(&command("cargo test", access)), Run::Unsandboxed, "{mode} with {access} access, {walls}");
            }
        }
    }

    #[test]
    fn with_the_sandbox_off_the_mode_alone_decides() {
        use Decision::{Allow, Ask};
        let deny = Decision::Deny(String::new());
        // look, write, network, outside: what it says it needs counts only
        // in read-only, which refuses what says it would change things.
        let table = [
            (Mode::ReadOnly, [Ask, deny.clone(), deny.clone(), deny.clone()]),
            (Mode::Ask, [Ask, Ask, Ask, Ask]),
            (Mode::Edits, [Ask, Ask, Ask, Ask]),
            (Mode::Auto, [Allow, Allow, Allow, Allow]),
            (Mode::Trust, [Allow, Allow, Allow, Allow]),
        ];
        decides(&table, |mode| Permissions::new(mode, "").unwrap(), "sandbox off");
        // Off with a sandbox here is off all the same.
        decides(
            &table,
            |mode| {
                let mut p = sandboxed(mode, "", "off");
                p.set_sandbox(false);
                p
            },
            "sandbox off, one here",
        );
        // Auto, which never asks, still refuses what an ask rule names.
        let auto = Permissions::new(Mode::Auto, "ask rm").unwrap();
        assert!(matches!(auto.decide(&bash("rm -rf target")), Decision::Deny(_)));
        assert_eq!(auto.decide(&bash("cargo test")), Decision::Allow);
    }

    #[test]
    fn with_the_sandbox_on_but_none_here_what_it_would_hold_asks() {
        use Decision::{Allow, Ask};
        let deny = Decision::Deny(String::new());
        // Never run unconfined when it was meant to be confined: what would
        // have run in the walls asks, and auto, which never asks, refuses.
        let table = [
            (Mode::ReadOnly, [Ask, deny.clone(), deny.clone(), deny.clone()]),
            (Mode::Ask, [Ask, Ask, Ask, Ask]),
            (Mode::Edits, [Ask, Ask, Ask, Ask]),
            (Mode::Auto, [deny.clone(), deny.clone(), deny.clone(), deny.clone()]),
            (Mode::Trust, [Ask, Ask, Ask, Allow]),
        ];
        decides(&table, |mode| missing(mode, ""), "sandbox on, none here");
        // What you allowed yourself runs, as it would without the walls.
        let mut trust = missing(Mode::Trust, "");
        trust.remember(&command("cargo test", "write"));
        assert_eq!(trust.decide(&command("cargo test", "write")), Allow);
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
        // On is on in every mode, trust too.
        assert_eq!(sandboxed(Mode::Trust, "", "how-trust").how(&command("ls", "look")), walls(Access::Look));
        // Off, or none here: what's allowed runs as it always did.
        assert_eq!(Permissions::new(Mode::Ask, "").unwrap().how(&command("ls", "look")), Run::Unsandboxed);
        assert_eq!(missing(Mode::Ask, "").how(&command("ls", "look")), Run::Unsandboxed);
        let mut off = sandboxed(Mode::Edits, "", "how-off");
        off.set_sandbox(false);
        assert_eq!(off.how(&command("cargo test", "write")), Run::Unsandboxed);
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

    /// Trust with the rules you start with: what deletes asks, wherever
    /// in the line and however it's spelt; the rest runs.
    fn trusting() -> Permissions {
        let mut p = Permissions::new(Mode::Trust, "").unwrap();
        p.set_yours(FIRST_RULES).unwrap();
        p
    }

    #[test]
    fn the_rules_you_start_with_ask_before_deleting_in_trust_mode() {
        let p = trusting();
        for deletes in [
            "rm -rf target",
            "/bin/rm x",
            "\\rm x",
            "sudo rm -rf /tmp/x",
            "find . -name '*.o' -delete",
            "find . -name x -exec rm {} ;",
            "ls | xargs rm",
            "cargo build && rm -rf dist",
            "bash -lc 'cd x && rm -rf y'",
            "sh -c \"rmdir old\"",
            "npx rimraf dist",
            "git clean -fdx",
            "git -C ../other clean -f",
            "git reset --hard HEAD~1",
            "git checkout -- src/main.rs",
            "git checkout .",
            "git restore src/lib.rs",
            "git stash drop",
            "git branch -D feature",
            "git push origin main --force",
            "git push --force-with-lease",
            "git push -fu origin main",
            "git push origin --delete old",
            "git rm src/old.rs",
            "rsync -a --delete-after src/ dst/",
            "docker system prune -af",
            "docker volume rm data",
            "kubectl -n prod delete pod web",
            "terraform destroy",
            "rm -rf target 2>/dev/null",
            "echo $(rm x)",
            "RM -RF x",
        ] {
            assert_eq!(p.decide(&bash(deletes)), Decision::Ask, "{deletes}");
        }
        for runs in [
            "ls -la",
            "cargo test 2>&1 | tail -20",
            "git status",
            "git push origin main",
            "git checkout main",
            "git commit -m 'drop the rm call'",
            "git log --oneline -- src/clean.rs",
            "git restorer",
            "rsync -a src/ dst/",
            "lsof -i",
            "echo $HOME",
            "docker ps",
        ] {
            assert_eq!(p.decide(&bash(runs)), Decision::Allow, "{runs}");
        }
    }

    #[test]
    fn an_ask_rules_other_words_come_after_its_command_in_order() {
        let rule = Rule::parse("ask git push --force", Whose::Yours).unwrap();
        let words = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert!(rule.finds(&words("git -C x push origin --force")));
        assert!(!rule.finds(&words("git --force push")), "the flag before the subcommand");
        assert!(!rule.finds(&words("push --force git")), "the words before the command");
    }

    #[test]
    fn yes_always_to_a_command_an_ask_rule_names_is_kept_for_exactly_it() {
        let mut p = trusting();
        p.remember(&bash("rm -rf target"));
        assert_eq!(p.decide(&bash("rm -rf target")), Decision::Allow, "you said always");
        assert_eq!(p.decide(&command("rm -rf target", "outside")), Decision::Allow, "trust has no walls: access is only words");
        assert_eq!(p.decide(&bash("rm -rf src")), Decision::Ask, "only that one");
        p.forget_approvals();
        assert_eq!(p.decide(&bash("rm -rf target")), Decision::Ask);
        // The same in a mode with walls, for as much access.
        let mut edits = sandboxed(Mode::Edits, "", "always-ask-rule");
        edits.set_yours("ask rm").unwrap();
        assert_eq!(edits.decide(&command("rm -rf target", "write")), Decision::Ask, "the walls don't stop asking");
        edits.remember(&command("rm -rf target", "write"));
        assert_eq!(edits.decide(&command("rm -rf target", "write")), Decision::Allow);
        assert_eq!(edits.decide(&command("rm -rf target", "outside")), Decision::Ask);
        // Auto never asks: it's refused.
        let mut auto = sandboxed(Mode::Auto, "", "auto-ask-rule");
        auto.set_yours("ask rm").unwrap();
        assert_eq!(auto.decide(&command("rm -rf target", "write")), Decision::Deny(AUTO_ASK_RULE.into()));
    }

    #[test]
    fn your_allow_rules_count_where_a_projects_dont() {
        // Without a sandbox, a project's rule spares nothing, yours does.
        let mut bare = Permissions::new(Mode::Ask, "allow cargo test\n").unwrap();
        assert_eq!(bare.decide(&bash("cargo test")), Decision::Ask);
        bare.set_yours("allow cargo test").unwrap();
        assert_eq!(bare.decide(&bash("cargo test")), Decision::Allow);
        assert_eq!(bare.decide(&bash("sudo cargo test")), Decision::Ask, "allow is whole words from the start");
        // Outside the walls, the same.
        let mut walled = sandboxed(Mode::Edits, "allow cargo fetch\n", "yours-outside");
        assert_eq!(walled.decide(&command("cargo fetch", "outside")), Decision::Ask);
        walled.set_yours("allow cargo fetch").unwrap();
        assert_eq!(walled.decide(&command("cargo fetch", "outside")), Decision::Allow);
        // A project's ask rule still asks about what yours allows.
        let mut both = sandboxed(Mode::Edits, "ask cargo fetch\n", "yours-and-theirs");
        both.set_yours("allow cargo fetch").unwrap();
        assert_eq!(both.decide(&command("cargo fetch", "network")), Decision::Ask);
    }

    #[test]
    fn your_rules_are_replaced_and_the_projects_kept() {
        let mut p = Permissions::new(Mode::Trust, "deny curl\n").unwrap();
        p.set_yours("ask rm\nallow cargo test").unwrap();
        p.set_yours("ask git commit -m 'a b'").unwrap();
        let shown: Vec<(String, Whose)> = p.rules().iter().map(|rule| (rule.to_string(), rule.whose())).collect();
        assert_eq!(shown, [("ask git commit -m 'a b'".to_owned(), Whose::Yours), ("deny curl".to_owned(), Whose::Project)]);
        assert_eq!((p.rule_count(Whose::Yours), p.rule_count(Whose::Project)), (1, 1));
        // As shown, it reads back the same.
        let again = Rule::parse(&p.rules()[0].to_string(), Whose::Yours).unwrap();
        assert!(again.says_the_same(&p.rules()[0]));
        assert_eq!(p.set_yours("ask rm\npermit x").unwrap_err(), "line 2: start with allow, ask or deny");
    }

    #[test]
    fn shift_tab_goes_round_every_mode_from_trust() {
        let mut mode = Mode::default();
        let mut seen = Vec::new();
        for _ in Mode::ALL {
            seen.push(mode);
            mode = mode.next();
        }
        assert_eq!(seen, [Mode::Trust, Mode::Ask, Mode::Edits, Mode::Auto, Mode::ReadOnly]);
        assert_eq!(mode, Mode::Trust, "and back round");
    }

    #[test]
    fn a_misspelt_mode_names_them_all() {
        assert_eq!("yolo".parse::<Mode>().unwrap_err(), "no mode called yolo; use trust, ask, edits, auto or read-only");
    }

    #[test]
    fn bad_rules_are_reported_with_their_line() {
        assert_eq!(Permissions::new(Mode::Ask, "allow ls\npermit ls").unwrap_err(), "line 2: start with allow, ask or deny");
        assert_eq!(Permissions::new(Mode::Ask, "allow ls && rm").unwrap_err(), "line 1: give one plain command, like `cargo test`");
    }
}
