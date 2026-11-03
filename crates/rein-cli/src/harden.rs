//! Keeping what's in rein's memory, its keys among it, in its memory.
//!
//! A crash with core dumps on writes rein's memory to a file, where a later
//! command could read it. And on Linux another process of yours can read
//! rein's memory (`/proc/<pid>/mem`, a debugger) unless rein says it can't
//! be dumped. So, first in `main`: no core dumps on any system, and on Linux
//! not dumpable at all. Commands rein starts begin with no core dumps too,
//! but may turn them back on for themselves; `exec` makes them dumpable
//! again on Linux.

use std::io;

/// Turns off core dumps for rein and, on Linux, makes it undumpable.
pub(crate) fn harden() -> io::Result<()> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit fills the struct we pass, which lives for the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &raw mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    limit.rlim_cur = 0;
    // SAFETY: setrlimit only reads the struct we pass, which lives for the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE takes plain integers and touches no memory of ours.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_core_dumps_and_on_linux_not_dumpable() {
        // Turned on first, as far as allowed, or there'd be nothing to turn off.
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: as in `harden`.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &raw mut limit) }, 0);
        limit.rlim_cur = limit.rlim_max;
        // SAFETY: as in `harden`.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const limit) }, 0);
        if limit.rlim_max == 0 {
            return;
        }
        harden().unwrap();
        let mut limit = libc::rlimit { rlim_cur: 1, rlim_max: 1 };
        // SAFETY: as in `harden`.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &raw mut limit) }, 0);
        assert_eq!(limit.rlim_cur, 0);
        #[cfg(target_os = "linux")]
        // SAFETY: PR_GET_DUMPABLE takes plain integers and returns the flag.
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) }, 0);
    }
}
