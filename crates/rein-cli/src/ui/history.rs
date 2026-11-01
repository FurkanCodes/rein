//! What you've sent from a folder, kept between runs, for ↑ and Ctrl-R.
//!
//! One file per folder, `~/.config/rein/history/<folder>.jsonl`, readable
//! only by you, with a line per message:
//!
//! ```text
//! {"at":1793600000,"text":"run the tests and fix what fails"}
//! ```
//!
//! Each message is kept once, where it was last sent. The file only grows
//! as you go; when it holds far more than is kept, it's written again with
//! just that. A message too big to be worth keeping, like one with a whole
//! log pasted in, is kept for this run only.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::editor::Entry;

/// How many messages are kept.
pub const KEEP: usize = 1000;

/// A message longer than this, in bytes, isn't written down.
const BIGGEST: usize = 100_000;

#[derive(Default)]
pub struct History {
    /// Oldest first, each message once.
    kept: Vec<Kept>,
    /// Where they're saved; `None` keeps them for this run only.
    file: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Kept {
    /// When it was sent, in seconds since 1970.
    at: u64,
    #[serde(flatten)]
    entry: Entry,
}

impl History {
    /// The messages saved in `file`, which new ones are added to. A file
    /// that can't be read is no history yet; a line that can't, like one cut
    /// short by a crash, is skipped.
    pub fn load(file: PathBuf) -> History {
        let text = fs::read_to_string(&file).unwrap_or_default();
        let mut history = History::default();
        let mut lines = 0;
        for line in text.lines() {
            lines += 1;
            if let Ok(kept) = serde_json::from_str::<Kept>(line) {
                history.push(kept);
            }
        }
        if lines > 2 * KEEP {
            let mut out = String::new();
            for kept in &history.kept {
                out += &serde_json::to_string(kept).unwrap_or_default();
                out.push('\n');
            }
            let _ = rein_llm::store::write_private(&file, out.as_bytes());
        }
        history.file = Some(file);
        history
    }

    /// Keeps `entry` as the newest message, and saves it.
    pub fn add(&mut self, entry: Entry) {
        let kept = Kept { at: now(), entry };
        if let Some(file) = &self.file
            && let Ok(mut line) = serde_json::to_string(&kept)
            && line.len() <= BIGGEST
        {
            line.push('\n');
            // Opened for each message, so a file written again by another
            // rein meanwhile is the one added to.
            let _ = append(file, line.as_bytes());
        }
        self.push(kept);
    }

    fn push(&mut self, kept: Kept) {
        if kept.entry.full().trim().is_empty() {
            return;
        }
        self.kept.retain(|old| old.entry != kept.entry);
        self.kept.push(kept);
        if self.kept.len() > KEEP {
            self.kept.remove(0);
        }
    }

    pub fn len(&self) -> usize {
        self.kept.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// The `i`th message, oldest first.
    pub fn get(&self, i: usize) -> &Entry {
        &self.kept[i].entry
    }

    /// When the `i`th message was sent, in seconds since 1970.
    pub fn sent(&self, i: usize) -> u64 {
        self.kept[i].at
    }

    /// The messages with every word of `query` in them, ignoring case,
    /// newest first. A big paste counts as its label, not its text.
    pub fn search(&self, query: &str) -> Vec<usize> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        (0..self.kept.len())
            .rev()
            .filter(|&i| {
                let shown = self.kept[i].entry.shown().to_lowercase();
                words.iter().all(|word| shown.contains(word.as_str()))
            })
            .collect()
    }
}

/// Where the messages sent from `cwd` are kept, under rein's folder `dir`:
/// named after its path, like its conversations' folder.
pub fn file(dir: &Path, cwd: &Path) -> PathBuf {
    let mut name = rein_agent::session::folder(&dir.join("history"), cwd).into_os_string();
    name.push(".jsonl");
    PathBuf::from(name)
}

/// How long ago `at` was, in a few characters: `now`, `5m`, `3h`, `2d`.
pub fn ago(at: u64) -> String {
    let secs = now().saturating_sub(at);
    match secs {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..1_209_600 => format!("{}d", secs / 86_400),
        1_209_600..31_536_000 => format!("{}w", secs / 604_800),
        _ => format!("{}y", secs / 31_536_000),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Adds `bytes` to the end of `path` in one write, creating it readable by
/// you only, in a folder only you can open.
fn append(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    let mut options = fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> Entry {
        Entry { text: text.to_owned(), pastes: Vec::new() }
    }

    fn texts(history: &History) -> Vec<String> {
        (0..history.len()).map(|i| history.get(i).full()).collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-history-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn is_kept_between_runs_once_per_message() {
        let dir = temp("runs");
        let path = file(&dir, Path::new("/work/app"));
        assert!(path.ends_with("history/-work-app.jsonl"), "{}", path.display());
        let mut first = History::load(path.clone());
        assert!(first.is_empty(), "no file yet");
        for text in ["run the tests", "fix the parser", "run the tests", "  "] {
            first.add(entry(text));
        }
        assert_eq!(texts(&first), ["fix the parser", "run the tests"], "once, where it was last sent; blank isn't kept");
        // A crash cut the last line short.
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"at\":1,\"te").unwrap();
        let second = History::load(path.clone());
        assert_eq!(texts(&second), ["fix the parser", "run the tests"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "only you can read it");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_newest_and_shrinks_the_file() {
        let dir = temp("trim");
        let path = file(&dir, Path::new("/p"));
        let lines: String = (0..2 * KEEP + 10).map(|n| format!("{{\"at\":{n},\"text\":\"message {n}\"}}\n")).collect();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, lines).unwrap();
        let history = History::load(path.clone());
        assert_eq!(history.len(), KEEP);
        assert_eq!(history.get(KEEP - 1).full(), format!("message {}", 2 * KEEP + 9), "the newest stay");
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), KEEP, "the file was written again with only those");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finds_messages_by_their_words() {
        let mut history = History::default();
        for text in ["add a test for the parser", "run the tests", "Parser: why does it panic?"] {
            history.add(entry(text));
        }
        let found = |query| history.search(query).iter().map(|&i| history.get(i).full()).collect::<Vec<_>>();
        assert_eq!(found("parser"), ["Parser: why does it panic?", "add a test for the parser"], "newest first, any case");
        assert_eq!(found("parser test"), ["add a test for the parser"], "every word, in any order");
        assert_eq!(found("").len(), 3, "nothing typed: everything");
        assert!(found("cargo").is_empty());
    }

    #[test]
    fn says_how_long_ago() {
        let now = now();
        assert_eq!(ago(now), "now");
        assert_eq!(ago(now - 300), "5m");
        assert_eq!(ago(now - 3 * 3600), "3h");
        assert_eq!(ago(now - 2 * 86_400), "2d");
        assert_eq!(ago(now - 21 * 86_400), "3w");
    }
}
