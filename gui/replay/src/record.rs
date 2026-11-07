//! Stands between a client and an agent, and writes down what the agent said
//! in each turn and when, line by line: a window that closes kills this too,
//! and a recording kept until the end would go with it. Nothing the client
//! sends is kept: what it typed may not be something to keep in a file.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value;

use crate::Said;

pub(crate) fn run(out: &Path, agent: &[String]) -> Result<(), String> {
    let mut file = File::create(out).map_err(|e| format!("can't write {}: {e}", out.display()))?;
    let mut child = Command::new(&agent[0])
        .args(&agent[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("can't start {}: {e}", agent[0]))?;
    let mut to_agent = child.stdin.take().ok_or("the agent has no input")?;
    let from_agent = child.stdout.take().ok_or("the agent has no output")?;
    let started: Arc<Mutex<Option<Instant>>> = Arc::default();

    let reading = Arc::clone(&started);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && message["method"] == "session/prompt"
            {
                reading.lock().expect("no writer panics holding the start").get_or_insert_with(Instant::now);
            }
            if writeln!(to_agent, "{line}").and_then(|()| to_agent.flush()).is_err() {
                break;
            }
        }
        // Dropping `to_agent` closes the agent's input, which ends it.
    });

    let mut stdout = std::io::stdout().lock();
    for line in BufReader::new(from_agent).lines().map_while(Result::ok) {
        if let Ok(message) = serde_json::from_str::<Value>(&line)
            && message["method"] == "session/update"
        {
            let began = *started.lock().expect("no writer panics holding the start");
            if let Some(began) = began {
                let ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
                let said = Said { ms, update: message["params"]["update"].clone() };
                writeln!(file, "{}", said.line()).map_err(|e| format!("can't write {}: {e}", out.display()))?;
            }
        }
        if writeln!(stdout, "{line}").and_then(|()| stdout.flush()).is_err() {
            break;
        }
    }
    let _ = child.wait();
    Ok(())
}
