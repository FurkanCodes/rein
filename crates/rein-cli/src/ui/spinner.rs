//! A line that shows rein is busy: `◐ Thinking… (3s · ctrl-c to stop)`.
//!
//! It redraws itself from its own thread, ten times a second, so it keeps
//! moving while rein waits on the network or a command. Whoever prints next
//! stops it first, which erases its line, so the two never draw over each
//! other.

use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];

pub struct Spinner {
    /// Dropping it tells the thread to stop, even mid-wait.
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Starts drawing `line(frame, seconds)` on stderr.
    pub fn start(line: impl Fn(char, u64) -> String + Send + 'static) -> Spinner {
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let started = Instant::now();
            let mut stderr = std::io::stderr();
            for i in 0.. {
                {
                    // Checked under the lock, so the viewer can't open in between.
                    let _guard = super::screen::lock();
                    if !super::screen::viewing() {
                        let _ = write!(stderr, "\r\x1b[2K{}", line(FRAMES[i % FRAMES.len()], started.elapsed().as_secs()));
                        let _ = stderr.flush();
                    }
                }
                // A tenth of a second until the next frame, unless told to
                // stop first.
                if stopped.recv_timeout(Duration::from_millis(100)) != Err(RecvTimeoutError::Timeout) {
                    break;
                }
            }
            super::screen::write("\r\x1b[2K");
        });
        Spinner { stop: Some(stop), thread: Some(thread) }
    }

    /// Stops it and erases its line.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.take();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_at_once() {
        let spinner = Spinner::start(|_, _| String::new());
        std::thread::sleep(Duration::from_millis(30));
        let asked = Instant::now();
        spinner.stop();
        assert!(asked.elapsed() < Duration::from_millis(20), "took {:?}", asked.elapsed());
    }
}
