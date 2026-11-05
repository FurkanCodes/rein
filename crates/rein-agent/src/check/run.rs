//! Running a check: as a command of the model's would run, in the sandbox
//! when it's on, within write walls (a build writes its `target`), in a
//! process group of its own that goes with it if it takes too long or the
//! turn stops.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use super::{BUDGET, Check, Problem, Reads, parse};
use crate::sandbox::{self, Run};

/// The most of each of a check's outputs read: cargo's JSON says a lot of
/// each warning.
const MOST: usize = 8 << 20;

/// What cargo says while another build holds the project's: the check
/// waits, and that isn't the check being slow.
const CARGO_WAITS: &str = "Blocking waiting for file lock";

/// How a check went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// It ran: the errors it found, how long it took, and those errors in
    /// lines, for you to read whole.
    Done { problems: Vec<Problem>, took: Duration, printed: String },
    /// It took longer than [`BUDGET`], and was stopped.
    Slow,
    /// It waited for the build, which another command held, and was stopped.
    Busy,
    /// It wouldn't start.
    Couldnt(String),
}

/// Runs `check` in the project at `root`, as `how` says, in `sandbox`,
/// for at most [`BUDGET`].
pub(crate) async fn run(check: &Check, root: &Path, sandbox: &sandbox::Shared, how: Run) -> Ran {
    within(check, root, sandbox, how, BUDGET).await
}

async fn within(check: &Check, root: &Path, sandbox: &sandbox::Shared, how: Run, budget: Duration) -> Ran {
    let began = Instant::now();
    // Where the sandbox can't keep a command from what runs code later,
    // that's noted now and put back once it's done: before the command, so
    // it's dropped after its group is stopped.
    let guard = match how {
        Run::Sandboxed(walls) => match sandbox.guard(walls) {
            Ok(guard) => guard,
            Err(why) => return Ran::Couldnt(why),
        },
        Run::Unsandboxed => None,
    };
    let mut command = match how {
        Run::Sandboxed(walls) => match sandbox.wrap(&check.command, walls) {
            Ok(command) => command,
            Err(why) => return Ran::Couldnt(why),
        },
        Run::Unsandboxed => {
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(&check.command).envs(sandbox::secrets::all().iter().map(|(name, value)| (name, value)));
            command
        }
    };
    command.envs([("CARGO_TERM_COLOR", "never"), ("NO_COLOR", "1"), ("FORCE_COLOR", "0")]);
    command.current_dir(root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Ran::Couldnt(format!("`{}` didn't start: {e}", check.command)),
    };
    // Dropped, as when the turn is stopped or the check takes too long, it
    // stops everything the check started.
    let group = sandbox::Group::new(child.id());
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let ran = tokio::time::timeout(budget, async { tokio::join!(read(&mut stdout, &mut out), read(&mut stderr, &mut err), child.wait()) }).await;
    let Ok(((), (), status)) = ran else {
        drop(group);
        drop(guard);
        // What it printed until it was stopped says why it was slow.
        let waited = check.reads == Reads::Cargo && String::from_utf8_lossy(&err).contains(CARGO_WAITS);
        return if waited { Ran::Busy } else { Ran::Slow };
    };
    group.disarm();
    if let Some(guard) = guard {
        guard.finish();
    }
    let code = match status {
        Ok(status) => status.code(),
        Err(e) => return Ran::Couldnt(format!("`{}` failed to run: {e}", check.command)),
    };
    let (out, err) = (String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
    let problems = parse::problems(check.reads, root, &out, &err, code);
    let printed = if problems.is_empty() { "No errors.".to_owned() } else { problems.iter().map(Problem::shown).collect::<Vec<_>>().join("\n") };
    Ran::Done { problems, took: began.elapsed(), printed }
}

/// Reads all of `pipe` into `into`, keeping the first [`MOST`] bytes.
async fn read(pipe: &mut Option<impl tokio::io::AsyncRead + Unpin>, into: &mut Vec<u8>) {
    let Some(pipe) = pipe else { return };
    let mut buf = [0u8; 8192];
    while let Ok(n @ 1..) = pipe.read(&mut buf).await {
        let room = MOST.saturating_sub(into.len()).min(n);
        into.extend_from_slice(&buf[..room]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rein-check-run-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn a_command_of_yours_passes_or_fails_by_its_exit_code() {
        let root = project("yours");
        let sandbox = sandbox::Shared::default();
        let passed = run(&Check::yours("echo fine"), &root, &sandbox, Run::Unsandboxed).await;
        assert!(matches!(&passed, Ran::Done { problems, .. } if problems.is_empty()), "{passed:?}");
        let failed = run(&Check::yours("echo starting; echo broken >&2; exit 3"), &root, &sandbox, Run::Unsandboxed).await;
        assert!(matches!(&failed, Ran::Done { problems, .. } if problems == &[Problem::Failed { code: Some(3), tail: "broken".into() }]), "{failed:?}");
        let missing = run(&Check::yours("no-such-program-anywhere"), &root, &sandbox, Run::Unsandboxed).await;
        assert!(matches!(&missing, Ran::Done { problems, .. } if matches!(problems[..], [Problem::Failed { code: Some(127), .. }])), "{missing:?}");
    }

    #[tokio::test]
    async fn a_check_too_slow_is_stopped_and_one_waiting_for_the_build_is_told_apart() {
        let root = project("slow");
        let sandbox = sandbox::Shared::default();
        let slow = within(&Check::yours("sleep 47.41"), &root, &sandbox, Run::Unsandboxed, Duration::from_millis(300)).await;
        assert_eq!(slow, Ran::Slow);
        let waits = Check { command: "echo '    Blocking waiting for file lock on build directory' >&2; sleep 47.43".into(), reads: Reads::Cargo, found_from: None };
        assert_eq!(within(&waits, &root, &sandbox, Run::Unsandboxed, Duration::from_millis(300)).await, Ran::Busy);
        let ps = std::process::Command::new("ps").args(["-axo", "stat=,command="]).output().unwrap();
        let left = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .any(|line| line.trim_start().split_once(' ').is_some_and(|(stat, rest)| !stat.starts_with('Z') && rest.trim().starts_with("sleep 47.4")));
        assert!(!left, "a check stopped takes what it started with it");
    }

    #[tokio::test]
    async fn a_real_cargo_check_says_where_an_edit_broke_the_build() {
        if !super::super::on_path("cargo") {
            return;
        }
        let root = project("cargo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn size() -> usize {\n    64\n}\n").unwrap();
        let check = super::super::detect(&root).unwrap();
        let sandbox = sandbox::Shared::default();
        let clean = run(&check, &root, &sandbox, Run::Unsandboxed).await;
        assert!(matches!(&clean, Ran::Done { problems, .. } if problems.is_empty()), "{clean:?}");
        std::fs::write(root.join("src/lib.rs"), "pub fn size() -> usize {\n    \"64\"\n}\n").unwrap();
        let Ran::Done { problems, .. } = run(&check, &root, &sandbox, Run::Unsandboxed).await else { panic!("it didn't run") };
        assert_eq!(problems, [Problem::At { file: "src/lib.rs".into(), line: 2, column: 5, code: Some("E0308".into()), message: "mismatched types".into() }]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
