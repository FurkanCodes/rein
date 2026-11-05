//! Errors after each edit: after a step that changed the project's files,
//! rein runs the project's own check (`cargo check`, `tsc --noEmit`) and
//! tells the model what its changes broke, at that step, not three steps
//! later when a build fails.
//!
//! Only what the turn broke is told: the errors there were before its
//! first change are its baseline, and errors are compared by file, code
//! and message, never by line, so an old one that moved isn't new. Errors
//! only, never warnings, at most [`SHOWN`]. A check that broke nothing
//! adds nothing to the conversation, and one whose errors are fixed says
//! so once.
//!
//! The project's own command, not a language server: `cargo check` on
//! rein itself answers in 0.3-0.7 s after an edit, needs nothing
//! installed, and says what the build says. Language servers in other
//! harnesses were late, stale or noisy, hung, or were left running.

mod parse;
mod run;

use std::path::Path;
use std::time::Duration;

pub use run::Ran;
pub(crate) use run::run;

/// How long a check may take before it's stopped, and checking pauses for
/// the conversation: a project whose check is that slow is better checked
/// by the model when it builds.
pub const BUDGET: Duration = Duration::from_secs(15);

/// The most errors told at once; the rest are counted.
pub const SHOWN: usize = 10;

/// How rein's note of a check starts: it's rein's, not the user's.
const NOTE: &str = "[rein checked the project with ";

/// `text` is rein's note of a check, not something the user said.
pub fn is_note(text: &str) -> bool {
    text.starts_with(NOTE)
}

/// What checks a project: a command, and how its output reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub command: String,
    pub reads: Reads,
    /// The project's file it was found from (`Cargo.toml`); `None` for
    /// yours.
    pub found_from: Option<&'static str>,
}

/// How a check's output is read into errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reads {
    /// `cargo … --message-format=json`: a JSON message a line.
    Cargo,
    /// `tsc --pretty false`: `a.ts(10,5): error TS2304: …`.
    Tsc,
    /// `go vet`: `./a.go:3:2: …`.
    Go,
    /// `mypy`: `a.py:3:5: error: … [code]`.
    Mypy,
    /// `pyright --outputjson`.
    Pyright,
    /// Anything else: it passed or failed, by its exit code.
    ExitCode,
}

impl Check {
    /// A command of yours: read as its program writes, where rein knows how
    /// it does, and by its exit code otherwise.
    pub fn yours(command: &str) -> Check {
        let words: Vec<&str> = command.split_whitespace().collect();
        let program = words.first().map(|word| word.rsplit('/').next().unwrap_or(word)).unwrap_or_default();
        let has = |flag: &str| words.contains(&flag);
        let reads = match program {
            "cargo" if has("--message-format=json") => Reads::Cargo,
            "tsc" => Reads::Tsc,
            "go" if matches!(words.get(1), Some(&("vet" | "build"))) => Reads::Go,
            "mypy" => Reads::Mypy,
            "pyright" if has("--outputjson") => Reads::Pyright,
            _ => Reads::ExitCode,
        };
        Check { command: command.trim().to_owned(), reads, found_from: None }
    }

    /// Its program, for saying what ran: `cargo check`, `tsc`.
    pub fn name(&self) -> String {
        let words: Vec<&str> = self.command.split_whitespace().take_while(|word| !word.starts_with('-')).collect();
        let program = words.first().map(|word| word.rsplit('/').next().unwrap_or(word)).unwrap_or_default();
        match words.get(1) {
            Some(sub) if matches!(program, "cargo" | "go" | "npm" | "pnpm" | "yarn" | "npx") => format!("{program} {sub}"),
            _ => program.to_owned(),
        }
    }
}

/// The check the files at `root` call for, as its own tools would run it:
/// `None` where rein doesn't know one, or its program isn't there.
pub fn detect(root: &Path) -> Option<Check> {
    let found = |file: &'static str, command: &str, reads: Reads| Some(Check { command: command.to_owned(), reads, found_from: Some(file) });
    if root.join("Cargo.toml").is_file() && on_path("cargo") {
        return found("Cargo.toml", "cargo check --workspace --all-targets --message-format=json", Reads::Cargo);
    }
    // The project's own tsc, as its version is the one it builds with.
    if root.join("tsconfig.json").is_file() && root.join("node_modules/.bin/tsc").is_file() {
        return found("tsconfig.json", "node_modules/.bin/tsc --noEmit --pretty false", Reads::Tsc);
    }
    if root.join("go.mod").is_file() && on_path("go") {
        return found("go.mod", "go vet ./...", Reads::Go);
    }
    let pyproject = std::fs::read_to_string(root.join("pyproject.toml")).unwrap_or_default();
    if (root.join("pyrightconfig.json").is_file() || pyproject.contains("[tool.pyright]")) && on_path("pyright") {
        let file = if root.join("pyrightconfig.json").is_file() { "pyrightconfig.json" } else { "pyproject.toml" };
        return found(file, "pyright --outputjson", Reads::Pyright);
    }
    if (root.join("mypy.ini").is_file() || pyproject.contains("[tool.mypy]")) && on_path("mypy") {
        let file = if root.join("mypy.ini").is_file() { "mypy.ini" } else { "pyproject.toml" };
        return found(file, "mypy --show-column-numbers --no-error-summary --no-color-output .", Reads::Mypy);
    }
    None
}

/// Whether `program` is in one of `$PATH`'s folders.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// One error a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// At a place in a file, as compilers say.
    At { file: String, line: u32, column: u32, code: Option<String>, message: String },
    /// The check failed without saying where: its exit code, and its
    /// last lines.
    Failed { code: Option<i32>, tail: String },
}

impl Problem {
    /// What it is, without where in its file: the same error moved by an
    /// edit above it is the same error.
    fn key(&self) -> (&str, Option<&str>, &str) {
        match self {
            Problem::At { file, code, message, .. } => (file, code.as_deref(), message),
            Problem::Failed { .. } => ("", Some("failed"), ""),
        }
    }

    /// `src/a.rs:12:5 error[E0308]: mismatched types`.
    pub fn shown(&self) -> String {
        match self {
            Problem::At { file, line, column, code, message } => {
                let code = code.as_ref().map(|code| format!("[{code}]")).unwrap_or_default();
                format!("{file}:{line}:{column} error{code}: {message}")
            }
            Problem::Failed { code: Some(code), tail } => format!("it failed (exit code {code}), and ended with:\n{tail}"),
            Problem::Failed { code: None, tail } => format!("a signal stopped it, and it ended with:\n{tail}"),
        }
    }
}

/// `now` less what was there in `before`, by what each is, as often as each
/// is there: what the changes brought in.
pub fn brought(before: &[Problem], now: &[Problem]) -> Vec<Problem> {
    let mut left: Vec<&Problem> = before.iter().collect();
    now.iter()
        .filter(|problem| match left.iter().position(|old| old.key() == problem.key()) {
            Some(at) => {
                left.remove(at);
                false
            }
            None => true,
        })
        .cloned()
        .collect()
}

/// What one check came to, after a step, for you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// What ran: `cargo check`.
    pub name: String,
    pub took: Duration,
    pub verdict: Verdict,
    /// The errors it brought in, the first [`SHOWN`].
    pub shown: Vec<String>,
}

/// What a check after a step said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing new broke.
    Clean,
    /// This many errors the turn's changes brought in.
    New(usize),
    /// The errors the turn brought in are gone.
    Fixed,
    /// It wasn't run: the build was busy with another.
    Busy,
}

/// The project's check in a conversation: what runs, if anything, whether
/// it's paused, and what this turn's checks know.
#[derive(Debug, Default)]
pub struct Checking {
    check: Option<Check>,
    /// It was too slow, or wouldn't start: off until it's turned on again.
    paused: bool,
    pub(crate) turn: Turn,
}

impl Checking {
    /// The check that runs from now on, or none; turned on, it isn't paused.
    pub fn set(&mut self, check: Option<Check>) {
        self.check = check;
        self.paused = false;
        self.turn = Turn::default();
    }

    /// The check, on or paused.
    pub fn check(&self) -> Option<&Check> {
        self.check.as_ref()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// The check that runs now: `None` when there's none, or it's paused.
    pub(crate) fn active(&self) -> Option<&Check> {
        self.check.as_ref().filter(|_| !self.paused)
    }

    pub(crate) fn pause(&mut self) {
        self.paused = true;
    }
}

/// What the turn's checks know.
#[derive(Debug, Default)]
pub struct Turn {
    /// The errors there were before the turn's first change; `None` until
    /// that's been looked at, or when it couldn't be.
    baseline: Option<Vec<Problem>>,
    /// Whether the baseline was tried: a check that failed to give one
    /// isn't run again in the turn.
    looked: bool,
    /// What the turn's changes brought in, as the last check said.
    brought: Vec<Problem>,
    /// The model was told, as it finished, of what's still broken.
    told_at_end: bool,
}

impl Turn {
    /// The turn needs its baseline before it changes anything.
    pub fn wants_baseline(&self) -> bool {
        !self.looked
    }

    /// There's a baseline to tell what's new by.
    pub fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    pub fn set_baseline(&mut self, baseline: Option<Vec<Problem>>) {
        self.looked = true;
        self.baseline = baseline;
    }

    /// What a check after a change found, `now`, which took `took`: what
    /// to show you, and what to tell the model, if anything. `None` when
    /// there's no baseline to tell what's new by.
    pub fn after(&mut self, check: &Check, now: &[Problem], took: Duration) -> Option<(Checked, Option<String>)> {
        let baseline = self.baseline.as_ref()?;
        let new = brought(baseline, now);
        let had = std::mem::replace(&mut self.brought, new.clone());
        let shown: Vec<String> = new.iter().take(SHOWN).map(Problem::shown).collect();
        let (verdict, note) = match (new.len(), had.len()) {
            (0, 0) => (Verdict::Clean, None),
            (0, _) => (Verdict::Fixed, Some(format!("{NOTE}`{}` after these changes: the errors your changes brought in are gone.]", check.name()))),
            (count, _) => {
                self.told_at_end = false;
                (Verdict::New(count), Some(format!("{NOTE}`{}` after these changes: {}.\n{}]", check.name(), errors(count), listed(&new))))
            }
        };
        Some((Checked { name: check.name(), took, verdict, shown }, note))
    }

    /// What to tell the model as it says it's done, once a turn: the
    /// errors its changes brought in that are still there.
    pub fn at_end(&mut self, check: &Check) -> Option<String> {
        if self.brought.is_empty() || std::mem::replace(&mut self.told_at_end, true) {
            return None;
        }
        let count = self.brought.len();
        Some(format!(
            "{NOTE}`{}`: {} your changes this turn brought in {} still there. Fix {}, or say why {} left:\n{}]",
            check.name(),
            errors(count),
            if count == 1 { "is" } else { "are" },
            if count == 1 { "it" } else { "them" },
            if count == 1 { "it's" } else { "they're" },
            listed(&self.brought)
        ))
    }
}

fn errors(count: usize) -> String {
    if count == 1 { "1 new error".to_owned() } else { format!("{count} new errors") }
}

/// `problems`, a line each, the first [`SHOWN`], and how many more.
fn listed(problems: &[Problem]) -> String {
    let mut lines: Vec<String> = problems.iter().take(SHOWN).map(Problem::shown).collect();
    if problems.len() > SHOWN {
        lines.push(format!("…and {} more", problems.len() - SHOWN));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(file: &str, line: u32, message: &str) -> Problem {
        Problem::At { file: file.into(), line, column: 1, code: Some("E0308".into()), message: message.into() }
    }

    #[test]
    fn whats_new_goes_by_what_an_error_is_not_where() {
        let before = [at("a.rs", 10, "mismatched types"), at("a.rs", 20, "mismatched types")];
        // The edit added lines above: both moved, and one more came.
        let now = [at("a.rs", 12, "mismatched types"), at("a.rs", 22, "mismatched types"), at("a.rs", 30, "mismatched types"), at("b.rs", 1, "unresolved import")];
        assert_eq!(brought(&before, &now), [at("a.rs", 30, "mismatched types"), at("b.rs", 1, "unresolved import")]);
        // Where it is doesn't make it old: another message in the same
        // file, or the same message in another file, is new.
        let now = [at("a.rs", 10, "cannot find value `x`"), at("b.rs", 20, "mismatched types")];
        assert_eq!(brought(&before, &now), now);
        let coded = |code: &str| Problem::At { file: "a.rs".into(), line: 10, column: 1, code: Some(code.into()), message: "mismatched types".into() };
        assert_eq!(brought(&[coded("E0308")], &[coded("E0599")]), [coded("E0599")], "another code is another error");
        let failed = |tail: &str| Problem::Failed { code: Some(1), tail: tail.into() };
        assert!(brought(&[failed("took 1.2s")], &[failed("took 3.4s")]).is_empty(), "still failing isn't new");
    }

    #[test]
    fn a_turn_tells_what_broke_then_that_its_fixed_then_nothing() {
        let check = Check::yours("cargo check --message-format=json");
        let mut turn = Turn::default();
        assert!(turn.wants_baseline());
        assert_eq!(turn.after(&check, &[], Duration::ZERO), None, "no baseline, nothing to tell by");
        turn.set_baseline(Some(vec![at("old.rs", 1, "old")]));
        let (checked, note) = turn.after(&check, &[at("old.rs", 2, "old")], Duration::ZERO).unwrap();
        assert_eq!((checked.verdict, note), (Verdict::Clean, None));
        let (checked, note) = turn.after(&check, &[at("old.rs", 2, "old"), at("a.rs", 3, "mismatched types")], Duration::ZERO).unwrap();
        assert_eq!(checked.verdict, Verdict::New(1));
        assert_eq!(note.unwrap(), "[rein checked the project with `cargo check` after these changes: 1 new error.\na.rs:3:1 error[E0308]: mismatched types]");
        let end = turn.at_end(&check).unwrap();
        assert!(is_note(&end) && end.contains("1 new error your changes this turn brought in is still there. Fix it"), "{end}");
        assert_eq!(turn.at_end(&check), None, "once a turn");
        let (checked, note) = turn.after(&check, &[at("old.rs", 2, "old")], Duration::ZERO).unwrap();
        assert_eq!(checked.verdict, Verdict::Fixed);
        assert!(note.unwrap().ends_with("the errors your changes brought in are gone.]"));
        let (checked, note) = turn.after(&check, &[], Duration::ZERO).unwrap();
        assert_eq!((checked.verdict, note), (Verdict::Clean, None), "said once");
        assert_eq!(turn.at_end(&check), None, "nothing left broken");
    }

    #[test]
    fn many_errors_are_counted_past_the_first_ten() {
        let check = Check::yours("tsc");
        let mut turn = Turn::default();
        turn.set_baseline(Some(Vec::new()));
        let many: Vec<Problem> = (1..=13).map(|n| at("a.ts", n, &format!("error {n}"))).collect();
        let (checked, note) = turn.after(&check, &many, Duration::ZERO).unwrap();
        assert_eq!((checked.verdict, checked.shown.len()), (Verdict::New(13), SHOWN));
        assert!(note.unwrap().ends_with("a.ts:10:1 error[E0308]: error 10\n…and 3 more]"));
    }

    #[test]
    fn yours_is_read_as_its_program_writes() {
        assert_eq!(Check::yours("cargo clippy --all-targets --message-format=json").reads, Reads::Cargo);
        assert_eq!(Check::yours("cargo clippy").reads, Reads::ExitCode, "its plain text isn't read");
        assert_eq!(Check::yours("./node_modules/.bin/tsc --noEmit").reads, Reads::Tsc);
        assert_eq!(Check::yours("go vet ./...").reads, Reads::Go);
        assert_eq!(Check::yours("make lint").reads, Reads::ExitCode);
        assert_eq!(Check::yours("cargo check --workspace --message-format=json").name(), "cargo check");
        assert_eq!(Check::yours("node_modules/.bin/tsc --noEmit --pretty false").name(), "tsc");
    }

    #[test]
    fn the_projects_files_say_which_check() {
        let root = std::env::temp_dir().join(format!("rein-check-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
        assert_eq!(detect(&root), None);
        std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(detect(&root), None, "no tsc of its own");
        std::fs::write(root.join("node_modules/.bin/tsc"), "").unwrap();
        assert_eq!(detect(&root).map(|check| (check.reads, check.found_from)), Some((Reads::Tsc, Some("tsconfig.json"))));
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        if on_path("cargo") {
            assert_eq!(detect(&root).map(|check| check.command), Some("cargo check --workspace --all-targets --message-format=json".to_owned()));
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
