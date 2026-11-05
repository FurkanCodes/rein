//! A started program's process group, stopped as a whole when it's dropped
//! before the program is done: on a timeout, when the turn's future is
//! dropped (Esc), or when rein lets an MCP server go. Killing the shell or
//! the server alone would leave what it started running.

use std::sync::Mutex;

/// Every group a [`Group`] still stops, with whether its leader was waited
/// for: a signal that ends rein (the tab closed) ends it before any Drop
/// runs, so [`stop_all`] stops them on its way out.
static ARMED: Mutex<Vec<(u32, bool)>> = Mutex::new(Vec::new());

fn armed() -> std::sync::MutexGuard<'static, Vec<(u32, bool)>> {
    ARMED.lock().unwrap_or_else(|p| p.into_inner())
}

/// Kills every group rein started that still runs: for when a signal ends
/// rein, and no Drop will.
pub fn stop_all() {
    // Taken out first: dropping a group locks the list again.
    let all = std::mem::take(&mut *armed());
    for (pid, reaped) in all {
        drop(Group { pid: Some(pid), reaped });
    }
}

/// The group a program was started in with `process_group(0)`, by the ID of
/// its leader, the program itself.
pub struct Group {
    pid: Option<u32>,
    /// The leader has been waited for: its ID is the group's only while the
    /// group still has members, so it's signalled only after a look.
    reaped: bool,
}

impl Group {
    /// The group led by `pid`, as `Child::id` gives it: `None` once the
    /// child has been waited for, and then there's nothing to stop.
    pub fn new(pid: Option<u32>) -> Group {
        if let Some(pid) = pid {
            armed().push((pid, false));
        }
        Group { pid, reaped: false }
    }

    /// The program finished: nothing to stop, and what it put in the
    /// background on purpose, like a server, is its own business.
    pub fn disarm(mut self) {
        self.forget();
    }

    /// Not to be stopped any more.
    fn forget(&mut self) {
        if let Some(pid) = self.pid.take() {
            armed().retain(|(armed, _)| *armed != pid);
        }
    }

    /// The leader was waited for, but what it started may still run: from
    /// now on the group is signalled only while it still has members.
    pub fn leader_done(&mut self) {
        self.reaped = true;
        if let Some(pid) = self.pid {
            for armed in armed().iter_mut().filter(|(armed, _)| *armed == pid) {
                armed.1 = true;
            }
        }
    }

    /// Whether anything in the group still runs.
    pub fn alive(&self) -> bool {
        self.signal(0)
    }

    /// Asks everything in the group to stop (`SIGTERM`), as a terminal's
    /// Ctrl-C would: a server gets to tidy up. Whether there was anything.
    pub fn ask_to_stop(&self) -> bool {
        #[cfg(unix)]
        return self.signal(libc::SIGTERM);
        #[cfg(not(unix))]
        false
    }

    /// Sends `signal` to the group; 0 only looks. Whether it reached any.
    #[cfg_attr(not(unix), allow(unused_variables))]
    fn signal(&self, signal: i32) -> bool {
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            if self.reaped && signal != 0 && !self.alive() {
                return false;
            }
            // SAFETY: killpg only sends a signal. The group is the one the
            // program was started in (`process_group(0)` makes its ID the
            // program's). Until the program is waited for, the ID can't
            // have been reused. After, neither Linux nor macOS hands out an
            // ID a group still has, so while it has members (looked at just
            // before, with signal 0) the ID is still this group's: a group
            // gone in between and its ID reused in the same instant would
            // take every other ID being used first.
            return unsafe { libc::killpg(pid, signal) } == 0;
        }
        false
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        self.signal(libc::SIGKILL);
        self.forget();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// One at a time: `stop_all` stops every group, another test's too.
    static ONE: Mutex<()> = Mutex::new(());

    fn one() -> std::sync::MutexGuard<'static, ()> {
        ONE.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn running(pattern: &str) -> bool {
        std::process::Command::new("pgrep").args(["-x", "-f", pattern]).output().is_ok_and(|out| out.status.success())
    }

    #[test]
    fn what_a_finished_leader_left_running_is_stopped_with_its_group() {
        let _one = one();
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 47.11 >/dev/null 2>&1 &"]).process_group(0).spawn().unwrap();
        let mut group = Group::new(Some(child.id()));
        child.wait().unwrap();
        group.leader_done();
        assert!(group.alive(), "the sleep runs on in the group");
        drop(group);
        let gone = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            !running("sleep 47.11")
        });
        assert!(gone, "the sleep was left running");
    }

    #[test]
    fn every_group_still_armed_is_stopped_on_the_way_out() {
        let _one = one();
        let mut child = std::process::Command::new("sleep").arg("47.13").process_group(0).spawn().unwrap();
        let group = Group::new(Some(child.id()));
        // Never dropped, as when a signal ends rein.
        std::mem::forget(group);
        stop_all();
        let status = child.wait().unwrap();
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(libc::SIGKILL), "{status:?}");
    }

    #[test]
    fn a_group_with_no_one_left_is_not_signalled() {
        let _one = one();
        let mut child = std::process::Command::new("true").process_group(0).spawn().unwrap();
        let mut group = Group::new(Some(child.id()));
        child.wait().unwrap();
        group.leader_done();
        assert!(!group.alive());
        assert!(!group.ask_to_stop());
    }
}
