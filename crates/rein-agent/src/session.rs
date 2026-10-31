//! Conversations saved to disk, so they can be continued later.
//!
//! One file per conversation, in JSON Lines: a header line, then one line
//! per conversation item, appended the moment the item joins the history.
//! A crash loses at most the answer that was still streaming. Files are
//! private to the user (0600, in 0700 folders): they hold everything said
//! and every file the model read.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rein_llm::{Item, Result, ToolCall};
use serde::{Deserialize, Serialize};

/// What a conversation file starts with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// Also the `session-id` sent with every request, so a continued
    /// conversation can still hit the prompt cache.
    pub id: String,
    /// The folder rein was working in.
    pub cwd: PathBuf,
    /// When it started, in local time.
    pub started: String,
    pub model: String,
    /// The system prompt, kept so a continued conversation sends exactly
    /// the same one.
    pub instructions: Option<String>,
}

/// One line of a conversation file. The format is rein's own, not a
/// provider's, so saved conversations outlive provider changes.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
    Session(Header),
    /// What the model is told before the first message (see `context`).
    /// Sent as a user message, but not something the user typed.
    Context { text: String },
    User { text: String },
    Assistant { text: String },
    ToolCall { call_id: String, name: String, arguments: String },
    ToolResult { call_id: String, output: String },
    Opaque { value: serde_json::Value },
}

impl Line {
    fn from_item(item: &Item) -> Line {
        match item {
            Item::User(text) => Line::User { text: text.clone() },
            Item::Assistant(text) => Line::Assistant { text: text.clone() },
            Item::ToolCall(call) => Line::ToolCall { call_id: call.call_id.clone(), name: call.name.clone(), arguments: call.arguments.clone() },
            Item::ToolResult { call_id, output } => Line::ToolResult { call_id: call_id.clone(), output: output.clone() },
            Item::Opaque(value) => Line::Opaque { value: value.clone() },
        }
    }

    /// The conversation item this line holds; `None` for the header.
    fn into_item(self) -> Option<Item> {
        Some(match self {
            Line::Session(_) => return None,
            Line::Context { text } | Line::User { text } => Item::User(text),
            Line::Assistant { text } => Item::Assistant(text),
            Line::ToolCall { call_id, name, arguments } => Item::ToolCall(ToolCall { call_id, name, arguments }),
            Line::ToolResult { call_id, output } => Item::ToolResult { call_id, output },
            Line::Opaque { value } => Item::Opaque(value),
        })
    }
}

/// A conversation's file. Nothing is written until the first item, so a
/// conversation you leave without a word leaves no file behind.
pub struct Session {
    path: PathBuf,
    header: Header,
    file: Option<File>,
    /// The file ends in a half-written line (rein was killed mid-write):
    /// end it before appending, or the next line would be glued onto it.
    unfinished: bool,
}

impl Session {
    /// A new conversation, saved in `dir` as `<stamp>-<id>.jsonl`.
    pub fn new(dir: &Path, stamp: &str, header: Header) -> Session {
        let short: String = header.id.chars().take(8).collect();
        Session { path: dir.join(format!("{stamp}-{short}.jsonl")), header, file: None, unfinished: false }
    }

    /// Reads a saved conversation, ready to continue. Lines that don't
    /// parse, like one cut off by a crash, are skipped.
    pub fn load(path: &Path) -> Result<(Session, Vec<Item>)> {
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines().filter_map(|line| serde_json::from_str::<Line>(line).ok());
        let Some(Line::Session(header)) = lines.next() else {
            return Err(std::io::Error::other(format!("{} isn't a rein conversation", path.display())).into());
        };
        let items = lines.filter_map(Line::into_item).collect();
        let unfinished = !text.is_empty() && !text.ends_with('\n');
        Ok((Session { path: path.to_owned(), header, file: None, unfinished }, items))
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, item: &Item) -> Result<()> {
        self.write(&Line::from_item(item))
    }

    pub fn append_context(&mut self, text: &str) -> Result<()> {
        self.write(&Line::Context { text: text.to_owned() })
    }

    fn write(&mut self, line: &Line) -> Result<()> {
        let mut text = serde_json::to_string(line)?;
        text.push('\n');
        // One write per line, so a crash can only cut off the last one.
        self.open()?.write_all(text.as_bytes())?;
        Ok(())
    }

    fn open(&mut self) -> Result<&mut File> {
        if self.file.is_none() {
            let mut file = open_private(&self.path)?;
            if file.metadata()?.len() == 0 {
                let mut header = serde_json::to_string(&Line::Session(self.header.clone()))?;
                header.push('\n');
                file.write_all(header.as_bytes())?;
            } else if self.unfinished {
                file.write_all(b"\n")?;
            }
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("opened above"))
    }
}

/// Opens `path` for appending, creating it readable by the user only.
fn open_private(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Where the conversations held in `cwd` are saved: one folder per project
/// under `sessions`, named after its path, like `-Users-me-Projects-rein`.
pub fn folder(sessions: &Path, cwd: &Path) -> PathBuf {
    let name: String = cwd.to_string_lossy().chars().map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c }).collect();
    sessions.join(name)
}

/// A saved conversation, as listed for choosing one to continue.
pub struct Summary {
    pub path: PathBuf,
    /// When something was last added to it.
    pub modified: SystemTime,
    pub started: String,
    /// The first thing the user typed.
    pub first: Option<String>,
    /// How many messages the user typed.
    pub messages: usize,
}

/// The conversations saved in `folder`, the most recently used first.
pub fn list(folder: &Path, limit: usize) -> Vec<Summary> {
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files
        .into_iter()
        .take(limit)
        .filter_map(|(modified, path)| {
            let text = fs::read_to_string(&path).ok()?;
            let mut lines = text.lines().filter_map(|line| serde_json::from_str::<Line>(line).ok());
            let Some(Line::Session(header)) = lines.next() else { return None };
            let typed: Vec<String> = lines
                .filter_map(|line| match line {
                    Line::User { text } => Some(text),
                    _ => None,
                })
                .collect();
            Some(Summary { path, modified, started: header.started, first: typed.first().cloned(), messages: typed.len() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-session-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn header(id: &str) -> Header {
        Header { id: id.into(), cwd: "/work".into(), started: "2026-11-01 10:00".into(), model: "m".into(), instructions: Some("be brief".into()) }
    }

    #[test]
    fn saves_and_loads_a_conversation() {
        let dir = dir("roundtrip");
        let mut session = Session::new(&dir, "2026-11-01T10-00-00", header("abcdef123456"));
        assert!(!session.path().exists(), "nothing is written before the first item");
        let items = vec![
            Item::User("hi".into()),
            Item::ToolCall(ToolCall { call_id: "c1".into(), name: "read".into(), arguments: "{}".into() }),
            Item::ToolResult { call_id: "c1".into(), output: "text".into() },
            Item::Opaque(serde_json::json!({ "type": "reasoning", "encrypted_content": "xyz" })),
            Item::Assistant("hello".into()),
        ];
        session.append_context("<environment>…</environment>").unwrap();
        for item in &items {
            session.append(item).unwrap();
        }
        assert!(session.path().ends_with("2026-11-01T10-00-00-abcdef12.jsonl"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(session.path()).unwrap().permissions().mode() & 0o777, 0o600);
        }

        let (loaded, history) = Session::load(session.path()).unwrap();
        assert_eq!(loaded.header(), &header("abcdef123456"));
        assert_eq!(history[0], Item::User("<environment>…</environment>".into()));
        assert_eq!(&history[1..], &items[..]);

        let listed = list(&dir, 10);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].first.as_deref(), Some("hi"), "the context isn't something the user typed");
        assert_eq!(listed[0].messages, 1);
    }

    #[test]
    fn a_line_cut_off_by_a_crash_is_skipped_and_ended() {
        let dir = dir("crash");
        let mut session = Session::new(&dir, "s", header("id"));
        session.append(&Item::User("one".into())).unwrap();
        // rein was killed halfway through writing the next line.
        let mut file = OpenOptions::new().append(true).open(session.path()).unwrap();
        file.write_all(br#"{"type":"assistant","te"#).unwrap();

        let (mut session, history) = Session::load(session.path()).unwrap();
        assert_eq!(history, vec![Item::User("one".into())]);
        session.append(&Item::User("two".into())).unwrap();
        let (_, history) = Session::load(session.path()).unwrap();
        assert_eq!(history, vec![Item::User("one".into()), Item::User("two".into())]);
    }

    #[test]
    fn lists_the_most_recently_used_first() {
        let dir = dir("list");
        let mut old = Session::new(&dir, "a", header("old"));
        old.append(&Item::User("first".into())).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut new = Session::new(&dir, "b", header("new"));
        new.append(&Item::User("second".into())).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        old.append(&Item::User("again".into())).unwrap();

        let listed = list(&dir, 10);
        assert_eq!(listed.iter().map(|s| s.first.clone().unwrap()).collect::<Vec<_>>(), ["first", "second"]);
        assert_eq!(listed[0].messages, 2);
        assert!(list(&dir.join("missing"), 10).is_empty());
    }

    #[test]
    fn names_the_folder_after_the_project_path() {
        assert_eq!(folder(Path::new("/s"), Path::new("/Users/me/rein")), Path::new("/s/-Users-me-rein"));
    }
}
