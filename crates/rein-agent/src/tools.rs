//! The tools the model can call. Each takes its arguments as JSON text and
//! returns text for the model, also when something goes wrong: an error the
//! model can read is one it can work around.

use std::path::PathBuf;
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

    #[test]
    fn long_output_is_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_OUTPUT); // two bytes each
        let cut = truncate(text);
        assert!(cut.ends_with(&format!("\n[{} more bytes cut]", MAX_OUTPUT)));
        assert!(cut.len() < MAX_OUTPUT + 40);
    }
}
