//! An agent with nothing to say of its own: it answers what a client asks
//! as it starts, and plays a recording on every prompt.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Said;

const SESSION: &str = "replay";

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Options {
    /// How many times faster than it happened; `None` holds it to the
    /// recording's own timing.
    speed: Option<f64>,
    /// Plays by itself this long after `session/new`, for a client nobody's
    /// at, which needs no prompt typed.
    auto: Option<Duration>,
}

impl Options {
    pub(crate) fn parse(flags: &[String]) -> Result<Options, String> {
        let mut options = Options::default();
        let mut flags = flags.iter();
        while let Some(flag) = flags.next() {
            let value = flags.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--speed" => options.speed = Some(value.parse().ok().filter(|speed| *speed > 0.0).ok_or_else(|| format!("that isn't a speed: {value}"))?),
                "--auto" => options.auto = Some(Duration::from_millis(value.parse().map_err(|_| format!("that isn't a number of milliseconds: {value}"))?)),
                _ => return Err(format!("no such flag: {flag}")),
            }
        }
        Ok(options)
    }
}

/// Standard output, which only whole lines go to.
#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    fn send(&self, message: &Value) {
        let mut out = self.0.lock().expect("no writer panics holding standard output");
        // Gone only once the client has stopped reading.
        let _ = writeln!(out, "{message}").and_then(|()| out.flush());
    }

    fn answer(&self, id: &Value, result: &Value) {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
}

/// Plays `said`, unless `stop` is set. Whether it got to the end.
fn play(said: &[Said], speed: Option<f64>, stop: &AtomicBool, out: &Out) -> bool {
    let started = Instant::now();
    for one in said {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        #[expect(clippy::cast_precision_loss, reason = "a recording isn't 2^52 milliseconds long")]
        let at = Duration::from_secs_f64(one.ms as f64 / 1000.0 / speed.unwrap_or(1.0));
        if let Some(wait) = at.checked_sub(started.elapsed()) {
            std::thread::sleep(wait);
        }
        out.send(&json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": SESSION, "update": one.update } }));
    }
    true
}

pub(crate) fn run(file: &Path, options: Options) -> Result<(), String> {
    let said = Arc::new(crate::read(file)?);
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let stop = Arc::new(AtomicBool::new(false));
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let id = &message["id"];
        match message["method"].as_str() {
            Some("initialize") => out.answer(
                id,
                &json!({
                    "protocolVersion": 1,
                    "agentCapabilities": { "loadSession": false },
                    "agentInfo": { "name": "rein-replay", "title": "rein-replay", "version": "0.0.0" },
                    "authMethods": [],
                }),
            ),
            Some("session/new") => {
                out.answer(id, &json!({ "sessionId": SESSION }));
                if let Some(wait) = options.auto {
                    let (said, stop, out, speed) = (Arc::clone(&said), Arc::clone(&stop), out.clone(), options.speed);
                    std::thread::spawn(move || {
                        std::thread::sleep(wait);
                        play(&said, speed, &stop, &out);
                    });
                }
            }
            Some("session/prompt") => {
                stop.store(false, Ordering::Relaxed);
                let (said, stop, out, speed, id) = (Arc::clone(&said), Arc::clone(&stop), out.clone(), options.speed, id.clone());
                std::thread::spawn(move || {
                    let reason = if play(&said, speed, &stop, &out) { "end_turn" } else { "cancelled" };
                    out.answer(&id, &json!({ "stopReason": reason }));
                });
            }
            Some("session/cancel") => stop.store(true, Ordering::Relaxed),
            Some(_) if !id.is_null() => out.answer(id, &json!({})),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_read_and_what_isnt_one_is_refused() {
        let flags = ["--speed", "2", "--auto", "500"].map(str::to_owned);
        assert_eq!(Options::parse(&flags), Ok(Options { speed: Some(2.0), auto: Some(Duration::from_millis(500)) }));
        assert!(Options::parse(&["--speed".to_owned(), "0".to_owned()]).is_err(), "0 would divide by nothing");
        assert!(Options::parse(&["--fast".to_owned(), "1".to_owned()]).is_err());
    }
}
