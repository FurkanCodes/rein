//! Commands that go on while the model works: a dev server, a watcher, a
//! long test suite. Every command `bash` runs is followed here, so one can
//! go to the background part way through without starting again: asked
//! for (`background`), at its timeout, with Ctrl-B, or when it ends but
//! leaves something running (a server put behind `&`).
//!
//! What a command prints is kept in memory, its start and its latest. The
//! model reads what's new with the `background` tool, or waits for a line,
//! and is told when one ends, once: it never has to poll. A command stops
//! with the conversation, its whole process group with it, and is in its
//! walls for as long as it runs.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Notify;

use crate::redact::Secrets;
use crate::sandbox::{self, Run};

/// The most commands running in the background at once: past that, a new
/// one is refused with the list, so the model stops one itself. None goes
/// behind its back.
pub const MOST: usize = 8;

/// How long a command asked to stop has before it's killed.
const GRACE: Duration = Duration::from_secs(2);

/// The most ended commands kept, for their output.
const KEPT_ENDED: usize = 8;

/// How long a wait for a line lasts when the model doesn't say, and at most.
const WAIT: Duration = Duration::from_secs(30);
const MOST_WAIT: Duration = Duration::from_secs(600);

/// The most of a command's last lines a note of its end carries.
const NOTE_LINES: usize = 20;

/// How a note of a command's end starts: it's rein's, not the user's.
const NOTE: &str = "[rein: the background command ";

/// `text` is rein's note of a background command's end, not something the
/// user said.
pub fn is_note(text: &str) -> bool {
    text.starts_with(NOTE)
}

/// A command's name, as the model and you know it: `b1`, `b2`...
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id(usize);

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "b{}", self.0)
    }
}

impl std::str::FromStr for Id {
    type Err = ();

    fn from_str(text: &str) -> Result<Id, ()> {
        text.trim().strip_prefix('b').and_then(|n| n.parse().ok()).map(Id).ok_or(())
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this code.
    Code(i32),
    /// A signal stopped it.
    Signal,
    /// It was stopped, by the model or by you.
    Stopped(By),
}

/// Who stopped a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Model,
    You,
}

impl Exit {
    fn of(status: std::process::ExitStatus) -> Exit {
        status.code().map_or(Exit::Signal, Exit::Code)
    }

    /// The line `bash` ends a command's output with.
    pub fn line(self) -> String {
        match self {
            Exit::Code(code) => format!("[exit code {code}]"),
            Exit::Signal | Exit::Stopped(_) => crate::tools::SIGNAL_LINE.to_owned(),
        }
    }

    /// In a few words, for a line on screen.
    pub fn said(self) -> String {
        match self {
            Exit::Code(0) => "done".to_owned(),
            Exit::Code(code) => format!("exit {code}"),
            Exit::Signal => "stopped by a signal".to_owned(),
            Exit::Stopped(By::Model) => "stopped".to_owned(),
            Exit::Stopped(By::You) => "stopped by you".to_owned(),
        }
    }

    pub fn failed(self) -> bool {
        !matches!(self, Exit::Code(0) | Exit::Stopped(_))
    }
}

/// Where a command is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Life {
    Running,
    /// The shell exited, but what it started runs on, or holds its output.
    Left(Exit),
    Over(Exit),
}

mod output;
mod watch;

pub use output::plain;
use output::{Output, fit, text};
use watch::watch;

/// A command rein follows.
struct Job {
    id: Id,
    command: String,
    started: Instant,
    /// When it was over.
    ended: Option<Instant>,
    output: Output,
    /// How much of the output the model has been given.
    read: usize,
    life: Life,
    /// In the background: the model knows its name, and it's in view.
    background: bool,
    /// The model knows it ended.
    told: bool,
    /// It's being stopped.
    stopping: Option<By>,
    /// How it runs, for what its walls were.
    how: Run,
    /// What the sandbox put back when it ended.
    restored: Vec<String>,
    /// What reads its output and waits for it: stopped with it.
    task: Option<tokio::task::AbortHandle>,
    /// Its process group, killed when it's dropped while still running.
    /// Before `guard`, so what's guarded is put back after it's stopped.
    group: Option<sandbox::Group>,
    guard: Option<sandbox::Guard>,
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Job {
    fn running(&self) -> bool {
        !matches!(self.life, Life::Over(_))
    }
}

/// A command just started, for [`Jobs::follow`].
pub(crate) struct Started {
    pub child: tokio::process::Child,
    pub group: sandbox::Group,
    pub guard: Option<sandbox::Guard>,
    pub command: String,
    pub how: Run,
}

/// A command waited for in the foreground: stopped with its whole group
/// if this is dropped first, when the turn is (Esc), unless it's kept.
pub(crate) struct Foreground<'a> {
    jobs: &'a Jobs,
    id: Option<Id>,
}

impl<'a> Foreground<'a> {
    pub(crate) fn new(jobs: &'a Jobs, id: Id) -> Foreground<'a> {
        Foreground { jobs, id: Some(id) }
    }

    /// It's done with, or goes on in the background: not stopped here.
    pub(crate) fn keep(mut self) {
        self.id = None;
    }
}

impl Drop for Foreground<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.jobs.kill(id);
        }
    }
}

/// How a command waited for in the foreground came out.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// It's done: nothing of it runs.
    Over,
    /// The shell exited, but something it started runs on.
    Left,
}

/// One line about a command, for you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: String,
    pub command: String,
    /// How long it's been running, or ran.
    pub took: Duration,
    /// How it ended, if it has.
    pub ended: Option<Exit>,
    /// The last thing it printed, plain.
    pub last: Option<String>,
}

/// A command's end, for the model, and in a line for you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub id: String,
    pub command: String,
    pub exit: Exit,
    /// What the model is told.
    pub note: String,
}

#[derive(Default)]
struct State {
    jobs: Vec<Job>,
    /// The number the next command gets.
    next: usize,
    /// Goes up with every change you'd see: a command started, ended, or
    /// printed.
    version: u64,
}

/// The commands of one conversation's tools. Dropped, each is stopped,
/// with everything it started.
pub struct Jobs {
    state: Mutex<State>,
    /// Output came, or a command's life changed.
    changed: Notify,
    /// Ctrl-B: the command waited for in the foreground goes on in the
    /// background.
    detach: Notify,
    tasks: Mutex<tokio::task::JoinSet<()>>,
    /// The keys rein knows, hidden from what's shown of a command.
    secrets: Mutex<Secrets>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl Jobs {
    pub fn new() -> Arc<Jobs> {
        Arc::new(Jobs { state: Mutex::default(), changed: Notify::new(), detach: Notify::new(), tasks: Mutex::default(), secrets: Mutex::default() })
    }

    /// Hides `secrets` in what's shown of commands from now on.
    pub(crate) fn hide(&self, secrets: Secrets) {
        *lock(&self.secrets) = secrets;
    }

    fn hidden(&self, text: String) -> String {
        lock(&self.secrets).hide(text)
    }

    /// Follows `started`, in the foreground, until [`Jobs::to_background`]
    /// or [`Jobs::finish`]: what it prints is kept, and it's waited for.
    pub(crate) fn follow(self: &Arc<Self>, started: Started) -> Id {
        let Started { mut child, group, guard, command, how } = started;
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let mut state = lock(&self.state);
        state.next += 1;
        let id = Id(state.next);
        let task = lock(&self.tasks).spawn(watch(Arc::downgrade(self), id, child, stdout, stderr));
        state.jobs.push(Job {
            id,
            command,
            started: Instant::now(),
            ended: None,
            output: Output::default(),
            read: 0,
            life: Life::Running,
            background: false,
            told: false,
            stopping: None,
            how,
            restored: Vec::new(),
            task: Some(task),
            group: Some(group),
            guard,
        });
        drop(state);
        self.tidy();
        id
    }

    /// Forgets the tasks that are done, and the oldest ended commands past
    /// [`KEPT_ENDED`].
    fn tidy(&self) {
        let mut tasks = lock(&self.tasks);
        while tasks.try_join_next().is_some() {}
        drop(tasks);
        let mut state = lock(&self.state);
        let ended = state.jobs.iter().filter(|job| job.background && !job.running() && job.told).count();
        let mut extra = ended.saturating_sub(KEPT_ENDED);
        state.jobs.retain(|job| {
            let drop = extra > 0 && job.background && !job.running() && job.told;
            extra -= usize::from(drop);
            !drop
        });
    }

    /// Does `work` on the command `id`, telling no one: for what only
    /// rein keeps track of.
    fn with<T>(&self, id: Id, work: impl FnOnce(&mut Job) -> T) -> Option<T> {
        let mut state = lock(&self.state);
        let job = state.jobs.iter_mut().find(|job| job.id == id)?;
        Some(work(job))
    }

    /// Does `work` on the command `id`, a change the waits and the screen
    /// hear of: it printed, its life moved on.
    fn change<T>(&self, id: Id, work: impl FnOnce(&mut Job) -> T) -> Option<T> {
        let mut state = lock(&self.state);
        let job = state.jobs.iter_mut().find(|job| job.id == id)?;
        let done = work(job);
        state.version += 1;
        drop(state);
        self.changed.notify_waiters();
        Some(done)
    }

    /// The commands running in the background.
    pub fn running(&self) -> usize {
        lock(&self.state).jobs.iter().filter(|job| job.background && job.running()).count()
    }

    /// Whether a command that may change the project's files runs in the
    /// background: `/undo` keeps looking for what it changes.
    pub(crate) fn writing(&self) -> bool {
        lock(&self.state).jobs.iter().any(|job| job.background && job.running() && !matches!(job.how, Run::Sandboxed(walls) if !walls.write))
    }

    /// A number that changes whenever something you'd see does.
    pub fn version(&self) -> u64 {
        lock(&self.state).version
    }

    /// Waits until the command `id`, waited for in the foreground, is over
    /// or has left something running.
    pub(crate) async fn settled(&self, id: Id) -> Settled {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.with(id, |job| job.life) {
                Some(Life::Running) => {}
                Some(Life::Left(_)) => return Settled::Left,
                Some(Life::Over(_)) | None => return Settled::Over,
            }
            changed.await;
        }
    }

    /// Ctrl-B was pressed: the command waited for in the foreground, if
    /// one is, goes on in the background.
    pub fn detach(&self) {
        self.detach.notify_waiters();
    }

    /// Waits for Ctrl-B.
    pub(crate) async fn detached(&self) {
        self.detach.notified().await;
    }

    /// The command `id` goes on in the background: what it printed so far,
    /// cut to `budget`, is what the model is given now, with how its shell
    /// ended, if it has.
    pub(crate) fn to_background(&self, id: Id, budget: usize) -> (String, Option<Exit>) {
        self.change(id, |job| {
            job.background = true;
            job.read = job.output.total;
            let exit = match job.life {
                Life::Left(exit) | Life::Over(exit) => Some(exit),
                Life::Running => None,
            };
            (fit(&job.output.all(), budget), exit)
        })
        .unwrap_or_default()
    }

    /// The foreground command `id`, over, is done with: what it printed,
    /// cut to `budget`, how it ended, and what the sandbox put back.
    pub(crate) fn finish(&self, id: Id, budget: usize) -> Option<(String, Exit, Vec<String>)> {
        let mut state = lock(&self.state);
        let at = state.jobs.iter().position(|job| job.id == id)?;
        let mut job = state.jobs.remove(at);
        drop(state);
        let exit = match job.life {
            Life::Over(exit) | Life::Left(exit) => exit,
            Life::Running => Exit::Signal,
        };
        Some((fit(&job.output.all(), budget), exit, std::mem::take(&mut job.restored)))
    }

    /// Stops the command `id` at once, with its whole group, and forgets it.
    pub(crate) fn kill(&self, id: Id) {
        let mut state = lock(&self.state);
        let job = state.jobs.iter().position(|job| job.id == id).map(|at| state.jobs.remove(at));
        state.version += 1;
        drop(state);
        drop(job);
        self.changed.notify_waiters();
    }

    /// The background command the model calls `id`.
    fn find(&self, id: &str) -> Result<Id, String> {
        let state = lock(&self.state);
        let names = || state.jobs.iter().filter(|job| job.background).map(|job| job.id.to_string()).collect::<Vec<_>>().join(", ");
        let found = id.parse::<Id>().ok().filter(|id| state.jobs.iter().any(|job| job.background && job.id == *id));
        found.ok_or_else(|| match names() {
            names if names.is_empty() => format!("there's no background command called {id}; none is running"),
            names => format!("there's no background command called {id}; there are {names}"),
        })
    }

    /// What `background` does: reads what the command printed since the
    /// model last read, or waits for a line first, or stops it.
    pub(crate) async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: Args = crate::tools::parse(arguments)?;
        let id = self.find(&args.id)?;
        match args.action {
            Action::Read => self.read(id, args.until.as_deref(), args.seconds).await,
            Action::Stop => self.stop(id, By::Model).await,
        }
    }

    async fn read(&self, id: Id, until: Option<&str>, seconds: Option<u64>) -> Result<String, String> {
        let pattern = match until.map(str::trim).filter(|until| !until.is_empty()) {
            Some(until) => Some(regex_lite::Regex::new(until).map_err(|e| format!("until isn't a regular expression rein can read: {e}"))?),
            None => None,
        };
        let wait = seconds.map_or(WAIT, |s| Duration::from_secs(s).clamp(Duration::from_secs(1), MOST_WAIT));
        let deadline = tokio::time::Instant::now() + wait;
        let found = loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (new, running) = self.with(id, |job| (text(job.output.since(job.read)), job.running())).unwrap_or_default();
            let Some(pattern) = &pattern else { break None };
            // Only whole lines: a line half printed may match only once it's all there.
            let whole = new.rfind('\n').map_or("", |end| &new[..end]);
            if let Some(line) = whole.lines().find(|line| pattern.is_match(&plain(line))) {
                break Some(Ok(plain(line)));
            }
            if !running {
                break Some(Err(Waited::Ended));
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                break Some(Err(Waited::TimedOut));
            }
        };
        let Some((said, new)) = self.with(id, |job| {
            let new = text(job.output.since(job.read));
            job.read = job.output.total;
            job.told |= !job.running();
            (state_of(job), new)
        }) else {
            return Err(format!("{id} is gone"));
        };
        let waited = match (found, until) {
            (None, _) | (_, None) => String::new(),
            (Some(Ok(line)), _) => format!("A line matched: {line}\n"),
            (Some(Err(Waited::Ended)), Some(until)) => format!("It ended before a line matched {until}.\n"),
            (Some(Err(Waited::TimedOut)), Some(until)) => format!("No line matched {until} in {}s.\n", wait.as_secs()),
        };
        let new = if new.is_empty() { "Nothing new since you last read.".to_owned() } else { fit(&new, crate::tools::MAX_OUTPUT - 500) };
        Ok(format!("{waited}{said}\n{new}"))
    }

    /// Stops the command `id`, as `by` asked: everything in its group is
    /// asked to stop, and killed if it hasn't within [`GRACE`].
    pub(crate) async fn stop(&self, id: Id, by: By) -> Result<String, String> {
        let asked = self.with(id, |job| {
            if !job.running() {
                return false;
            }
            job.stopping = Some(by);
            job.group.as_ref().is_some_and(sandbox::Group::ask_to_stop)
        });
        if asked == Some(true) && tokio::time::timeout(GRACE, self.over(id)).await.is_err() {
            self.change(id, |job| job.group.take());
            let _ = tokio::time::timeout(GRACE, self.over(id)).await;
        }
        // Nothing of it answered: it's over all the same.
        self.change(id, |job| {
            if job.running() {
                job.group.take();
                job.life = Life::Over(Exit::Stopped(by));
                job.ended = Some(Instant::now());
                if let Some(task) = job.task.take() {
                    task.abort();
                }
            }
        });
        let new = self.with(id, |job| {
            let new = text(job.output.since(job.read));
            job.read = job.output.total;
            job.told |= by == By::Model;
            (state_of(job), new)
        });
        let Some((said, new)) = new else { return Err(format!("{id} is gone")) };
        Ok(if new.is_empty() { said } else { format!("{said}\nWhat it printed since you last read:\n{}", fit(&new, crate::tools::MAX_OUTPUT - 500)) })
    }

    /// Waits until the command `id` is over.
    async fn over(&self, id: Id) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.with(id, |job| job.running()).unwrap_or(false) {
                return;
            }
            changed.await;
        }
    }

    /// You stopped the background command `id`, from `/tasks`. The model
    /// is told with the next step.
    pub async fn stop_yours(&self, id: &str) -> Result<(), String> {
        let id = self.find(id)?;
        self.stop(id, By::You).await.map(|_| ())
    }

    /// The background commands that ended since the model was told, each
    /// told now: what it's told, with what they printed last.
    pub fn untold(&self) -> Vec<Ended> {
        let ended: Vec<Ended> = {
            let mut state = lock(&self.state);
            state
                .jobs
                .iter_mut()
                .filter(|job| job.background && !job.told)
                .filter_map(|job| {
                    let Life::Over(exit) = job.life else { return None };
                    job.told = true;
                    let new = text(job.output.since(job.read));
                    job.read = job.output.total;
                    let lines: Vec<&str> = new.lines().collect();
                    let last = lines[lines.len().saturating_sub(NOTE_LINES)..].join("\n");
                    let how = match exit {
                        Exit::Stopped(By::You) => "was stopped by the user".to_owned(),
                        exit => format!("ended: {}", exit.line()),
                    };
                    let mut note = format!("{NOTE}{} ({}) {how}.", job.id, job.command);
                    if last.is_empty() {
                        note.push_str(" It printed nothing new since you last read.");
                    } else {
                        let more = if lines.len() > NOTE_LINES { format!(" (the last {NOTE_LINES} lines; read it with background for more)") } else { String::new() };
                        note.push_str(&format!(" What it printed since you last read{more}:\n{}", fit(&last, 4000)));
                    }
                    if let Run::Sandboxed(walls) = job.how
                        && exit.failed()
                    {
                        note.push('\n');
                        note.push_str(walls.note());
                    }
                    if !job.restored.is_empty() {
                        note.push_str(&format!(
                            "\n[The sandbox here can't keep a command from files that run code later, so rein put them back as they were: {}.]",
                            job.restored.join(", ")
                        ));
                    }
                    note.push(']');
                    Some(Ended { id: job.id.to_string(), command: job.command.clone(), exit, note })
                })
                .collect()
        };
        self.tidy();
        ended.into_iter().map(|ended| Ended { note: self.hidden(ended.note), ..ended }).collect()
    }

    /// Waits until a background command has ended that the model hasn't
    /// been told of: not one you stopped, which it hears of with your next
    /// message.
    pub async fn ended(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let ended = |job: &Job| matches!(job.life, Life::Over(exit) if exit != Exit::Stopped(By::You));
            if lock(&self.state).jobs.iter().any(|job| job.background && !job.told && ended(job)) {
                return;
            }
            changed.await;
        }
    }

    /// The background commands, the oldest first, for you.
    pub fn list(&self) -> Vec<Listed> {
        let state = lock(&self.state);
        let listed: Vec<Listed> = state
            .jobs
            .iter()
            .filter(|job| job.background)
            .map(|job| Listed {
                id: job.id.to_string(),
                command: job.command.clone(),
                took: job.ended.unwrap_or_else(Instant::now).saturating_duration_since(job.started),
                ended: match job.life {
                    Life::Over(exit) => Some(exit),
                    _ => None,
                },
                last: job.output.last_line(),
            })
            .collect();
        drop(state);
        listed.into_iter().map(|listed| Listed { last: listed.last.map(|last| self.hidden(last)), command: self.hidden(listed.command), ..listed }).collect()
    }

    /// What each background command printed, kept, for the viewer
    /// (Ctrl-O): its title and its output.
    pub fn outputs(&self) -> Vec<(String, String)> {
        let state = lock(&self.state);
        let all: Vec<(String, String)> = state
            .jobs
            .iter()
            .filter(|job| job.background)
            .map(|job| {
                let ended = match job.life {
                    Life::Over(exit) => exit.said(),
                    _ => "running".to_owned(),
                };
                (format!("{}  {} · {ended}", job.id, job.command), job.output.all())
            })
            .collect();
        drop(state);
        all.into_iter().map(|(title, output)| (self.hidden(title), self.hidden(output))).collect()
    }

    /// Stops every command, each asked first and killed if it hasn't
    /// stopped within [`GRACE`]: what was running in the background.
    pub async fn stop_all(&self) -> Vec<Listed> {
        let running: Vec<Listed> = self.list().into_iter().filter(|listed| listed.ended.is_none()).collect();
        let ids: Vec<Id> = lock(&self.state).jobs.iter().filter(|job| job.running()).map(|job| job.id).collect();
        for id in &ids {
            self.with(*id, |job| job.group.as_ref().map(sandbox::Group::ask_to_stop));
        }
        let all = async {
            for id in &ids {
                self.over(*id).await;
            }
        };
        let _ = tokio::time::timeout(GRACE, all).await;
        let jobs = std::mem::take(&mut lock(&self.state).jobs);
        drop(jobs);
        running
    }

    /// Whether anything in the group of `id` still runs.
    fn group_alive(&self, id: Id) -> bool {
        self.with(id, |job| job.group.as_ref().is_some_and(sandbox::Group::alive)).unwrap_or(false)
    }
}

/// What the model said `background` should do.
#[derive(Deserialize)]
struct Args {
    id: String,
    action: Action,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    seconds: Option<u64>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Action {
    Read,
    Stop,
}

/// Why a wait for a line ended without one.
enum Waited {
    Ended,
    TimedOut,
}

/// How a command is doing, in a line for the model.
fn state_of(job: &Job) -> String {
    match job.life {
        Life::Running | Life::Left(_) => format!("{} ({}) is still running.", job.id, job.command),
        Life::Over(Exit::Stopped(By::Model)) => format!("{} ({}) was stopped.", job.id, job.command),
        Life::Over(Exit::Stopped(By::You)) => format!("{} ({}) was stopped by the user.", job.id, job.command),
        Life::Over(exit) => format!("{} ({}) ended: {}", job.id, job.command, exit.line()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_back() {
        assert_eq!("b12".parse::<Id>(), Ok(Id(12)));
        assert_eq!(Id(3).to_string(), "b3");
        assert!("12".parse::<Id>().is_err() && "bx".parse::<Id>().is_err());
    }
}
