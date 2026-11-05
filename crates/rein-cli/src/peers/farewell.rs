//! What a sub-agent tells the rein that started it when it's ended before
//! its task is done: closing its tab hangs up on it (SIGHUP), and it went
//! without a word, its parent waiting for its result for good.
//!
//! What the task's turn has done is kept here as it goes, the steps a line
//! each, the files it changed and the answer it's writing, so the one
//! signal handler can send it before rein exits. It goes over the socket,
//! never the terminal, which is gone by then: Ghostty hangs up again every
//! 10 ms and waits for rein to exit, its window frozen meanwhile, so it's
//! sent once, within [`WITHIN`].

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::{Body, Ended, Report};

/// The most steps told: the latest, after a line saying how many came
/// before them.
const STEPS: usize = 30;
/// The longest a step's line is.
const STEP_CHARS: usize = 160;
/// How long saying how it ended may take.
const WITHIN: Duration = Duration::from_secs(2);

/// What the turn has done so far, and who's owed its result.
struct Progress {
    /// Where the reins are, and this one's workspace: set as it joins.
    folder: Option<PathBuf>,
    workspace: String,
    /// The tasks this turn owes a result: where each goes, and its id.
    owed: Vec<(String, String)>,
    started: Option<Instant>,
    steps: Vec<String>,
    /// Steps no longer kept, before the first of `steps`.
    earlier: usize,
    /// The step running now.
    on: Option<String>,
    files: Vec<String>,
    /// The answer being written, or the last one.
    words: String,
}

impl Progress {
    const fn new() -> Progress {
        Progress { folder: None, workspace: String::new(), owed: Vec::new(), started: None, steps: Vec::new(), earlier: 0, on: None, files: Vec::new(), words: String::new() }
    }

    fn step(&mut self, line: &str) {
        self.on = None;
        self.steps.push(one_line(line));
        if self.steps.len() > STEPS {
            self.steps.remove(0);
            self.earlier += 1;
        }
    }

    /// What the turn did so far, as `ended`, saying `summary`.
    fn report(&self, ended: Ended, summary: String) -> Report {
        let seconds = self.started.map_or(0, |started| started.elapsed().as_secs());
        Report { ended, summary, steps: self.steps.clone(), earlier: self.earlier, on: self.on.clone(), seconds, files: self.files.clone() }
    }

    /// The results owed, each to where it goes, saying the turn `ended`
    /// with what it did; none are owed after.
    fn results(&mut self, ended: Ended) -> Vec<(String, Body)> {
        let owed = std::mem::take(&mut self.owed);
        // It's gone: no one to ask for the rest.
        let report = self.report(ended, super::talk::capped(&self.words, None));
        owed.into_iter().map(|(to, id)| (to, Body::Result { id, report: report.clone() })).collect()
    }
}

/// `line` on one line, at most [`STEP_CHARS`] long.
fn one_line(line: &str) -> String {
    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > STEP_CHARS { format!("{}…", line.chars().take(STEP_CHARS - 1).collect::<String>()) } else { line }
}

static PROGRESS: Mutex<Progress> = Mutex::new(Progress::new());

fn progress() -> MutexGuard<'static, Progress> {
    PROGRESS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// This rein joined the others in `folder`, in `workspace`.
pub(crate) fn joined(folder: PathBuf, workspace: &str) {
    let mut progress = progress();
    progress.folder = Some(folder);
    workspace.clone_into(&mut progress.workspace);
}

/// A turn starts: nothing done yet. What's owed stays: a task is heard
/// just before the turn on it starts.
pub(crate) fn turn_started() {
    let mut progress = progress();
    progress.started = Some(Instant::now());
    progress.steps.clear();
    progress.earlier = 0;
    progress.on = None;
    progress.files.clear();
    progress.words.clear();
}

/// The turn owes `reply_to` the result of task `id`.
pub(crate) fn owe(reply_to: &str, id: &str) {
    progress().owed.push((reply_to.to_owned(), id.to_owned()));
}

/// The results owed went: nothing to say if it's ended now.
pub(crate) fn settled() {
    progress().owed.clear();
}

/// A step starts: `line` says what it is.
pub(crate) fn running(line: &str) {
    progress().on = Some(one_line(line));
}

/// A step ran: `line` says what it was and how it went.
pub(crate) fn stepped(line: &str) {
    progress().step(line);
}

/// The turn changed `path`.
pub(crate) fn changed(path: &str) {
    let mut progress = progress();
    if !progress.files.iter().any(|file| file == path) {
        progress.files.push(path.to_owned());
    }
}

/// A new answer starts, after a step or at the turn's start: the words
/// kept are its own from now on.
pub(crate) fn writing() {
    progress().words.clear();
}

/// More of the answer it's writing.
pub(crate) fn wrote(delta: &str) {
    progress().words.push_str(delta);
}

/// What the turn did, as `ended`, with its last answer as the summary,
/// for `me`, still there, to be asked for the rest of.
pub(crate) fn finished(ended: Ended, me: &str) -> Report {
    let progress = progress();
    progress.report(ended, super::talk::capped(&progress.words, Some(me)))
}

/// Tells each rein owed a result that this one `ended` before it was
/// done, with what it did: once, within [`WITHIN`], and never through the
/// terminal.
pub(crate) async fn say_how_it_ended(ended: Ended) {
    let (folder, workspace, results) = {
        let mut progress = progress();
        let Some(folder) = progress.folder.clone() else { return };
        (folder, progress.workspace.clone(), progress.results(ended))
    };
    if results.is_empty() {
        return;
    }
    let _ = tokio::time::timeout(WITHIN, async {
        let peers = super::list(&folder).await;
        for (to, body) in results {
            if let Ok(peer) = super::find(&peers, &to, &workspace) {
                let _ = super::ask(peer, body).await;
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_it_did_goes_to_each_rein_owed_once() {
        let mut progress = Progress::new();
        progress.owed = vec![("w/lead".into(), "t1".into())];
        for n in 1..=STEPS + 2 {
            progress.step(&format!("Run   step {n}\n · exit 0"));
        }
        progress.step(&"x".repeat(500));
        assert_eq!((progress.earlier, progress.steps.len()), (3, STEPS), "the latest are kept, and how many came before");
        assert_eq!(progress.steps[0], "Run step 4 · exit 0");
        assert_eq!(progress.steps.last().map(|line| line.chars().count()), Some(STEP_CHARS), "a long step is cut");
        progress.files = vec!["src/a.rs".into()];
        progress.words = "Now running the tests.".into();
        progress.on = Some("Run cargo test".into());
        let results = progress.results(Ended::Closed);
        let [(to, Body::Result { id, report })] = &results[..] else { panic!("{results:?}") };
        assert_eq!((to.as_str(), id.as_str(), report.ended, report.summary.as_str()), ("w/lead", "t1", Ended::Closed, "Now running the tests."));
        assert_eq!((report.step_count(), report.files.as_slice(), report.on.as_deref()), (STEPS + 3, &["src/a.rs".to_owned()][..], Some("Run cargo test")));
        assert!(progress.results(Ended::Closed).is_empty(), "said once");
        progress.step("Run cargo test · exit 0");
        assert_eq!(progress.on, None, "a step that finished isn't cut off");
    }
}
