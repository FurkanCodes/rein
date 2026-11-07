//! Plays a conversation to a client as an agent would, at the speed it
//! happened: the same input for Zed, for any window, and for any toolkit.
//!
//! `rein-replay record out.jsonl -- rein serve` stands between a client and
//! an agent, and writes down what the agent said and when.
//! `rein-replay make long out.jsonl` writes a made-up workload instead.
//! `rein-replay serve out.jsonl` is an agent that plays it on every prompt.

mod make;
mod record;
mod serve;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

/// One thing the agent said: a `session/update`'s `update`, and when.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Said {
    /// Milliseconds after the turn started.
    pub(crate) ms: u64,
    pub(crate) update: Value,
}

impl Said {
    pub(crate) fn line(&self) -> String {
        serde_json::json!({ "ms": self.ms, "update": self.update }).to_string()
    }
}

/// Reads a recording, which is one [`Said`] a line.
pub(crate) fn read(path: &Path) -> Result<Vec<Said>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            let value: Value = serde_json::from_str(line).map_err(|e| format!("{}:{}: {e}", path.display(), i + 1))?;
            let ms = value["ms"].as_u64().ok_or_else(|| format!("{}:{}: no ms", path.display(), i + 1))?;
            Ok(Said { ms, update: value["update"].clone() })
        })
        .collect()
}

/// Writes a recording whole: a temp file, then a rename.
pub(crate) fn write(path: &Path, said: &[Said]) -> Result<(), String> {
    let mut text = String::new();
    for one in said {
        text.push_str(&one.line());
        text.push('\n');
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, text).map_err(|e| format!("can't write {}: {e}", temp.display()))?;
    std::fs::rename(&temp, path).map_err(|e| format!("can't write {}: {e}", path.display()))
}

const USAGE: &str = "usage:
  rein-replay record <out.jsonl> -- <agent> [args...]
  rein-replay make <workload> <out.jsonl>     (rein-replay make list)
  rein-replay serve <file.jsonl> [--speed N] [--auto MS]";

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [what, out, dashes, agent @ ..] if what == "record" && dashes == "--" && !agent.is_empty() => record::run(Path::new(out), agent),
        [what, list] if what == "make" && list == "list" => {
            println!("{}", make::NAMES.join("\n"));
            Ok(())
        }
        [what, name, out] if what == "make" => make::run(name, Path::new(out)),
        [what, file, flags @ ..] if what == "serve" => serve::run(&PathBuf::from(file), serve::Options::parse(flags)?),
        _ => Err(USAGE.to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
