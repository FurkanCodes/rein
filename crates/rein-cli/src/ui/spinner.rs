//! A line that shows rein is busy: `◐ Thinking… (3s · ctrl-c to stop)`.
//!
//! It redraws itself from its own thread, ten times a second, so it keeps
//! moving while rein waits on the network or a command. Whoever prints next
//! stops it first, which erases its line, so the two never draw over each
//! other.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];

pub struct Spinner {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Starts drawing `line(frame, seconds)` on stderr.
    pub fn start(line: impl Fn(char, u64) -> String + Send + 'static) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let started = Instant::now();
            let mut stderr = std::io::stderr();
            for i in 0.. {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                {
                    let _guard = super::screen::lock();
                    let _ = write!(stderr, "\r\x1b[2K{}", line(FRAMES[i % FRAMES.len()], started.elapsed().as_secs()));
                    let _ = stderr.flush();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _guard = super::screen::lock();
            let _ = write!(stderr, "\r\x1b[2K");
            let _ = stderr.flush();
        });
        Spinner { stop, thread: Some(thread) }
    }

    /// Stops it and erases its line.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.halt();
    }
}
