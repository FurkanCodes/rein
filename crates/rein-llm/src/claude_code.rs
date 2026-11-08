//! Claude Code as a subprocess, not an HTTP provider: your Claude plan, as
//! Claude Code uses it. A conversation is a session of the CLI's kept going
//! ([`live`]), in which the model calls rein's tools as its own, and rein
//! runs them; the CLI's own tools and customizations are off.

mod bridge;
mod live;

pub(crate) use live::Turn;

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::meter::{Meter, Reading, Standing};
use crate::{Error, Loading, Request, Result, Tool};

/// What the CLI keeps of rein's environment: never API keys, endpoint
/// overrides, provider switches, or executable hooks.
const PASSED: [&str; 9] = ["HOME", "PATH", "TMPDIR", "USER", "LOGNAME", "LANG", "LC_ALL", "XDG_CONFIG_HOME", "CLAUDE_CONFIG_DIR"];

#[derive(Clone)]
pub(crate) struct Backend {
    token: Option<String>,
    launcher: live::Launcher,
    sessions: live::Sessions,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClaudeCode(..)")
    }
}

impl Backend {
    pub(crate) fn new(token: Option<String>) -> Self {
        Self { token, launcher: live::Launcher::Cli("claude".into()), sessions: live::Sessions::default() }
    }

    /// The model's next step in `request`'s conversation, as it comes.
    pub(crate) async fn respond(&self, request: &Request<'_>, idle: Duration) -> Result<Turn> {
        live::turn(&self.sessions, &self.launcher, self.token.as_deref(), request, idle).await
    }

    /// Searches the web for `question` with `model` and Claude Code's own
    /// WebSearch, the one tool it's given: its answer and the sources it
    /// names, in the shape asked for.
    pub(crate) async fn search(&self, model: &str, question: &str, idle: Duration) -> Result<crate::search::Searched> {
        self.search_with("claude", model, question, idle).await
    }

    async fn search_with(&self, program: &str, model: &str, question: &str, idle: Duration) -> Result<crate::search::Searched> {
        let job = Job { model, tool: "WebSearch", schema: crate::search::schema(), prompt: crate::search::asked(question) };
        crate::search::from_cli(self.exec(program, &job, idle).await?.answer()?)
    }

    /// Runs the CLI for `job`: what it printed, read, however it ended.
    async fn exec(&self, program: &str, job: &Job<'_>, idle: Duration) -> Result<Printed> {
        // Declared before the process group, so it goes after the CLI does.
        let workdir = Workdir::new()?;
        let mut command = Command::new(program);
        command.current_dir(&workdir.0);
        command.env_clear();
        for name in PASSED {
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
            "--tools",
            job.tool,
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
            job.model,
            "--json-schema",
            &job.schema.to_string(),
        ]);
        // A print run asks before each tool and, with nobody to answer, is
        // turned down: the one it's given is allowed. Streamed, the answer
        // comes with what the CLI says of your plan's limits.
        command.args(["--allowedTools", job.tool, "--output-format", "stream-json", "--verbose"]);
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
        let input = job.prompt.clone();
        let work = async {
            let write = async {
                stdin.write_all(input.as_bytes()).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, Error>(())
            };
            let (_, out, _) = tokio::try_join!(write, read_capped(stdout, idle), read_capped(stderr, idle))?;
            let status = child.wait().await?;
            group.0 = None;
            Ok(Printed::read(&out, status))
        };
        // Even a child that closes its pipes and then hangs is bounded.
        tokio::time::timeout(Duration::from_secs(600), work).await.map_err(|_| Error::Incomplete("Claude Code exceeded ten minutes".into()))?
    }
}

/// One run of the CLI, for a web search: the model, the one tool of its
/// own it may use, the shape of its answer, and what it's asked.
struct Job<'a> {
    model: &'a str,
    tool: &'a str,
    schema: Value,
    prompt: String,
}

/// What a run of the CLI printed, read: its answer, and what it said of
/// your plan's limits on the way.
struct Printed {
    /// The `result` line, as `json` output gives it: `None` when there's none.
    result: Option<Value>,
    /// The plan's limits, as it last said each.
    meters: Vec<Meter>,
    status: std::process::ExitStatus,
}

impl Printed {
    fn read(out: &[u8], status: std::process::ExitStatus) -> Printed {
        let (mut result, mut meters) = (None, Vec::new());
        for line in out.split(|&b| b == b'\n') {
            let Ok(value) = serde_json::from_slice::<Value>(line) else { continue };
            match value["type"].as_str() {
                Some("result") => result = Some(value),
                Some("rate_limit_event") => crate::meter::merge(&mut meters, plan_meters(&value["rate_limit_info"])),
                _ => {}
            }
        }
        Printed { result, meters, status }
    }

    /// The answer, or why there's none: the plan's limit, where the CLI
    /// said one is reached, rather than a sign-in to check. Neither stderr
    /// nor the CLI's error is safe to show, as it or an auth helper may
    /// echo credentials: failures stay terse.
    fn answer(&self) -> Result<&Value> {
        let failed = self.result.as_ref().is_none_or(|result| result["is_error"] == true);
        if (failed || !self.status.success())
            && let Some(reached) = self.meters.iter().find(|meter| meter.standing == Standing::Reached)
        {
            return Err(Error::usage_limit(format!("your Claude plan's {} limit is reached", reached.label())));
        }
        if !self.status.success() {
            return Err(Error::Auth(format!("Claude Code exited with {}; check `claude auth status` or run `claude setup-token`", self.status)));
        }
        let result = self.result.as_ref().ok_or_else(|| Error::Incomplete("Claude Code returned invalid JSON".into()))?;
        if result["is_error"] == true {
            return Err(Error::Auth("Claude Code reported an error; check `claude auth status` and your subscription limits".into()));
        }
        Ok(result)
    }
}

/// What the CLI says of your plan's limits, a `rate_limit_event`'s
/// `rate_limit_info`: every window at once in `unifiedWindows`, as 2.1.318
/// sends it, or else the one window it's about.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RateLimitInfo {
    status: Option<Status>,
    resets_at: Option<f64>,
    rate_limit_type: Option<String>,
    utilization: Option<f64>,
    unified_windows: BTreeMap<String, Window>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Window {
    status: Option<Status>,
    /// Unix seconds.
    resets_at: Option<f64>,
    /// The share used: 0.28 for 28%.
    utilization: Option<f64>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Status {
    Allowed,
    AllowedWarning,
    Rejected,
    #[serde(other)]
    Other,
}

/// Your plan's windows in `info`, as meters; what can't be read is left out.
fn plan_meters(info: &Value) -> Vec<Meter> {
    let Ok(info) = RateLimitInfo::deserialize(info) else { return Vec::new() };
    let windows = if info.unified_windows.is_empty() {
        let window = Window { status: info.status, resets_at: info.resets_at, utilization: info.utilization };
        info.rate_limit_type.map(|name| (name, window)).into_iter().collect()
    } else {
        info.unified_windows
    };
    windows.into_iter().filter_map(|(name, window)| plan_meter(&name, &window)).collect()
}

/// The window called `name` as a meter. Its length is in its name
/// (`five_hour`, `seven_day_opus`), so a window rein hasn't seen yet shows
/// as well, as long as it says how long it is.
fn plan_meter(name: &str, window: &Window) -> Option<Meter> {
    let mut words = name.splitn(3, '_');
    let count: u64 = match words.next()? {
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "ten" => 10,
        "twelve" => 12,
        "fourteen" => 14,
        "thirty" => 30,
        _ => return None,
    };
    let unit = match words.next()? {
        "hour" => 3600,
        "day" => 86_400,
        "week" => 7 * 86_400,
        "month" => 30 * 86_400,
        _ => return None,
    };
    let only = words.next().unwrap_or_default();
    // Overage is money past the plan, not a window of it.
    if only.contains("overage") {
        return None;
    }
    let used = window.utilization.filter(|used| used.is_finite())?;
    // One over 1 is taken as said in percent.
    let percent = if used <= 1.0 { used * 100.0 } else { used };
    let reading = Reading::Used(percent.round().clamp(0.0, 100.0) as u8);
    let standing = match window.status {
        Some(Status::Rejected) => Standing::Reached,
        Some(Status::AllowedWarning) => Standing::Warned,
        Some(Status::Allowed) => Standing::Fine,
        Some(Status::Other) | None => Standing::of(reading),
    };
    let resets = window.resets_at.and_then(|secs| Duration::try_from_secs_f64(secs).ok()).and_then(|after| SystemTime::UNIX_EPOCH.checked_add(after));
    let mut letters = only.chars().map(|c| if c == '_' { ' ' } else { c });
    let only = letters.next().map(|first| first.to_uppercase().chain(letters).collect::<String>());
    Some(Meter { id: name.to_owned(), only, span: Some(Duration::from_secs(count.saturating_mul(unit))), resets, reading, standing })
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

/// The tools the CLI is told of. Held back, a tool is told of once the
/// search finds it, in the prompt's list, at its end, and in the answer's
/// shape.
fn shown<'a>(request: &'a Request<'_>) -> Vec<&'a Tool> {
    if !request.holds_back() {
        return request.all_tools().collect();
    }
    let found = crate::found_in(request.input);
    request.tools.iter().filter(|tool| tool.loading != Loading::Found || found.contains(tool.name.as_str())).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{Event, Item};
    use serde_json::json;
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
            // Made by a process of its own: a file open for writing here, handed
            // to a child another test is forking just then, can't be run
            // ("Text file busy", on Linux).
            let text = dir.join("claude.txt");
            std::fs::write(&text, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&text, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(std::process::Command::new("cp").arg(&text).arg(&program).status().unwrap().success());
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
            hold_back: false,
            images: false,
        }
    }

    /// A backend that starts `fake` for its sessions.
    fn backend(fake: &Fake, token: Option<&str>) -> Backend {
        Backend { token: token.map(str::to_owned), launcher: live::Launcher::Cli(fake.program.clone()), sessions: live::Sessions::default() }
    }

    /// Every event of the first step of a conversation that says hello.
    async fn hello(backend: &Backend, idle: Duration) -> Result<Vec<Result<Event>>> {
        let input = [Item::User("hello".into())];
        let request = Request { input: &input, ..request() };
        let mut turn = backend.respond(&request, idle).await?;
        let mut events = Vec::new();
        while let Some(event) = turn.next().await {
            events.push(event);
        }
        Ok(events)
    }

    /// A step's lines as Claude Code prints them: a message of `text`, its
    /// usage, and the end of the turn.
    const ANSWERED: &str = r#"
printf '%s\n' '{"type":"system","subtype":"init"}'
printf '%s\n' '{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}},"parent_tool_use_id":null}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Hello from CLI"}]},"parent_tool_use_id":null}'
printf '%s\n' '{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}},"parent_tool_use_id":null}'
printf '%s\n' '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Hello from CLI"}'
"#;

    #[test]
    fn a_held_back_tool_is_offered_once_the_search_finds_it() {
        use crate::testing::*;
        let tools = held_back_tools();
        let names = |request: &Request<'_>| shown(request).iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
        let before = [Item::User("file an issue".into())];
        assert_eq!(names(&request(&before, &tools, true)), ["read", crate::TOOL_SEARCH]);
        let after = found_and_called();
        assert_eq!(names(&request(&after, &tools, true)), ["read", "mcp__linear__create_issue", crate::TOOL_SEARCH]);
        assert_eq!(names(&request(&before, &tools, false)), ["read", "mcp__linear__create_issue"], "nothing held back: all, no search");
    }

    #[tokio::test]
    async fn the_cli_gets_its_token_on_a_pipe_reins_tools_and_none_of_its_own() {
        // The token comes on a descriptor, never in the environment, which
        // other processes can read. Reading to the end proves rein's write
        // end didn't leak into the CLI: `cat` would wait on it for good.
        let fake = Fake::new(&format!(
            r#"
[ -z "${{CLAUDE_CODE_OAUTH_TOKEN+set}}" ] || exit 12
[ "$(/bin/cat "/dev/fd/$CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR")" = test-secret ] || exit 10
[ -z "$ANTHROPIC_API_KEY$ANTHROPIC_AUTH_TOKEN$ANTHROPIC_BASE_URL$OPENAI_API_KEY$CLAUDE_CODE_USE_BEDROCK" ] || exit 11
printf '%s\n' "$@" > "$0.args"
printf '%s' "$MCP_TOOL_TIMEOUT" > "$0.timeout"
while [ $# -gt 0 ]; do [ "$1" = --mcp-config ] && /bin/cp "$2" "$0.mcp" && /bin/ls -l "$2" > "$0.mcpmode"; shift; done
read -r line; printf '%s' "$line" > "$0.input"
{ANSWERED}
/bin/cat > /dev/null"#
        ));
        let backend = backend(&fake, Some("test-secret"));
        let events = hello(&backend, Duration::from_secs(5)).await.unwrap();
        let read = |what: &str| std::fs::read_to_string(format!("{}.{what}", fake.program)).unwrap();
        let args = read("args");
        for flag in [
            "-p\n",
            "--input-format\nstream-json\n",
            "--include-partial-messages\n",
            "--replay-user-messages\n",
            "--tools\n\n",
            "--strict-mcp-config\n",
            "--allowedTools\nmcp__rein\n",
            "--disable-slash-commands\n",
            "--settings\n{\"disableAllHooks\":true}\n",
            "--no-session-persistence\n",
            "--setting-sources\nuser\n",
            "--model\nsonnet\n",
        ] {
            assert!(args.contains(flag), "{flag:?} in {args}");
        }
        let appended = format!("--append-system-prompt\n{}\n\nrein rules\n", live::FOLDER);
        assert!(args.contains(&appended), "where the project is, then rein's instructions: {args}");
        // `--safe-mode` turns off every MCP server, rein's too; `--bare`
        // reads no sign-in.
        for flag in ["--safe-mode", "--bare", "--json-schema"] {
            assert!(!args.contains(flag), "{flag} in {args}");
        }
        assert_eq!(read("timeout"), "86400000", "a call waits as long as rein does");
        let mcp: Value = serde_json::from_str(&read("mcp")).unwrap();
        let rein = &mcp["mcpServers"]["rein"];
        assert!(rein["url"].as_str().unwrap().starts_with("http://127.0.0.1:"), "{mcp}");
        assert!(rein["headers"]["Authorization"].as_str().unwrap().starts_with("Bearer "), "{mcp}");
        assert!(read("mcpmode").starts_with("-rw-------"), "{}", read("mcpmode"));
        let told: Value = serde_json::from_str(&read("input")).unwrap();
        assert_eq!(told["message"]["content"], json!([{ "type": "text", "text": "hello" }]));
        assert!(matches!(&events[0], Ok(Event::TextDelta(text)) if text == "Hello from CLI"), "a block that came whole shows whole: {events:?}");
        assert!(matches!(&events[1], Ok(Event::Item(Item::Assistant(text))) if text == "Hello from CLI"), "{events:?}");
        assert!(matches!(events.last(), Some(Ok(Event::Completed { usage })) if usage.input_tokens == 9 && usage.output_tokens == 5 && usage.cached_input_tokens == 3));
        assert!(!format!("{backend:?}").contains("test-secret"));
    }

    #[tokio::test]
    async fn a_search_gives_the_cli_its_web_search_alone_and_reads_what_it_found() {
        let fake = Fake::new(
            r#"
printf '%s\n' "$@" > "$0.args"
/bin/cat > "$0.input"
printf '%s\n' '{"type":"result","structured_output":{"answer":"Rust 1.99.1","sources":[{"url":"https://blog.rust-lang.org/","title":"Rust Blog"}]}}'
"#,
        );
        let found = Backend::new(None).search_with(&fake.program, "haiku", "latest rust", Duration::from_secs(5)).await.unwrap();
        assert_eq!((found.answer.as_str(), found.sources[0].url.as_str()), ("Rust 1.99.1", "https://blog.rust-lang.org/"));
        let args = std::fs::read_to_string(format!("{}.args", fake.program)).unwrap();
        for flag in ["--tools\nWebSearch\n", "--allowedTools\nWebSearch\n", "--model\nhaiku\n", "--strict-mcp-config", "--safe-mode", "--setting-sources\nuser\n"] {
            assert!(args.contains(flag), "{flag:?} in {args}");
        }
        assert!(std::fs::read_to_string(format!("{}.input", fake.program)).unwrap().ends_with("The question: latest rust"));
    }

    #[tokio::test]
    async fn the_cli_runs_in_an_empty_private_folder_on_the_users_settings_alone() {
        // A cloned project's `.claude/settings.json` could send the token
        // elsewhere or run a command: the CLI never runs in it, nor reads
        // a project's settings.
        let fake = Fake::new(&format!(
            r#"
pwd -P > "$0.pwd"
/bin/ls -A > "$0.ls"
/bin/ls -ld . > "$0.mode"
printf '%s\n' "$@" > "$0.args"
[ -z "${{CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR+set}}" ] || exit 12
read -r line
{ANSWERED}
/bin/cat > /dev/null"#
        ));
        let backend = backend(&fake, None);
        hello(&backend, Duration::from_secs(5)).await.unwrap();
        let read = |what: &str| std::fs::read_to_string(format!("{}.{what}", fake.program)).unwrap();
        let (pwd, project) = (read("pwd"), std::env::current_dir().unwrap().canonicalize().unwrap());
        let pwd = std::path::Path::new(pwd.trim_end());
        assert!(!pwd.starts_with(&project), "ran in rein's folder: {}", pwd.display());
        assert_eq!(read("ls"), "mcp.json\n", "nothing in the folder but where rein's tools are");
        assert!(read("mode").starts_with("drwx------"), "{}", read("mode"));
        assert!(read("args").contains("--setting-sources\nuser\n"), "{}", read("args"));
        assert!(pwd.exists(), "the session goes on");
        drop(backend);
        assert!(!pwd.exists(), "the folder goes with the session");
    }

    #[tokio::test]
    async fn a_token_too_long_for_a_pipe_is_refused_before_the_cli_starts() {
        let fake = Fake::new(": > \"$0.ran\"");
        let token = "x".repeat(LONGEST_TOKEN + 1);
        let err = hello(&backend(&fake, Some(&token)), Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(&err, Error::Auth(message) if !message.contains(&token)), "{err}");
        assert!(!std::path::Path::new(&format!("{}.ran", fake.program)).exists());
    }

    #[tokio::test]
    async fn the_cli_failing_or_saying_nonsense_echoes_no_secret() {
        for body in [
            "echo secret >&2; exit 1",
            "read -r line; echo '{\"type\":\"result\",\"is_error\":true,\"result\":\"secret\"}'; /bin/cat > /dev/null",
            "read -r line; echo '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"result\":\"secret\"}'; /bin/cat > /dev/null",
            "read -r line; echo secret",
        ] {
            let fake = Fake::new(body);
            let events = hello(&backend(&fake, None), Duration::from_secs(5)).await.unwrap();
            let err = events.into_iter().find_map(Result::err).expect("the step failed");
            assert!(!err.to_string().contains("secret"), "{err}");
        }
    }

    /// What Claude Code 2.1.318 printed of a Max plan at 28% of its five
    /// hours and 75% of its week.
    const WARNED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1793901600,"rateLimitType":"seven_day","utilization":0.75,"overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"surpassedThreshold":0.75,"unifiedWindows":{"five_hour":{"utilization":0.28,"resetsAt":1793888400,"status":"allowed"},"seven_day":{"utilization":0.75,"resetsAt":1793901600,"status":"allowed_warning","surpassedThreshold":0.75}},"overageEnabled":false},"uuid":"u","session_id":"s"}"#;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[tokio::test]
    async fn a_plans_limits_come_as_the_cli_says_them() {
        let fake = Fake::new(&format!("read -r line\nprintf '%s\\n' '{WARNED}'\n{ANSWERED}\n/bin/cat > /dev/null"));
        let events = hello(&backend(&fake, None), Duration::from_secs(5)).await.unwrap();
        let Some(Ok(Event::Meters(meters))) = events.first() else { panic!("{events:?}") };
        let read: Vec<_> = meters.iter().map(|m| (m.label(), m.reading, m.standing, m.resets)).collect();
        assert_eq!(read, [("5h".into(), Reading::Used(28), Standing::Fine, at(1_793_888_400)), ("7d".into(), Reading::Used(75), Standing::Warned, at(1_793_901_600))]);
        assert!(matches!(&events[2], Ok(Event::Item(Item::Assistant(text))) if text == "Hello from CLI"), "then the answer, as ever");
    }

    #[test]
    fn a_window_is_read_by_the_length_in_its_name_and_what_isnt_a_window_is_left_out() {
        let one = |info: Value| plan_meters(&info).into_iter().map(|m| (m.label(), m.reading, m.standing)).collect::<Vec<_>>();
        // Without `unifiedWindows`, the one window it's about, as the SDK's types have it.
        let single = json!({"status": "allowed_warning", "rateLimitType": "five_hour", "utilization": 0.9, "resetsAt": 1_793_888_400});
        assert_eq!(one(single), [("5h".into(), Reading::Used(90), Standing::Warned)]);
        let opus = json!({"rateLimitType": "seven_day_opus", "utilization": 0.5, "status": "allowed"});
        assert_eq!(one(opus), [("7d Opus".into(), Reading::Used(50), Standing::Fine)]);
        let monthly = json!({"unifiedWindows": {"thirty_day": {"utilization": 0.1}}});
        assert_eq!(one(monthly), [("mo".into(), Reading::Used(10), Standing::Fine)], "a window not seen before shows too");
        assert!(one(json!({"rateLimitType": "overage", "utilization": 0.5})).is_empty(), "overage is money past the plan");
        assert!(one(json!({"rateLimitType": "seven_day_overage_included", "utilization": 0.5})).is_empty());
        assert!(one(json!({"rateLimitType": "five_hour", "utilization": "lots"})).is_empty(), "what can't be read isn't guessed");
        assert!(one(json!({"rateLimitType": "five_hour"})).is_empty(), "no reading, no meter");
        assert!(one(json!("nonsense")).is_empty());
        let percent = json!({"rateLimitType": "five_hour", "utilization": 42.0, "status": "rejected"});
        assert_eq!(one(percent), [("5h".into(), Reading::Used(42), Standing::Reached)], "one over 1 is a percent, and the CLI's word on it stands");
    }

    #[tokio::test]
    async fn a_reached_limit_is_said_as_the_plans_not_as_a_sign_in_to_check() {
        let reached = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day","utilization":1.0,"resetsAt":1793901600}}"#;
        let refused = r#"{"type":"result","is_error":true,"api_error_status":429,"result":"Weekly limit hit"}"#;
        let fake = Fake::new(&format!("read -r line\nprintf '%s\\n' '{reached}' '{refused}'\n/bin/cat > /dev/null"));
        let events = hello(&backend(&fake, None), Duration::from_secs(5)).await.unwrap();
        assert!(matches!(events.first(), Some(Ok(Event::Meters(meters))) if meters[0].standing == Standing::Reached), "{events:?}");
        let err = events.into_iter().find_map(Result::err).expect("the step failed");
        assert_eq!(err.kind(), Some(crate::ErrorKind::UsageLimit), "{err}");
        assert!(err.to_string().contains("Claude plan's 7d limit is reached"), "{err}");
        assert!(!err.retryable(), "waiting a few seconds won't help");
        // Served past a reached window, from overage say, it's an answer.
        let fake = Fake::new(&format!("read -r line\nprintf '%s\\n' '{reached}'\n{ANSWERED}\n/bin/cat > /dev/null"));
        let events = hello(&backend(&fake, None), Duration::from_secs(5)).await.unwrap();
        assert!(events.iter().all(Result::is_ok), "{events:?}");
    }

    #[tokio::test]
    async fn a_quiet_cli_times_out_and_output_is_capped() {
        let fake = Fake::new("read -r line; /bin/sleep 30");
        let start = std::time::Instant::now();
        let events = hello(&backend(&fake, None), Duration::from_millis(100)).await.unwrap();
        assert!(matches!(events.last(), Some(Err(Error::Incomplete(_)))), "{events:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
        let bytes = vec![0; 16 * 1024 * 1024 + 1];
        assert!(read_capped(bytes.as_slice(), Duration::from_secs(1)).await.is_err());
    }
}
