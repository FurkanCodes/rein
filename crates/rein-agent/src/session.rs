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

use rein_llm::{Api, Item, Opaque, Result, ToolCall};
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
    Context {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        call_id: String,
        output: String,
    },
    /// What the tool search found, by name.
    Found {
        call_id: String,
        names: Vec<String>,
    },
    /// An image a call gave: where rein keeps it, never its bytes, which
    /// would make the conversation's file grow by a screenshot each time.
    Image {
        call_id: String,
        kind: rein_llm::ImageKind,
        file: std::path::PathBuf,
        width: u32,
        height: u32,
        about: String,
    },
    /// Provider data, with the format that can read it. Conversations
    /// saved before rein spoke more than one say none: OpenAI's.
    Opaque {
        #[serde(default)]
        api: Api,
        value: serde_json::Value,
    },
    /// Tool outputs replaced to save space (see `compact`). The originals
    /// stay in the file, earlier.
    Retired {
        outputs: Vec<RetiredOutput>,
    },
    /// The conversation switched to another model from here on.
    Model {
        model: String,
    },
    /// The conversation was compacted: from here on it's the first message,
    /// this summary, and `kept`, its latest part word for word.
    Compaction {
        summary: String,
        kept: Vec<Line>,
    },
    /// The provider turned the thinking in the conversation away (it no
    /// longer matched what came before it), so it was dropped from here on.
    Unbound,
}

#[derive(Serialize, Deserialize)]
struct RetiredOutput {
    call_id: String,
    output: String,
}

impl Line {
    fn from_item(item: &Item) -> Line {
        match item {
            Item::User(text) => Line::User { text: text.clone() },
            Item::Assistant(text) => Line::Assistant { text: text.clone() },
            Item::ToolCall(call) => Line::ToolCall { call_id: call.call_id.clone(), name: call.name.clone(), arguments: call.arguments.clone() },
            Item::ToolResult { call_id, output } => Line::ToolResult { call_id: call_id.clone(), output: output.clone() },
            Item::Found { call_id, names } => Line::Found { call_id: call_id.clone(), names: names.clone() },
            Item::Image { call_id, image } => {
                Line::Image { call_id: call_id.clone(), kind: image.kind, file: image.file.clone(), width: image.width, height: image.height, about: image.about.clone() }
            }
            Item::Opaque(opaque) => Line::Opaque { api: opaque.api, value: opaque.value.clone() },
        }
    }

    /// The conversation item this line holds; `None` for the header.
    fn into_item(self) -> Option<Item> {
        Some(match self {
            Line::Session(_) | Line::Model { .. } | Line::Retired { .. } | Line::Compaction { .. } | Line::Unbound => return None,
            Line::Context { text } | Line::User { text } => Item::User(text),
            Line::Assistant { text } => Item::Assistant(text),
            Line::ToolCall { call_id, name, arguments } => Item::ToolCall(ToolCall { call_id, name, arguments }),
            Line::ToolResult { call_id, output } => Item::ToolResult { call_id, output },
            Line::Found { call_id, names } => Item::Found { call_id, names },
            // One whose file is gone, or isn't what it was, is said in words.
            Line::Image { call_id, kind, file, width, height, about } => {
                let data = std::fs::read(&file).ok().filter(|bytes| crate::image::kind_of(bytes) == Some(kind)).map(std::sync::Arc::from);
                Item::Image { call_id, image: rein_llm::Image { kind, data, file, width, height, about } }
            }
            Line::Opaque { api, value } => Item::Opaque(Opaque::new(api, value)),
        })
    }
}

/// A saved conversation's header and the model it uses now, read without
/// the rest of it.
pub fn peek(path: &Path) -> Option<Header> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines().filter_map(|line| serde_json::from_str::<Line>(line).ok());
    let Some(Line::Session(mut header)) = lines.next() else { return None };
    for line in lines {
        if let Line::Model { model } = line {
            header.model = model;
        }
    }
    Some(header)
}

/// A saved conversation, read back.
pub struct Loaded {
    pub session: Session,
    /// What the next request sends.
    pub history: Vec<Item>,
    /// Everything ever said, including what compaction removed.
    pub full: Vec<Item>,
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

    /// Reads a saved conversation, ready to continue: it replays the file,
    /// compactions included, so the history comes out exactly as it was.
    /// Lines that don't parse, like one cut off by a crash, are skipped.
    pub fn load(path: &Path) -> Result<Loaded> {
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines().filter_map(|line| serde_json::from_str::<Line>(line).ok());
        let Some(Line::Session(mut header)) = lines.next() else {
            return Err(std::io::Error::other(format!("{} isn't a rein conversation", path.display())).into());
        };
        let (mut history, mut full) = (Vec::new(), Vec::new());
        for line in lines {
            match line {
                // The header then says the model in use now.
                Line::Model { model } => header.model = model,
                Line::Retired { outputs } => {
                    let outputs: Vec<(String, String)> = outputs.into_iter().map(|o| (o.call_id, o.output)).collect();
                    crate::compact::retire(&mut history, &outputs);
                }
                Line::Unbound => crate::compact::unbind(&mut history),
                Line::Compaction { summary, kept } => {
                    let pinned = history.first().filter(|item| crate::context::is_context(item)).cloned();
                    history = pinned.into_iter().chain([Item::User(summary)]).chain(kept.into_iter().filter_map(Line::into_item)).collect();
                }
                line => {
                    if let Some(item) = line.into_item() {
                        full.push(item.clone());
                        history.push(item);
                    }
                }
            }
        }
        let unfinished = !text.is_empty() && !text.ends_with('\n');
        Ok(Loaded { session: Session { path: path.to_owned(), header, file: None, unfinished }, history, full })
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

    /// Records a switch to `model`, which a continued conversation keeps.
    pub fn append_model(&mut self, model: &str) -> Result<()> {
        self.write(&Line::Model { model: model.to_owned() })?;
        self.header.model = model.to_owned();
        Ok(())
    }

    pub fn append_retired(&mut self, outputs: &[(String, String)]) -> Result<()> {
        let outputs = outputs.iter().map(|(call_id, output)| RetiredOutput { call_id: call_id.clone(), output: output.clone() }).collect();
        self.write(&Line::Retired { outputs })
    }

    /// Records that the conversation's thinking was dropped ([`crate::compact::unbind`]).
    pub fn append_unbound(&mut self) -> Result<()> {
        self.write(&Line::Unbound)
    }

    pub fn append_compaction(&mut self, summary: &str, kept: &[Item]) -> Result<()> {
        self.write(&Line::Compaction { summary: summary.to_owned(), kept: kept.iter().map(Line::from_item).collect() })
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
    pub id: String,
    /// The folder it was held in.
    pub cwd: PathBuf,
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
    page(std::slice::from_ref(&folder.to_owned()), 0, limit).0
}

/// The conversations saved in `folders`, the most recently used first:
/// `limit` of them after the first `skip`, and whether there are more. Only
/// those are read, as a conversation's file can be long.
pub fn page(folders: &[PathBuf], skip: usize, limit: usize) -> (Vec<Summary>, bool) {
    let mut files: Vec<(SystemTime, PathBuf)> =
        folders.iter().flat_map(|folder| saved_in(folder)).filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path))).collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let more = files.len() > skip.saturating_add(limit);
    let summaries = files
        .into_iter()
        .skip(skip)
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
            Some(Summary { path, id: header.id, cwd: header.cwd, modified, started: header.started, first: typed.first().cloned(), messages: typed.len() })
        })
        .collect();
    (summaries, more)
}

/// The files of conversations in `folder`.
fn saved_in(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    entries.filter_map(|entry| entry.ok()).map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")).collect()
}

/// Each project's folder under `sessions`.
pub fn folders(sessions: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sessions) else { return Vec::new() };
    entries.filter_map(|entry| entry.ok()).map(|entry| entry.path()).filter(|path| path.is_dir()).collect()
}

/// The conversation `id` in `folder`, by what its file says: the name holds
/// only the start of the id, and `id` may come from another program, so it
/// never becomes part of a path.
pub fn find(folder: &Path, id: &str) -> Option<PathBuf> {
    saved_in(folder).into_iter().find(|path| {
        let short: String = id.chars().take(8).collect();
        path.file_stem().is_some_and(|stem| stem.to_string_lossy().ends_with(&format!("-{short}"))) && peek(path).is_some_and(|header| header.id == id)
    })
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
    fn an_image_is_saved_by_where_its_kept_and_said_in_words_once_gone() {
        let dir = dir("image");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.png");
        let bytes = crate::image::tests::png(2, 2, png::Compression::Fast);
        std::fs::write(&file, &bytes).unwrap();
        let image = rein_llm::Image {
            kind: rein_llm::ImageKind::Png,
            data: Some(std::sync::Arc::from(bytes.clone())),
            file: file.clone(),
            width: 2,
            height: 2,
            about: "image a.png, 2×2".into(),
        };
        let mut session = Session::new(&dir, "2026-11-01T10-00-00", header("img123456789"));
        session.append(&Item::Image { call_id: "r1".into(), image: image.clone() }).unwrap();
        let text = std::fs::read_to_string(session.path()).unwrap();
        assert!(!text.contains("iVBOR") && text.len() < 400, "never its bytes: {text}");
        assert_eq!(Session::load(session.path()).unwrap().history, [Item::Image { call_id: "r1".into(), image: image.clone() }]);
        std::fs::write(&file, b"not a png").unwrap();
        let Item::Image { image: gone, .. } = &Session::load(session.path()).unwrap().history[0] else { panic!("an image") };
        assert_eq!((gone.data.is_none(), gone.about.as_str()), (true, "image a.png, 2×2"), "a file that isn't what it was: in words");
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
            Item::ToolCall(ToolCall { call_id: "s1".into(), name: "tool_search".into(), arguments: r#"{"query":"issue"}"#.into() }),
            Item::Found { call_id: "s1".into(), names: vec!["mcp__linear__create_issue".into()] },
            Item::Opaque(Opaque::new(Api::Responses, serde_json::json!({ "type": "reasoning", "encrypted_content": "xyz" }))),
            Item::Opaque(Opaque::new(Api::Anthropic, serde_json::json!({ "type": "thinking", "thinking": "", "signature": "s" }))),
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

        let Loaded { session: loaded, history, .. } = Session::load(session.path()).unwrap();
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

        let Loaded { session: mut reopened, history, .. } = Session::load(session.path()).unwrap();
        assert_eq!(history, vec![Item::User("one".into())]);
        reopened.append(&Item::User("two".into())).unwrap();
        let history = Session::load(reopened.path()).unwrap().history;
        assert_eq!(history, vec![Item::User("one".into()), Item::User("two".into())]);
    }

    #[test]
    fn reads_reasoning_saved_before_rein_spoke_more_than_openai() {
        let dir = dir("old-opaque");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.jsonl");
        let header = serde_json::to_string(&Line::Session(header("id"))).unwrap();
        fs::write(&path, format!("{header}\n{{\"type\":\"opaque\",\"value\":{{\"type\":\"reasoning\"}}}}\n")).unwrap();
        let history = Session::load(&path).unwrap().history;
        assert_eq!(history, [Item::Opaque(Opaque::new(Api::Responses, serde_json::json!({ "type": "reasoning" })))]);
    }

    #[test]
    fn dropped_thinking_stays_dropped() {
        let dir = dir("unbound");
        let mut session = Session::new(&dir, "s", header("id"));
        let thinking = Item::Opaque(Opaque::new(Api::Anthropic, serde_json::json!({ "type": "thinking", "thinking": "", "signature": "s" })));
        for item in [Item::User("one".into()), thinking.clone(), Item::Assistant("a".into())] {
            session.append(&item).unwrap();
        }
        session.append_unbound().unwrap();
        session.append(&thinking).unwrap();
        let history = Session::load(session.path()).unwrap().history;
        assert_eq!(history, [Item::User("one".into()), Item::Assistant("a".into()), thinking], "only what came before the drop");
    }

    #[test]
    fn a_model_switch_is_kept() {
        let dir = dir("model");
        let mut session = Session::new(&dir, "s", header("id"));
        session.append(&Item::User("one".into())).unwrap();
        session.append_model("other").unwrap();
        session.append(&Item::User("two".into())).unwrap();
        let loaded = Session::load(session.path()).unwrap();
        assert_eq!(loaded.session.header().model, "other");
        assert_eq!(loaded.history, vec![Item::User("one".into()), Item::User("two".into())]);
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
    fn a_page_says_whether_more_follow_and_reads_across_projects() {
        let dir = dir("page");
        let (first, second) = (dir.join("first"), dir.join("second"));
        for (i, folder) in [&first, &second, &first].into_iter().enumerate() {
            let mut session = Session::new(folder, &format!("s{i}"), header(&format!("id{i}")));
            session.append(&Item::User(format!("message {i}"))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join("a-file"), "not a project").unwrap();
        let mut found = folders(&dir);
        found.sort();
        assert_eq!(found, [dir.join("empty"), first.clone(), second.clone()]);
        let ids = |listed: &[Summary]| listed.iter().map(|summary| summary.id.clone()).collect::<Vec<_>>();
        let (head, more) = page(&found, 0, 2);
        assert_eq!((ids(&head), more), (vec!["id2".to_owned(), "id1".to_owned()], true));
        let (rest, more) = page(&found, 2, 2);
        assert_eq!((ids(&rest), more), (vec!["id0".to_owned()], false));
        assert_eq!(head[0].cwd, PathBuf::from("/work"));
        let (past_the_end, more) = page(&found, usize::MAX, 2);
        assert!(past_the_end.is_empty() && !more);
    }

    #[test]
    fn a_conversation_is_found_by_its_whole_id_as_its_file_says() {
        let dir = dir("find");
        let mut wanted = Session::new(&dir, "a", header("abcdefgh-one"));
        wanted.append(&Item::User("mine".into())).unwrap();
        let mut other = Session::new(&dir, "b", header("abcdefgh-two"));
        other.append(&Item::User("not mine".into())).unwrap();
        let path = wanted.path().to_owned();
        assert_eq!(find(&dir, "abcdefgh-one"), Some(path), "the same start, another conversation");
        assert_eq!(find(&dir, "abcdefgh"), None, "a start isn't an id");
        assert_eq!(find(&dir, "../../keys"), None);
        assert_eq!(find(&dir.join("missing"), "abcdefgh-one"), None);
    }

    #[test]
    fn names_the_folder_after_the_project_path() {
        assert_eq!(folder(Path::new("/s"), Path::new("/Users/me/rein")), Path::new("/s/-Users-me-rein"));
    }
}
