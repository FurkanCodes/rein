//! Running one hook: its command, given what happened as JSON on its
//! input, in a process group of its own, which goes with it if it takes
//! too long or the turn stops. With no terminal: what it says comes back
//! to rein, never to the screen or a tab's title.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;

use super::config::{Command, From};
use crate::sandbox::{self, Sandbox, Walls};

/// How a hook runs: outside the sandbox, as your commands do in trust, or
/// in its walls, as they do in the other modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum How {
    Outside,
    Walled(Walls),
}

/// What a hook did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ran {
    /// Its exit code: `None` if a signal stopped it, or it didn't end.
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    /// Why it didn't run, or didn't end: it took too long, or wouldn't start.
    pub(crate) failed: Option<String>,
    pub(crate) took: Duration,
}

/// Runs `command`, set `from`, in `root`, with `input` on its input, as
/// `how` says, for at most `timeout`.
pub(crate) async fn run(command: &Command, from: &From, input: &[u8], root: &Path, sandbox: &Sandbox, how: How, timeout: Duration) -> Ran {
    let began = Instant::now();
    // Where the sandbox can't keep a command from what runs code later,
    // that's noted now and put back once it's done, as for a command.
    let guard = match how {
        How::Walled(walls) => match sandbox.guard(walls) {
            Ok(guard) => guard,
            Err(why) => return Ran { failed: Some(why), took: began.elapsed(), ..Ran::default() },
        },
        How::Outside => None,
    };
    let mut ran = match start(command, from, root, sandbox, how) {
        Ok(child) => finish(child, input, timeout).await,
        Err(why) => Ran { failed: Some(why), ..Ran::default() },
    };
    drop(guard.map(sandbox::Guard::finish));
    ran.took = began.elapsed();
    ran
}

fn start(command: &Command, from: &From, root: &Path, sandbox: &Sandbox, how: How) -> Result<tokio::process::Child, String> {
    let shell = if command.bash { "bash" } else { "sh" };
    let mut process = match how {
        How::Outside => {
            let mut process = match &command.args {
                Some(args) => {
                    let mut process = tokio::process::Command::new(&command.line);
                    process.args(args);
                    process
                }
                None => {
                    let mut process = tokio::process::Command::new(shell);
                    process.arg("-c").arg(&command.line);
                    process
                }
            };
            // Your whole environment, as a command outside the sandbox
            // gets it.
            process.envs(sandbox::secrets::all().iter().map(|(name, value)| (name, value)));
            process
        }
        How::Walled(walls) => {
            let line = match &command.args {
                Some(args) => format!("exec {}", std::iter::once(&command.line).chain(args).map(|word| quote(word)).collect::<Vec<_>>().join(" ")),
                None if command.bash => format!("exec bash -c {}", quote(&command.line)),
                None => command.line.clone(),
            };
            sandbox.wrap(&line, walls)?
        }
    };
    // As Claude Code gives its hooks: the project, and a plugin's folder.
    process.env("CLAUDE_PROJECT_DIR", root);
    if let From::Plugin { root: plugin, .. } = from {
        process.env("CLAUDE_PLUGIN_ROOT", plugin);
    }
    process.current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    // A group of its own, so stopping it stops what it started too; and
    // no terminal, so nothing it prints reaches the screen.
    #[cfg(unix)]
    process.process_group(0);
    process.spawn().map_err(|e| format!("it didn't start: {e}"))
}

async fn finish(mut child: tokio::process::Child, input: &[u8], timeout: Duration) -> Ran {
    let group = sandbox::Group::new(child.id());
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let feed = async {
        // One that doesn't read what it's given is let be.
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(input).await;
        }
    };
    let run = async { tokio::join!(feed, crate::tools::read_capped(stdout), crate::tools::read_capped(stderr), child.wait()) };
    // Given up on, or the turn stopped: `group` is dropped, and with it
    // everything the hook started.
    let Ok(((), (out, _), (err, _), status)) = tokio::time::timeout(timeout, run).await else {
        return Ran { failed: Some(format!("it took longer than {}s and was stopped", timeout.as_secs())), ..Ran::default() };
    };
    group.disarm();
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    match status {
        Ok(status) => Ran { code: status.code(), stdout: text(out), stderr: text(err), ..Ran::default() },
        Err(e) => Ran { failed: Some(format!("it failed to run: {e}")), ..Ran::default() },
    }
}

/// `word` quoted for `sh`, as it is.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(line: &str) -> Command {
        Command { line: line.into(), args: None, bash: false }
    }

    async fn outside(line: &str, input: &str, timeout: Duration) -> Ran {
        let root = std::env::temp_dir();
        let sandbox = Sandbox::missing(sandbox::Places::default(), sandbox::Missing::NotHere);
        run(&command(line), &From::Rein(root.clone()), input.as_bytes(), &root, &sandbox, How::Outside, timeout).await
    }

    #[tokio::test]
    async fn a_hook_reads_what_happened_and_says_back() {
        let ran = outside(r#"read line; echo "got $line"; echo oops >&2; exit 3"#, "{\"a\":1}\n", Duration::from_secs(5)).await;
        assert_eq!((ran.code, ran.stdout.as_str(), ran.stderr.as_str(), ran.failed), (Some(3), "got {\"a\":1}\n", "oops\n", None));
        let ignores = outside("echo fine", &"x".repeat(1_000_000), Duration::from_secs(5)).await;
        assert_eq!(ignores.stdout, "fine\n", "one that doesn't read its input isn't held up by it");
    }

    #[tokio::test]
    async fn a_hook_that_takes_too_long_is_stopped_with_what_it_started() {
        let marker = std::env::temp_dir().join(format!("rein-hook-slow-{}", std::process::id()));
        let line = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let ran = outside(&line, "", Duration::from_millis(300)).await;
        assert!(ran.failed.as_deref().is_some_and(|why| why.contains("took longer")), "{ran:?}");
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "what it started went with it");
    }

    #[tokio::test]
    async fn one_given_as_a_program_and_its_arguments_runs_with_no_shell() {
        let root = std::env::temp_dir();
        let sandbox = Sandbox::missing(sandbox::Places::default(), sandbox::Missing::NotHere);
        let exec = Command { line: "printf".into(), args: Some(vec!["%s|".into(), "$HOME".into(), "a b".into()]), bash: false };
        let ran = run(&exec, &From::Rein(root.clone()), b"", &root, &sandbox, How::Outside, Duration::from_secs(5)).await;
        assert_eq!(ran.stdout, "$HOME|a b|", "nothing in them is the shell's");
    }
}
