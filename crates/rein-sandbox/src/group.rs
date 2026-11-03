//! A started program's process group, stopped as a whole when it's dropped
//! before the program is done: on a timeout, when the turn's future is
//! dropped (Esc), or when rein lets an MCP server go. Killing the shell or
//! the server alone would leave what it started running.

/// The group a program was started in with `process_group(0)`, by the ID of
/// its leader, the program itself.
pub struct Group(Option<u32>);

impl Group {
    /// The group led by `pid`, as `Child::id` gives it: `None` once the
    /// child has been waited for, and then there's nothing to stop.
    pub fn new(pid: Option<u32>) -> Group {
        Group(pid)
    }

    /// The program finished: nothing to stop, and what it put in the
    /// background on purpose, like a server, is its own business.
    pub fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: killpg only sends a signal. The group is the one the
            // program was started in (`process_group(0)` makes its ID the
            // program's), and the program hasn't been waited for yet, so the
            // ID can't have been reused.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
}
