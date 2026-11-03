//! Claude Code as a subprocess, not an HTTP provider. Built-in tools and
//! customizations are disabled; structured replies ask rein to run its tools.

use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::{Error, Event, Item, Request, Result, ToolCall, Usage};

#[derive(Clone)]
pub(crate) struct Backend {
    token: Option<String>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClaudeCode(..)")
    }
}

impl Backend {
    pub(crate) fn new(token: Option<String>) -> Self {
        Self { token }
    }

    pub(crate) async fn respond(&self, request: &Request<'_>, idle: Duration) -> Result<VecDeque<Result<Event>>> {
        self.run("claude", request, idle).await
    }

    async fn run(&self, program: &str, request: &Request<'_>, idle: Duration) -> Result<VecDeque<Result<Event>>> {
        // Declared before the process group, so it goes after the CLI does.
        let workdir = Workdir::new()?;
        let mut command = Command::new(program);
        command.current_dir(&workdir.0);
        command.env_clear();
        // Never inherit API keys, endpoint overrides, provider switches, or
        // executable hooks from the harness's environment.
        for name in ["HOME", "PATH", "TMPDIR", "USER", "LOGNAME", "LANG", "LC_ALL", "XDG_CONFIG_HOME", "CLAUDE_CONFIG_DIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(unix)]
        if let Some(token) = &self.token {
            hand_over(&mut command, token)?;
        }
        command.args([
            "-p",
            "--output-format",
            "json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--safe-mode",
            "--disable-slash-commands",
            "--no-session-persistence",
            // Only the user's own settings: a project's, even from another
            // folder, can set `env` and `apiKeyHelper`, which `--safe-mode`
            // leaves on.
            "--setting-sources",
            "user",
            "--model",
            request.model,
            "--json-schema",
            &schema(request).to_string(),
        ]);
        if let Some(effort) = request.reasoning_effort {
            let effort = if effort == crate::Effort::Minimal { crate::Effort::Low } else { effort };
            command.args(["--effort", effort.as_str()]);
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|e| Error::Auth(format!("couldn't start Claude Code: {e}; install the `claude` CLI and run `rein login claude-code`")))?;
        // The CLI has its own copy of the token's pipe now; this closes rein's.
        drop(command);
        let mut group = Group(child.id());
        let mut stdin = child.stdin.take().ok_or_else(|| Error::Incomplete("Claude Code stdin unavailable".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Incomplete("Claude Code stdout unavailable".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| Error::Incomplete("Claude Code stderr unavailable".into()))?;
        let prompt = prompt(request).to_string();
        let work = async {
            let write = async {
                stdin.write_all(prompt.as_bytes()).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, Error>(())
            };
            let (_, out, _) = tokio::try_join!(write, read_capped(stdout, idle), read_capped(stderr, idle))?;
            let status = child.wait().await?;
            group.0 = None;
            if !status.success() {
                // Neither stderr nor error JSON is safe: the CLI or an auth
                // helper may echo credentials. Keep failures deliberately terse.
                return Err(Error::Auth(format!("Claude Code exited with {status}; check `claude auth status` or run `claude setup-token`")));
            }
            decode(&out, request)
        };
        // Even a child that closes its pipes and then hangs is bounded.
        tokio::time::timeout(Duration::from_secs(600), work).await.map_err(|_| Error::Incomplete("Claude Code exceeded ten minutes".into()))?
    }
}

/// Where the CLI reads the token rein hands it: a descriptor it inherits.
const TOKEN_FD: &str = "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR";

/// The longest token rein hands over. rein fills the pipe before the CLI
/// starts, so the token must fit in it at once, and a pipe holds at least
/// a page: a longer token would block rein for good.
const LONGEST_TOKEN: usize = 4096;

/// Hands the CLI its token on a pipe, never in its environment: on macOS
/// any process of the user's, a sandboxed command too, can read another's
/// environment (`KERN_PROCARGS2`). rein writes the token and closes its end
/// before the CLI starts, so the CLI reads the token and then the pipe's
/// end. The read end is the only descriptor the CLI gets beyond its stdio.
#[cfg(unix)]
fn hand_over(command: &mut Command, token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    if token.len() > LONGEST_TOKEN {
        return Err(Error::Auth(format!("the Claude Code token is over {LONGEST_TOKEN} bytes; run `claude setup-token` for a new one")));
    }
    // Both ends are close-on-exec: no other command rein starts gets them.
    let (reader, mut writer) = std::io::pipe()?;
    writer.write_all(token.as_bytes())?;
    drop(writer);
    command.env(TOKEN_FD, reader.as_raw_fd().to_string());
    // SAFETY: between fork and exec this only calls fcntl, which is
    // async-signal-safe, on the read end, which the closure owns and so
    // keeps open for as long as the command can spawn.
    unsafe {
        command.pre_exec(move || {
            // Only this descriptor stays open across exec.
            if libc::fcntl(reader.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// A fresh, empty, private folder for one run of the CLI, removed when the
/// run ends or is cancelled. Run in the user's project, the CLI read the
/// project's `.claude/settings.json`: a cloned repo sent the token to a
/// server of its own, or ran a command as the user.
struct Workdir(std::path::PathBuf);

impl Workdir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("rein-claude-code-{}", crate::oauth::random_token()?));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        // Not `recursive`: a folder or link already at this path is an error,
        // never a place to run in.
        builder.create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
            // SAFETY: this group was created for this child with process_group(0).
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
}

async fn read_capped(mut reader: impl AsyncRead + Unpin, idle: Duration) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut bytes = [0; 8192];
        let n = tokio::time::timeout(idle, reader.read(&mut bytes)).await.map_err(|_| Error::Incomplete("Claude Code output timed out".into()))??;
        if n == 0 {
            return Ok(out);
        }
        if out.len().saturating_add(n) > 16 * 1024 * 1024 {
            return Err(Error::Incomplete("Claude Code output exceeded 16 MiB".into()));
        }
        out.extend_from_slice(&bytes[..n]);
    }
}

fn schema(request: &Request<'_>) -> Value {
    let calls: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "object", "properties": {
                    "name": {"type":"string", "enum":[tool.name]}, "arguments": tool.parameters
                }, "required":["name", "arguments"], "additionalProperties": false
            })
        })
        .collect();
    let items = if calls.is_empty() { json!({"type":"object"}) } else { json!({"anyOf": calls}) };
    json!({"type":"object", "properties": {
        "text":{"type":"string"}, "tool_calls":{"type":"array", "items":items, "maxItems": if calls.is_empty() { 0 } else { 64 }}
    }, "required":["text", "tool_calls"], "additionalProperties":false})
}

fn prompt(request: &Request<'_>) -> Value {
    let history: Vec<Value> = request
        .input
        .iter()
        .filter_map(|item| {
            Some(match item {
                Item::User(text) => json!({"role":"user", "text":text}),
                Item::Assistant(text) => json!({"role":"assistant", "text":text}),
                Item::ToolCall(call) => json!({"role":"assistant", "call_id":call.call_id, "name":call.name, "arguments":call.arguments}),
                Item::ToolResult { call_id, output } => json!({"role":"tool", "call_id":call_id, "output":output}),
                Item::Opaque(_) => return None,
            })
        })
        .collect();
    let tools: Vec<Value> = request.tools.iter().map(|t| json!({"name":t.name, "description":t.description, "parameters":t.parameters})).collect();
    json!({
        "task":"Continue this rein conversation. Follow its instructions. Return text and tool_calls using the output schema. To use a tool, put its name and arguments in tool_calls; rein executes it and supplies the result on the next request. Do not execute tools yourself. Return an empty tool_calls array when finished.",
        "instructions":request.instructions, "conversation":history, "tools":tools
    })
}

#[derive(Deserialize)]
struct Reply {
    text: String,
    tool_calls: Vec<Call>,
}

#[derive(Deserialize)]
struct Call {
    name: String,
    arguments: Value,
}

fn decode(bytes: &[u8], request: &Request<'_>) -> Result<VecDeque<Result<Event>>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| Error::Incomplete("Claude Code returned invalid JSON".into()))?;
    if value["is_error"].as_bool() == Some(true) {
        return Err(Error::Auth("Claude Code reported an error; check `claude auth status` and your subscription limits".into()));
    }
    let reply: Reply = serde_json::from_value(value["structured_output"].clone()).map_err(|_| Error::Incomplete("Claude Code returned no valid structured reply".into()))?;
    let mut events = VecDeque::new();
    if !reply.text.is_empty() {
        events.push_back(Ok(Event::TextDelta(reply.text.clone())));
        events.push_back(Ok(Event::Item(Item::Assistant(reply.text))));
    }
    for call in reply.tool_calls {
        if !request.tools.iter().any(|tool| tool.name == call.name) {
            return Err(Error::Incomplete("Claude Code returned an unknown rein tool".into()));
        }
        events.push_back(Ok(Event::Item(Item::ToolCall(ToolCall { call_id: crate::oauth::random_token()?, name: call.name, arguments: call.arguments.to_string() }))));
    }
    let usage = &value["usage"];
    events.push_back(Ok(Event::Completed {
        usage: Usage {
            input_tokens: usage["input_tokens"]
                .as_u64()
                .unwrap_or_default()
                .saturating_add(usage["cache_read_input_tokens"].as_u64().unwrap_or_default())
                .saturating_add(usage["cache_creation_input_tokens"].as_u64().unwrap_or_default()),
            cached_input_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or_default(),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or_default(),
            ..Usage::default()
        },
    }));
    Ok(events)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fake {
        dir: std::path::PathBuf,
        program: String,
    }

    impl Fake {
        fn new(body: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rein-claude-code-{}", crate::oauth::random_token().unwrap()));
            std::fs::create_dir_all(&dir).unwrap();
            let program = dir.join("claude");
            std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self { dir, program: program.to_str().unwrap().into() }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn request() -> Request<'static> {
        Request {
            model: "sonnet",
            instructions: Some("rein rules"),
            input: &[],
            tools: &[],
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
        }
    }

    #[tokio::test]
    async fn subprocess_receives_token_stdin_and_safe_flags() {
        // The token comes on a descriptor, never in the environment, which
        // other processes can read. Reading to the end proves rein's write
        // end didn't leak into the CLI: `cat` would wait on it for good.
        let fake = Fake::new(
            r#"
[ -z "${CLAUDE_CODE_OAUTH_TOKEN+set}" ] || exit 12
[ "$(/bin/cat "/dev/fd/$CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR")" = test-secret ] || exit 10
[ -z "$ANTHROPIC_API_KEY$ANTHROPIC_AUTH_TOKEN$ANTHROPIC_BASE_URL$OPENAI_API_KEY$CLAUDE_CODE_USE_BEDROCK" ] || exit 11
printf '%s\n' "$@" > "$0.args"
/bin/cat > "$0.input"
printf '%s' '{"structured_output":{"text":"Hello from CLI","tool_calls":[]},"usage":{"input_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4,"output_tokens":5}}'
"#,
        );
        let backend = Backend::new(Some("test-secret".into()));
        let events = backend.run(&fake.program, &request(), Duration::from_secs(5)).await.unwrap();
        let args = std::fs::read_to_string(format!("{}.args", fake.program)).unwrap();
        for flag in ["-p", "--safe-mode", "--strict-mcp-config", "--no-session-persistence", "--json-schema", "--tools\n\n", "--setting-sources\nuser\n"] {
            assert!(args.contains(flag), "{args}");
        }
        assert!(!args.contains("--bare"));
        let input: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}.input", fake.program)).unwrap()).unwrap();
        assert_eq!(input["instructions"], "rein rules");
        assert!(matches!(events.front(), Some(Ok(Event::TextDelta(text))) if text == "Hello from CLI"));
        assert!(matches!(events.back(), Some(Ok(Event::Completed { usage })) if usage.input_tokens == 9 && usage.output_tokens == 5 && usage.cached_input_tokens == 3));
        assert!(!format!("{backend:?}").contains("test-secret"));
    }

    #[tokio::test]
    async fn subprocess_runs_in_an_empty_private_folder_on_the_users_settings_alone() {
        // A cloned project's `.claude/settings.json` could send the token
        // elsewhere or run a command: the CLI never runs in it, nor reads
        // a project's settings.
        let fake = Fake::new(
            r#"
pwd -P > "$0.pwd"
/bin/ls -A > "$0.ls"
/bin/ls -ld . > "$0.mode"
printf '%s\n' "$@" > "$0.args"
[ -z "${CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR+set}" ] || exit 12
/bin/cat > /dev/null
printf '%s' '{"structured_output":{"text":"ok","tool_calls":[]}}'
"#,
        );
        Backend::new(None).run(&fake.program, &request(), Duration::from_secs(5)).await.unwrap();
        let read = |what: &str| std::fs::read_to_string(format!("{}.{what}", fake.program)).unwrap();
        let (pwd, project) = (read("pwd"), std::env::current_dir().unwrap().canonicalize().unwrap());
        let pwd = std::path::Path::new(pwd.trim_end());
        assert!(!pwd.starts_with(&project), "ran in rein's folder: {}", pwd.display());
        assert_eq!(read("ls"), "", "the folder was empty");
        assert!(read("mode").starts_with("drwx------"), "{}", read("mode"));
        assert!(read("args").contains("--setting-sources\nuser\n"), "{}", read("args"));
        assert!(!pwd.exists(), "the folder goes when the run ends");
    }

    #[tokio::test]
    async fn a_token_too_long_for_a_pipe_is_refused_before_the_cli_starts() {
        let fake = Fake::new(": > \"$0.ran\"");
        let token = "x".repeat(LONGEST_TOKEN + 1);
        let err = Backend::new(Some(token.clone())).run(&fake.program, &request(), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(&err, Error::Auth(message) if !message.contains(&token)), "{err}");
        assert!(!std::path::Path::new(&format!("{}.ran", fake.program)).exists());
    }

    #[tokio::test]
    async fn a_cancelled_run_removes_its_folder() {
        let fake = Fake::new("pwd -P > \"$0.pwd.tmp\"; /bin/mv \"$0.pwd.tmp\" \"$0.pwd\"; /bin/sleep 30");
        let (backend, request) = (Backend::new(None), request());
        let started = async {
            loop {
                if let Ok(pwd) = std::fs::read_to_string(format!("{}.pwd", fake.program)) {
                    return pwd;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        // The run is dropped once the CLI is in its folder: a stopped turn.
        let pwd = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                result = backend.run(&fake.program, &request, Duration::from_secs(30)) => panic!("the fake never finishes: {:?}", result.err()),
                pwd = started => pwd,
            }
        })
        .await
        .expect("the fake started");
        let pwd = std::path::Path::new(pwd.trim_end());
        assert!(pwd.starts_with(std::env::temp_dir().canonicalize().unwrap()), "{}", pwd.display());
        assert!(!pwd.exists(), "{} is still there", pwd.display());
    }

    #[test]
    fn structured_tools_preserve_names_arguments_and_history() {
        let tools =
            [crate::Tool { strict: true, name: "read".into(), description: "Read a file".into(), parameters: json!({"type":"object", "properties":{"path":{"type":"string"}}}) }];
        let input = [Item::User("read this".into()), Item::ToolResult { call_id: "old".into(), output: "contents".into() }];
        let request = Request { tools: &tools, input: &input, ..request() };
        let events = decode(br#"{"structured_output":{"text":"Checking","tool_calls":[{"name":"read","arguments":{"path":"a.rs"}}]}}"#, &request).unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(Event::Item(Item::ToolCall(call))) if call.name == "read" && call.arguments == r#"{"path":"a.rs"}"# && !call.call_id.is_empty()))
        );
        assert_eq!(schema(&request)["properties"]["tool_calls"]["items"]["anyOf"][0]["properties"]["name"]["enum"][0], "read");
        assert_eq!(prompt(&request)["conversation"][1]["output"], "contents");
        assert!(decode(br#"{"structured_output":{"text":"","tool_calls":[{"name":"bash","arguments":{}}]}}"#, &request).is_err());
    }

    #[tokio::test]
    async fn subprocess_failures_and_malformed_replies_do_not_echo_secrets() {
        for body in ["echo secret >&2; exit 1", "echo '{\"is_error\":true,\"result\":\"secret\"}'", "echo '{\"structured_output\":\"secret\"}'", "echo secret"] {
            let fake = Fake::new(body);
            let err = Backend::new(None).run(&fake.program, &request(), Duration::from_secs(5)).await.unwrap_err();
            assert!(!err.to_string().contains("secret"), "{err}");
        }
    }

    #[tokio::test]
    async fn quiet_subprocess_times_out_and_output_is_capped() {
        let fake = Fake::new("/bin/sleep 30");
        let start = std::time::Instant::now();
        let err = Backend::new(None).run(&fake.program, &request(), Duration::from_millis(100)).await.unwrap_err();
        assert!(matches!(err, Error::Incomplete(_)));
        assert!(start.elapsed() < Duration::from_secs(5));
        let bytes = vec![0; 16 * 1024 * 1024 + 1];
        assert!(read_capped(bytes.as_slice(), Duration::from_secs(1)).await.is_err());
    }
}
