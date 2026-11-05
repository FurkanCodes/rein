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
//! own default. `/mode` writes `mode` here when you keep one, and leaves
//! the rest of the file as it was.

use std::collections::HashMap;
use std::path::Path;

pub const FILE: &str = "config";
const KEYS: [&str; 13] = ["model", "effort", "mode", "search", "compact_at", "max_steps", "bell", "compress", "sandbox", "memory", "peers", "strip", "tab_command"];

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

    /// `key` as the file has it, whatever the environment says.
    pub fn in_file(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

/// Sets `key` to `value` in the settings file at `path`: in place of the
/// line it had, or after the rest. Comments and other settings stay as
/// they were, and the file is written whole.
pub fn set(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let text = with(&text, key, value);
    // What's written must read back.
    Settings::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    rein_llm::store::write_private(path, text.as_bytes()).map_err(|e| format!("can't write {}: {}", path.display(), crate::describe(e)))
}

/// `text` with `key = value` in it once: where `key` was first, or at the end.
fn with(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = {value}");
    let mut lines: Vec<String> = Vec::new();
    let mut placed = false;
    for own in text.lines() {
        let is_key = own.split_once('=').is_some_and(|(name, _)| name.trim() == key);
        match (is_key, placed) {
            (false, _) => lines.push(own.to_owned()),
            (true, false) => {
                lines.push(line.clone());
                placed = true;
            }
            (true, true) => {}
        }
    }
    if !placed {
        lines.push(line);
    }
    lines.join("\n") + "\n"
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
    fn a_setting_is_set_in_its_place_and_the_rest_kept() {
        let text = "# mine\nmodel = gpt\nmode = ask\nbell = off\n";
        assert_eq!(with(text, "mode", "edits"), "# mine\nmodel = gpt\nmode = edits\nbell = off\n");
        assert_eq!(with("model = gpt", "mode", "trust"), "model = gpt\nmode = trust\n");
        assert_eq!(with("", "mode", "trust"), "mode = trust\n");
        assert_eq!(with("# mode = edits\nmode=ask\nmode = auto\n", "mode", "trust"), "# mode = edits\nmode = trust\n", "once, and not in a comment");
        let dir = std::env::temp_dir().join(format!("rein-settings-{}", std::process::id()));
        let path = dir.join("config");
        set(&path, "mode", "edits").unwrap();
        set(&path, "bell", "off").unwrap();
        set(&path, "mode", "auto").unwrap();
        let read = Settings::load(&path).unwrap();
        assert_eq!((read.in_file("mode"), read.in_file("bell")), (Some("auto"), Some("off")));
        assert!(set(&path, "colour", "blue").is_err(), "only what reads back");
        assert_eq!(Settings::load(&path).unwrap().in_file("mode"), Some("auto"), "and nothing written then");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn says_what_is_wrong() {
        assert_eq!(
            Settings::parse("colour = blue").unwrap_err(),
            "line 1: unknown setting colour; use model, effort, mode, search, compact_at, max_steps, bell, compress, sandbox, memory, peers, strip, tab_command"
        );
        assert_eq!(Settings::parse("model").unwrap_err(), "line 1: write it as key = value");
    }
}
