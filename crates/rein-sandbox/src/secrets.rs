//! Keys, tokens and passwords in the environment rein was started with.
//!
//! Other programs of yours can read that environment: on macOS any process
//! of the same user can (the kernel hands out another's through
//! `KERN_PROCARGS2`), and then a key you exported for rein would be one
//! command away. So rein keeps a copy for itself, where nothing else can
//! read it, and wipes the values from the environment it was given.

use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

/// What [`keep_with`] took out, for rein, and for commands that run outside
/// the sandbox, which get your whole environment.
static KEPT: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();

/// Takes the keys, tokens and passwords out of rein's environment, keeping
/// them for [`get`] and [`all`]: each variable [`crate::passes`] refuses.
/// Call it first in `main`, while rein has one thread: nothing may read the
/// environment while it changes.
pub fn keep() {
    keep_with(&[]);
}

/// [`keep`], with the variables named in `extra` taken out too, whatever
/// their names say: the ones that hold your providers' keys. A command in
/// the sandbox doesn't get them either. Only the first call counts.
pub fn keep_with(extra: &[&str]) {
    let kept: Vec<(OsString, OsString)> = std::env::vars_os().filter(|(name, value)| secret(&name.to_string_lossy(), value, extra)).collect();
    if KEPT.set(kept).is_err() {
        return;
    }
    wipe();
}

/// Whether the variable `name`, with `value`, is a secret: [`crate::passes`]
/// refuses it, or it's one of `extra`.
fn secret(name: &str, value: &OsStr, extra: &[&str]) -> bool {
    !crate::passes(name, value) || extra.contains(&name)
}

/// Whether a command in the sandbox goes without `name`, at `value`: a
/// secret, or one [`keep_with`] took out, whose value here is wiped.
pub(crate) fn withheld(name: &str, value: &OsStr) -> bool {
    !crate::passes(name, value) || KEPT.get().is_some_and(|kept| kept.iter().any(|(kept, _)| kept == name))
}

/// A kept variable's value: what the environment said before [`keep`], or
/// says now for the rest.
pub fn get(name: &str) -> Option<String> {
    var_os(name).and_then(|value| value.into_string().ok())
}

/// [`get`], for a value that may not be text: a path, say.
pub(crate) fn var_os(name: &str) -> Option<OsString> {
    let kept = KEPT.get().and_then(|kept| kept.iter().find(|(n, _)| n == name)).map(|(_, value)| value.clone());
    kept.or_else(|| std::env::var_os(name))
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
        use std::os::unix::ffi::OsStrExt;
        let kept = all();
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
                    && kept.iter().any(|(name, _)| name.as_bytes() == &text[..equals])
                {
                    std::ptr::write_bytes(entry.add(equals + 1), 0, text.len() - equals - 1);
                }
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `keep_with` leaves, as rein and as other processes see it, from
    /// a process of its own: the test binary again, running only this.
    #[test]
    fn keep_with_takes_out_the_names_it_is_given() {
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args(["--exact", "secrets::tests::keep_with_in_a_process_of_its_own", "--ignored", "--nocapture", "--test-threads", "1"])
            .env("REIN_TEST_PROVIDER_VAR", "FAKE-provider-value-123")
            .env("REIN_TEST_PLAIN", "FAKE-plain-value-456")
            .env("REIN_TEST_DB", "postgres://me:FAKE-db-789@localhost/db")
            .output()
            .unwrap();
        let said = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success() && said.contains("1 passed"), "{said}");
    }

    #[test]
    #[ignore = "run by keep_with_takes_out_the_names_it_is_given, in a process of its own"]
    fn keep_with_in_a_process_of_its_own() {
        keep_with(&["REIN_TEST_PROVIDER_VAR"]);
        // rein still has it.
        assert_eq!(get("REIN_TEST_PROVIDER_VAR").as_deref(), Some("FAKE-provider-value-123"));
        assert_eq!(get("REIN_TEST_DB").as_deref(), Some("postgres://me:FAKE-db-789@localhost/db"));
        // Its environment doesn't, and nor does a command in the sandbox.
        assert_eq!(std::env::var("REIN_TEST_PROVIDER_VAR").as_deref(), Ok(""));
        assert_eq!(std::env::var("REIN_TEST_DB").as_deref(), Ok(""));
        assert!(withheld("REIN_TEST_PROVIDER_VAR", OsStr::new("")));
        assert!(withheld("REIN_TEST_DB", OsStr::new("")));
        // What's not secret stays.
        assert_eq!(std::env::var("REIN_TEST_PLAIN").as_deref(), Ok("FAKE-plain-value-456"));
        assert!(!withheld("REIN_TEST_PLAIN", OsStr::new("FAKE-plain-value-456")));
        assert!(all().iter().all(|(name, _)| name != "REIN_TEST_PLAIN"));
        // What other processes read of this one.
        let seen = own_environment();
        assert!(seen.contains("FAKE-plain-value-456"), "the probe reads the environment");
        assert!(!seen.contains("FAKE-provider-value-123") && !seen.contains("FAKE-db-789"), "{seen}");
    }

    /// This process's environment as another process of yours reads it.
    fn own_environment() -> String {
        #[cfg(target_os = "macos")]
        {
            let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, i32::try_from(std::process::id()).unwrap()];
            let mut size: libc::size_t = 0;
            // SAFETY: asks only for the size; `mib` holds three ints.
            assert_eq!(unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) }, 0);
            let mut buf = vec![0u8; size];
            // SAFETY: `buf` holds `size` bytes, as the kernel was told.
            assert_eq!(unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) }, 0);
            buf.truncate(size);
            String::from_utf8_lossy(&buf).into_owned()
        }
        #[cfg(not(target_os = "macos"))]
        String::from_utf8_lossy(&std::fs::read("/proc/self/environ").unwrap()).into_owned()
    }

    #[test]
    fn a_secret_is_one_passes_refuses_or_one_named() {
        assert!(secret("OPENAI_API_KEY", OsStr::new("x"), &[]));
        assert!(secret("MY_PROVIDER", OsStr::new("x"), &["MY_PROVIDER"]));
        assert!(!secret("MY_PROVIDER", OsStr::new("x"), &["OTHER"]));
        assert!(!secret("HOME", OsStr::new("/home/me"), &["MY_PROVIDER"]));
    }
}
