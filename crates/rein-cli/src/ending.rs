//! How rein ends when a signal ends it, before any Drop runs: closing the
//! tab hangs up on it (SIGHUP), and `kill` sends SIGTERM. A sub-agent at
//! work tells the rein that started it first. What it started is stopped
//! (a dev server in the background would have run on for good), and the
//! files that say it's running go, so no one finds a rein that's gone:
//! after the sub-agent has spoken, as the others know it by its file.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The files that go when a signal ends rein.
static FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// `path` goes if a signal ends rein.
pub(crate) fn remove_on_signal(path: PathBuf) {
    FILES.lock().unwrap_or_else(|p| p.into_inner()).push(path);
}

/// A signal is being acted on: one that comes after waits for it.
static ENDING: AtomicBool = AtomicBool::new(false);

/// Listens for the signals that end rein, from now on: once, at the start.
/// A sub-agent ended before its task is done says so first, with what it
/// did, to the rein that started it.
pub(crate) fn watch() {
    #[cfg(unix)]
    for (kind, number, ended) in [
        (tokio::signal::unix::SignalKind::hangup(), libc::SIGHUP, crate::peers::Ended::Closed),
        (tokio::signal::unix::SignalKind::terminate(), libc::SIGTERM, crate::peers::Ended::Killed),
    ] {
        if let Ok(mut signal) = tokio::signal::unix::signal(kind) {
            // For as long as rein runs: nothing to stop it before.
            tokio::spawn(async move {
                if signal.recv().await.is_some() {
                    // Ghostty hangs up again every 10 ms; SIGTERM may follow.
                    if ENDING.swap(true, Ordering::SeqCst) {
                        return std::future::pending().await;
                    }
                    crate::peers::farewell::say_how_it_ended(ended).await;
                    end(number);
                }
            });
        }
    }
}

/// Stops what rein started, removes its files, and exits as `signal` would.
fn end(signal: i32) -> ! {
    rein_agent::sandbox::group::stop_all();
    // Taken out first, so the list isn't locked while they go.
    let files = std::mem::take(&mut *FILES.lock().unwrap_or_else(|p| p.into_inner()));
    for file in files {
        let _ = std::fs::remove_file(file);
    }
    std::process::exit(128 + signal)
}
