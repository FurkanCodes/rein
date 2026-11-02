//! Keys, tokens and passwords in the environment rein was started with.
//!
//! Other programs of yours can read that environment: on macOS a command in
//! the sandbox can too (the kernel hands any process of the same user
//! another's through `KERN_PROCARGS2`, whatever its sandbox says), and then
//! a key you exported for rein would be one command away. So rein keeps a
//! copy for itself, where nothing else can read it, and wipes the values
//! from the environment it was given.

use std::ffi::OsString;
use std::sync::OnceLock;

/// What [`keep`] took out, for rein, and for commands that run outside the
/// sandbox, which get your whole environment.
static KEPT: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();

/// Takes the keys, tokens and passwords out of rein's environment, keeping
/// them for [`get`] and [`all`]. Call it first in `main`, while rein has one
/// thread: nothing may read the environment while it changes.
pub fn keep() {
    let kept: Vec<(OsString, OsString)> = std::env::vars_os().filter(|(name, _)| !crate::passes(&name.to_string_lossy())).collect();
    if KEPT.set(kept).is_err() {
        return;
    }
    wipe();
}

/// A kept variable's value: what the environment said before [`keep`], or
/// says now for the rest.
pub fn get(name: &str) -> Option<String> {
    let kept = KEPT.get().and_then(|kept| kept.iter().find(|(n, _)| n == name)).and_then(|(_, value)| value.to_str().map(str::to_owned));
    kept.or_else(|| std::env::var(name).ok())
}

/// Everything [`keep`] took out, to give back to a command that runs with
/// your full access.
pub fn all() -> &'static [(OsString, OsString)] {
    KEPT.get().map_or(&[], Vec::as_slice)
}

/// Overwrites, in place, the value of each environment entry [`keep`] took
/// out, so what other programs read there is the name and nothing more.
fn wipe() {
    #[cfg(unix)]
    {
        #[cfg(target_os = "macos")]
        // SAFETY: _NSGetEnviron returns the address of the process's
        // `environ` pointer, which is valid for the life of the process.
        let entries = unsafe { *libc::_NSGetEnviron() };
        #[cfg(not(target_os = "macos"))]
        let entries = {
            unsafe extern "C" {
                static mut environ: *mut *mut libc::c_char;
            }
            // SAFETY: reading the pointer itself; `keep` runs while rein has
            // one thread, so nothing changes it meanwhile.
            unsafe { environ }
        };
        if entries.is_null() {
            return;
        }
        let mut i = 0;
        loop {
            // SAFETY: `environ` is a NULL-terminated array of pointers to
            // NUL-terminated `NAME=value` strings in writable memory (the
            // block exec set up, or what setenv allocated). Each entry is
            // read up to its NUL and written only within that length, while
            // nothing else runs.
            unsafe {
                let entry = *entries.add(i);
                if entry.is_null() {
                    break;
                }
                let text = std::ffi::CStr::from_ptr(entry).to_bytes();
                if let Some(equals) = text.iter().position(|&b| b == b'=')
                    && !crate::passes(&String::from_utf8_lossy(&text[..equals]))
                {
                    std::ptr::write_bytes(entry.add(equals + 1), 0, text.len() - equals - 1);
                }
            }
            i += 1;
        }
    }
}
