//! The tools the model can call. Each takes its arguments as JSON text and
//! returns text for the model, also when something goes wrong: an error the
//! model can read is one it can work around.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rein_llm::{Tool, ToolCall};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The most a tool sends back. Everything the model sees is resent with
/// every later request, so long outputs get cut.
const MAX_OUTPUT: usize = 30_000;

pub struct Tools {
    /// The project directory: `read` can't see outside it, `bash` starts in it.
    root: PathBuf,
    /// How long a command may run before it's stopped.
    timeout: Duration,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
    replace_all: bool,
}

impl Tools {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Tools { root: root.into(), timeout: Duration::from_secs(60) }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// What the model is told about each tool.
    pub fn specs(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "read".into(),
                description: "Read a UTF-8 text file in the project and return its contents.".into(),
                parameters: object(json!({
                    "path": { "type": "string", "description": "Path relative to the project root." }
                })),
            },
            Tool {
                name: "write".into(),
                description: "Create a text file, or replace an existing file's whole contents. Missing folders \
                              are created. To change part of a file, use edit instead."
                    .into(),
                parameters: object(json!({
                    "path": { "type": "string", "description": "Path relative to the project root." },
                    "content": { "type": "string", "description": "The complete new contents of the file." }
                })),
            },
            Tool {
                name: "edit".into(),
                description: "Replace text in a file. old_text must match the file exactly, spaces and \
                              indentation included, and appear exactly once unless replace_all is true: add \
                              surrounding lines to make it unique. Read the file first if you haven't."
                    .into(),
                parameters: object(json!({
                    "path": { "type": "string", "description": "Path relative to the project root." },
                    "old_text": { "type": "string", "description": "The exact text to replace." },
                    "new_text": { "type": "string", "description": "The text to put in its place." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of exactly one." }
                })),
            },
            Tool {
                name: "bash".into(),
                description: "Run a shell command in the project root and return its output and exit code. \
                              The user approves each command before it runs."
                    .into(),
                parameters: object(json!({
                    "command": { "type": "string", "description": "The command line, run with `sh -c`." }
                })),
            },
        ]
    }

    /// Runs one call and returns the text to send back to the model.
    pub async fn run(&self, call: &ToolCall) -> String {
        let result = match call.name.as_str() {
            "read" => self.read(&call.arguments).await,
            "bash" => self.bash(&call.arguments).await,
            "write" => self.write(&call.arguments).await,
            "edit" => self.edit(&call.arguments).await,
            other => Err(format!("there is no tool called {other}")),
        };
        truncate(result.unwrap_or_else(|e| format!("error: {e}")))
    }

    async fn read(&self, arguments: &str) -> Result<String, String> {
        let args: ReadArgs = parse(arguments)?;
        let path = self.inside_root(&args.path)?;
        let bytes = tokio::fs::read(&path).await.map_err(|e| format!("can't read {}: {e}", args.path))?;
        String::from_utf8(bytes).map_err(|_| format!("{} isn't a UTF-8 text file", args.path))
    }

    /// Resolves `path`, following symlinks, and refuses anything outside the
    /// project, so the model can't read, say, your credentials in ~/.config.
    fn inside_root(&self, path: &str) -> Result<PathBuf, String> {
        let root = self.root.canonicalize().map_err(|e| format!("can't open the project directory: {e}"))?;
        let full = root.join(path).canonicalize().map_err(|e| format!("can't read {path}: {e}"))?;
        if full.starts_with(&root) { Ok(full) } else { Err(format!("{path} is outside the project")) }
    }

    /// Where `path` may be written: inside the project, even for a file or
    /// folders that don't exist yet. The deepest part that does exist is
    /// resolved, following symlinks, and must be inside the project.
    fn writable(&self, path: &str) -> Result<PathBuf, String> {
        let root = self.root.canonicalize().map_err(|e| format!("can't open the project directory: {e}"))?;
        if Path::new(path).components().any(|c| c == Component::ParentDir) {
            return Err(format!("{path} contains `..`; use a path inside the project"));
        }
        let full = root.join(path);
        // Walk up to the deepest part that exists. `symlink_metadata` doesn't
        // follow links, so a link pointing nowhere still counts as existing
        // and gets resolved (and refused) below.
        let mut existing = full.as_path();
        let mut missing = Vec::new();
        while existing.symlink_metadata().is_err() {
            missing.push(existing.file_name().ok_or_else(|| format!("{path} isn't a file path"))?);
            existing = existing.parent().ok_or_else(|| format!("{path} isn't a file path"))?;
        }
        let resolved = existing.canonicalize().map_err(|e| format!("can't write {path}: {e}"))?;
        if !resolved.starts_with(&root) {
            return Err(format!("{path} is outside the project"));
        }
        Ok(missing.iter().rev().fold(resolved, |dir, name| dir.join(name)))
    }

    async fn write(&self, arguments: &str) -> Result<String, String> {
        let args: WriteArgs = parse(arguments)?;
        let path = self.writable(&args.path)?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| format!("can't create the folders for {}: {e}", args.path))?;
        }
        let existed = path.exists();
        tokio::fs::write(&path, &args.content).await.map_err(|e| format!("can't write {}: {e}", args.path))?;
        let verb = if existed { "replaced" } else { "created" };
        let lines = args.content.lines().count();
        Ok(format!("{verb} {} ({lines} line{})", args.path, if lines == 1 { "" } else { "s" }))
    }

    async fn edit(&self, arguments: &str) -> Result<String, String> {
        let args: EditArgs = parse(arguments)?;
        let path = self.inside_root(&args.path)?;
        let bytes = tokio::fs::read(&path).await.map_err(|e| format!("can't read {}: {e}", args.path))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{} isn't a UTF-8 text file", args.path))?;
        let edited = replace(&text, &args.old_text, &args.new_text, args.replace_all).map_err(|e| format!("{e} in {}", args.path))?;
        tokio::fs::write(&path, edited.text).await.map_err(|e| format!("can't write {}: {e}", args.path))?;
        let times = if edited.count == 1 { "1 place".to_owned() } else { format!("{} places", edited.count) };
        Ok(format!("edited {}: replaced {times}", args.path))
    }

    async fn bash(&self, arguments: &str) -> Result<String, String> {
        let args: BashArgs = parse(arguments)?;
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If we give up waiting, dropping the child stops it.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("can't start sh: {e}"))?;
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(result) => result.map_err(|e| format!("the command failed to run: {e}"))?,
            Err(_) => return Err(format!("the command took longer than {:?} and was stopped", self.timeout)),
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let status = match output.status.code() {
            Some(code) => format!("exit code {code}"),
            None => "stopped by a signal".to_owned(),
        };
        Ok(format!("{text}[{status}]"))
    }
}

struct Edited {
    text: String,
    count: usize,
}

/// Replaces `old` with `new` in a file's text. Matching ignores the file's
/// line endings and byte-order mark, and the result keeps both, so the
/// model never has to know about either.
fn replace(file: &str, old: &str, new: &str, replace_all: bool) -> Result<Edited, String> {
    let (bom, body) = match file.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", file),
    };
    let crlf = body.contains("\r\n");
    let lf = |s: &str| s.replace("\r\n", "\n");
    let (body, old, new) = (lf(body), lf(old), lf(new));
    if old.is_empty() {
        return Err("old_text is empty; to create a file, use write".into());
    }
    if old == new {
        return Err("old_text and new_text are the same".into());
    }
    let count = body.matches(&old).count();
    match count {
        0 => return Err("old_text wasn't found; it must match exactly, spaces and indentation included. Read the file and copy the text".into()),
        1 => {}
        n if !replace_all => return Err(format!("old_text appears {n} times; add surrounding lines to make it unique, or set replace_all")),
        _ => {}
    }
    let mut text = body.replace(&old, &new);
    if crlf {
        text = text.replace('\n', "\r\n");
    }
    Ok(Edited { text: format!("{bom}{text}"), count })
}

fn parse<T: DeserializeOwned>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("bad arguments: {e}"))
}

/// A JSON Schema object in the form strict mode wants: every property
/// required, nothing extra allowed.
fn object(properties: Value) -> Value {
    let required: Vec<String> = properties.as_object().map(|p| p.keys().cloned().collect()).unwrap_or_default();
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// Cuts `text` to `MAX_OUTPUT` bytes, on a character boundary, and says so.
fn truncate(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    let mut cut = MAX_OUTPUT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = text.len() - cut;
    text.truncate(cut);
    text.push_str(&format!("\n[{dropped} more bytes cut]"));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test, with `files` written into it.
    fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-tools-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (path, text) in files {
            std::fs::write(dir.join(path), text).unwrap();
        }
        dir
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall { call_id: "call_1".into(), name: name.into(), arguments: arguments.to_string() }
    }

    #[tokio::test]
    async fn read_returns_the_file() {
        let tools = Tools::new(project("read", &[("a.txt", "hello")]));
        assert_eq!(tools.run(&call("read", json!({"path": "a.txt"}))).await, "hello");
        let missing = tools.run(&call("read", json!({"path": "nope.txt"}))).await;
        assert!(missing.starts_with("error: can't read nope.txt"), "{missing}");
    }

    #[tokio::test]
    async fn read_refuses_paths_outside_the_project() {
        let tools = Tools::new(project("outside", &[]));
        // Enough `..` to reach / from any temp dir; extra ones stay at /.
        let climb = format!("{}etc/hosts", "../".repeat(20));
        for path in [climb.as_str(), "/etc/hosts"] {
            let answer = tools.run(&call("read", json!({ "path": path }))).await;
            assert_eq!(answer, format!("error: {path} is outside the project"));
        }
    }

    #[tokio::test]
    async fn bash_returns_output_and_exit_code() {
        let tools = Tools::new(project("bash", &[]));
        let answer = tools.run(&call("bash", json!({"command": "echo hi; echo oops >&2; exit 3"}))).await;
        assert_eq!(answer, "hi\noops\n[exit code 3]");
    }

    #[tokio::test]
    async fn bash_stops_slow_commands() {
        let tools = Tools::new(project("slow", &[])).with_timeout(Duration::from_millis(200));
        let answer = tools.run(&call("bash", json!({"command": "sleep 5"}))).await;
        assert_eq!(answer, "error: the command took longer than 200ms and was stopped");
    }

    #[tokio::test]
    async fn mistakes_come_back_as_text() {
        let tools = Tools::new(project("mistakes", &[]));
        assert_eq!(tools.run(&call("rm", json!({}))).await, "error: there is no tool called rm");
        let bad = tools.run(&call("read", json!({"file": "a.txt"}))).await;
        assert!(bad.starts_with("error: bad arguments: missing field `path`"), "{bad}");
    }

    #[tokio::test]
    async fn write_creates_files_and_folders_inside_the_project() {
        let dir = project("write", &[]);
        let tools = Tools::new(&dir);
        let created = tools.run(&call("write", json!({"path": "src/new/a.txt", "content": "one\ntwo\n"}))).await;
        assert_eq!(created, "created src/new/a.txt (2 lines)");
        assert_eq!(std::fs::read_to_string(dir.join("src/new/a.txt")).unwrap(), "one\ntwo\n");
        let replaced = tools.run(&call("write", json!({"path": "src/new/a.txt", "content": "three"}))).await;
        assert_eq!(replaced, "replaced src/new/a.txt (1 line)");
    }

    #[tokio::test]
    async fn write_refuses_paths_outside_the_project() {
        let dir = project("write-outside", &[]);
        let outside = std::env::temp_dir().join(format!("rein-tools-{}-elsewhere", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let tools = Tools::new(&dir);
        for (path, error) in [
            ("../escape.txt", "error: ../escape.txt contains `..`; use a path inside the project"),
            ("/tmp/escape.txt", "error: /tmp/escape.txt is outside the project"),
            ("link/escape.txt", "error: link/escape.txt is outside the project"),
        ] {
            assert_eq!(tools.run(&call("write", json!({"path": path, "content": "x"}))).await, error);
        }
        assert!(!outside.join("escape.txt").exists());
    }

    #[tokio::test]
    async fn edit_replaces_exactly_one_match() {
        let dir = project("edit", &[("a.rs", "fn a() {}\nfn b() {}\n")]);
        let tools = Tools::new(&dir);
        let edit = |old: &str, new: &str, all: bool| call("edit", json!({"path": "a.rs", "old_text": old, "new_text": new, "replace_all": all}));
        assert_eq!(tools.run(&edit("fn b() {}", "fn c() {}", false)).await, "edited a.rs: replaced 1 place");
        assert_eq!(std::fs::read_to_string(dir.join("a.rs")).unwrap(), "fn a() {}\nfn c() {}\n");

        let twice = tools.run(&edit("() {}", "() { todo!() }", false)).await;
        assert_eq!(twice, "error: old_text appears 2 times; add surrounding lines to make it unique, or set replace_all in a.rs");
        assert_eq!(tools.run(&edit("() {}", "() { todo!() }", true)).await, "edited a.rs: replaced 2 places");

        let missing = tools.run(&edit("fn z() {}", "fn y() {}", false)).await;
        assert!(missing.starts_with("error: old_text wasn't found"), "{missing}");
    }

    #[test]
    fn edits_keep_line_endings_and_the_byte_order_mark() {
        let edited = replace("\u{feff}one\r\ntwo\r\n", "one\ntwo", "uno\ndos", false).unwrap();
        assert_eq!(edited.text, "\u{feff}uno\r\ndos\r\n");
        assert_eq!(replace("a\n", "a", "a", false).err().as_deref(), Some("old_text and new_text are the same"));
    }

    #[test]
    fn long_output_is_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_OUTPUT); // two bytes each
        let cut = truncate(text);
        assert!(cut.ends_with(&format!("\n[{} more bytes cut]", MAX_OUTPUT)));
        assert!(cut.len() < MAX_OUTPUT + 40);
    }
}
