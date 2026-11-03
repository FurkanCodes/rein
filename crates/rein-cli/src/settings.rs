//! Your defaults, in `~/.config/rein/config`:
//!
//! ```text
//! # Comments start with #.
//! model = "gpt-6.1-sol"
//! effort = "medium"
//! mode = "edits"
//! compact_at = 120000
//! max_steps = 200
//! bell = off
//! compress = off
//! sandbox = on
//! peers = hold
//! ```
//!
//! Plain `key = value` lines; quotes are optional. A flag wins over an
//! environment variable, which wins over this file, which wins over rein's
//! own default.

use std::collections::HashMap;
use std::path::Path;

pub const FILE: &str = "config";
const KEYS: [&str; 9] = ["model", "effort", "mode", "compact_at", "max_steps", "bell", "compress", "sandbox", "peers"];

#[derive(Debug, Default)]
pub struct Settings {
    values: HashMap<String, String>,
}

impl Settings {
    /// The settings in `path`; none if there's no file.
    pub fn load(path: &Path) -> Result<Settings, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Settings::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(format!("can't read {}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Settings, String> {
        let mut values = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {}: write it as key = value", n + 1));
            };
            let key = key.trim();
            if !KEYS.contains(&key) {
                return Err(format!("line {}: unknown setting {key}; use {}", n + 1, KEYS.join(", ")));
            }
            let value = value.trim().trim_matches('"').trim();
            values.insert(key.to_owned(), value.to_owned());
        }
        Ok(Settings { values })
    }

    /// `key`, from the environment variable `env` if it's set, otherwise
    /// from the file.
    pub fn get(&self, key: &str, env: &str) -> Option<String> {
        std::env::var(env).ok().filter(|v| !v.is_empty()).or_else(|| self.values.get(key).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_simple_lines() {
        let s = Settings::parse("# mine\nmodel = \"gpt-6.1-terra\"\neffort=low\n\ncompact_at = 90000\n").unwrap();
        assert_eq!(s.get("model", "REIN_TEST_UNSET_1"), Some("gpt-6.1-terra".into()));
        assert_eq!(s.get("effort", "REIN_TEST_UNSET_2"), Some("low".into()));
        assert_eq!(s.get("mode", "REIN_TEST_UNSET_3"), None);
    }

    #[test]
    fn says_what_is_wrong() {
        assert_eq!(Settings::parse("colour = blue").unwrap_err(), "line 1: unknown setting colour; use model, effort, mode, compact_at, max_steps, bell, compress, sandbox, peers");
        assert_eq!(Settings::parse("model").unwrap_err(), "line 1: write it as key = value");
    }
}
