//! Claude Code as a subprocess, not an HTTP provider. Built-in tools and
//! customizations are disabled; structured replies ask rein to run its tools.

use std::collections::{BTreeMap, VecDeque};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::meter::{Meter, Reading, Standing};
use crate::{Error, Event, Item, Loading, Request, Result, Tool, ToolCall, Usage, found_text};

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

    /// The CLI's answer to `request`, after what it said of your plan's
    /// limits: those come even when the answer is that a limit is reached.
    async fn run(&self, program: &str, request: &Request<'_>, idle: Duration) -> Result<VecDeque<Result<Event>>> {
        let images = shown_images(request);
        let job = Job { model: request.model, effort: request.reasoning_effort, tool: None, schema: schema(request), prompt: prompt(request).to_string(), images };
        let printed = self.exec(program, &job, idle).await?;
        let mut events = VecDeque::new();
        if !printed.meters.is_empty() {
            events.push_back(Ok(Event::Meters(printed.meters.clone())));
        }
        match printed.answer().and_then(|result| decode(result, request)) {
            Ok(answer) => events.extend(answer),
            Err(e) => events.push_back(Err(e)),
        }
        Ok(events)
    }

    /// Searches the web for `question` with `model` and Claude Code's own
    /// WebSearch, the one tool it's given: its answer and the sources it
    /// names, in the shape asked for.
    pub(crate) async fn search(&self, model: &str, question: &str, idle: Duration) -> Result<crate::search::Searched> {
        self.search_with("claude", model, question, idle).await
    }

    async fn search_with(&self, program: &str, model: &str, question: &str, idle: Duration) -> Result<crate::search::Searched> {
        let job = Job { model, effort: None, tool: Some("WebSearch"), schema: crate::search::schema(), prompt: crate::search::asked(question), images: Vec::new() };
        crate::search::from_cli(self.exec(program, &job, idle).await?.answer()?)
    }

    /// Runs the CLI for `job`: what it printed, read, however it ended.
    async fn exec(&self, program: &str, job: &Job<'_>, idle: Duration) -> Result<Printed> {
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
            "--tools",
            job.tool.unwrap_or_default(),
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
        // turned down: the one it's given is allowed.
        if let Some(tool) = job.tool {
            command.args(["--allowedTools", tool]);
        }
        // Streamed, the answer comes with what the CLI says of your plan's
        // limits, which `json` leaves out. Images go only in streamed JSON.
        if !job.images.is_empty() {
            command.args(["--input-format", "stream-json"]);
        }
        command.args(["--output-format", "stream-json", "--verbose"]);
        if let Some(effort) = job.effort {
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
        let input = if job.images.is_empty() { job.prompt.clone() } else { streamed(&job.prompt, &job.images) };
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

/// One run of the CLI: the model, its effort, the one tool of its own it
/// may use (none for a step of rein's conversation, whose tools rein runs),
/// the shape of its answer, and what it's asked.
struct Job<'a> {
    model: &'a str,
    effort: Option<crate::Effort>,
    tool: Option<&'a str>,
    schema: Value,
    prompt: String,
    /// The images it's shown, after the prompt, in order.
    images: Vec<&'a crate::Image>,
}

/// The prompt and its images as one streamed user message: the images
/// each after what's said of it.
fn streamed(prompt: &str, images: &[&crate::Image]) -> String {
    let mut content = vec![json!({ "type": "text", "text": prompt })];
    for (n, image) in images.iter().enumerate() {
        content.push(json!({ "type": "text", "text": format!("Image {}: {}", n + 1, image.said()) }));
        content.push(json!({ "type": "image", "source": { "type": "base64", "media_type": image.kind.media_type(), "data": image.base64() } }));
    }
    format!("{}\n", json!({ "type": "user", "message": { "role": "user", "content": content }, "parent_tool_use_id": null }))
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

/// The images `request` shows, in the order they come.
fn shown_images<'a>(request: &'a Request<'_>) -> Vec<&'a crate::Image> {
    if !request.images {
        return Vec::new();
    }
    request
        .input
        .iter()
        .filter_map(|item| match item {
            Item::Image { image, .. } if image.data.is_some() => Some(image),
            _ => None,
        })
        .collect()
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

fn schema(request: &Request<'_>) -> Value {
    let calls: Vec<Value> = shown(request)
        .into_iter()
        .map(|tool| {
            // A schema that isn't kept to strict mode's rules, as an MCP
            // server's, is said in the prompt and not held to here: the CLI
            // turned a whole answer away for a null it didn't allow, and the
            // model gave up on the call. Its server says what's wrong.
            let arguments = if tool.strict { tool.parameters.clone() } else { json!({"type":"object"}) };
            json!({
                "type": "object", "properties": {
                    "name": {"type":"string", "enum":[tool.name]}, "arguments": arguments
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
    let images = crate::images_of(request.input);
    // The images shown come after the conversation, numbered in order.
    let mut numbered = 0;
    let mut history: Vec<Value> = Vec::new();
    for item in request.input {
        history.push(match item {
            Item::User(text) => json!({"role":"user", "text":text}),
            Item::Assistant(text) => json!({"role":"assistant", "text":text}),
            Item::ToolCall(call) => json!({"role":"assistant", "call_id":call.call_id, "name":call.name, "arguments":call.arguments}),
            Item::ToolResult { call_id, output } => {
                let (mut text, shown) = crate::split_images(output, images.get(call_id.as_str()).map(Vec::as_slice).unwrap_or_default(), request.images);
                for image in shown {
                    numbered += 1;
                    text.push_str(&format!("\n{} (image {numbered}, after the conversation)", image.said()));
                }
                json!({"role":"tool", "call_id":call_id, "output":text})
            }
            Item::Found { call_id, names } => json!({"role":"tool", "call_id":call_id, "output":found_text(names)}),
            Item::Opaque(_) | Item::Image { .. } => continue,
        });
    }
    let tools: Vec<Value> = shown(request).into_iter().map(|t| json!({"name":t.name, "description":t.description, "parameters":t.parameters})).collect();
    json!({"task":TASK, "instructions":request.instructions, "conversation":history, "tools":tools})
}

/// What the CLI is asked to do. It has no tools of its own but the one it
/// answers with, and says so: told only not to run rein's tools, the model
/// called a server's (`mcp__linear__list_teams`) as its own, was told
/// there's no such tool, and often gave up on it.
const TASK: &str = "Continue this rein conversation, following its instructions. You have one tool of your own, \
                    StructuredOutput: answer only by calling it. The tools listed under \"tools\" are rein's, not \
                    yours: you can't call them yourself, and calling one by its name fails. To use one, put its name \
                    and arguments in StructuredOutput's tool_calls; rein runs it and gives you its result in the next \
                    request. Return an empty tool_calls array when you're done.";

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

fn decode(value: &Value, request: &Request<'_>) -> Result<VecDeque<Result<Event>>> {
    let reply: Reply = serde_json::from_value(value["structured_output"].clone()).map_err(|_| Error::Incomplete("Claude Code returned no valid structured reply".into()))?;
    let mut events = VecDeque::new();
    if !reply.text.is_empty() {
        events.push_back(Ok(Event::TextDelta(reply.text.clone())));
        events.push_back(Ok(Event::Item(Item::Assistant(reply.text))));
    }
    for call in reply.tool_calls {
        if !shown(request).iter().any(|tool| tool.name == call.name) {
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
            hold_back: false,
            images: false,
        }
    }

    #[test]
    fn the_cli_is_told_rein_runs_the_tools_and_held_only_to_strict_schemas() {
        let strict = json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false});
        let server = json!({"type": "object", "properties": {"query": {"type": "string"}}});
        let tools = [
            crate::Tool { loading: Loading::UpFront, strict: true, name: "read".into(), description: "Read".into(), parameters: strict.clone() },
            crate::Tool { loading: Loading::UpFront, strict: false, name: "mcp__linear__list_teams".into(), description: "Teams".into(), parameters: server.clone() },
        ];
        let request = Request { tools: &tools, ..request() };
        let calls = &schema(&request)["properties"]["tool_calls"]["items"]["anyOf"];
        assert_eq!(calls[0]["properties"]["arguments"], strict, "rein's own: held to it");
        assert_eq!(calls[1]["properties"]["arguments"], json!({"type": "object"}), "a server's: its server checks it");
        let prompt = prompt(&request);
        assert_eq!(prompt["tools"][1]["parameters"], server, "and the model is still told it");
        assert!(prompt["task"].as_str().unwrap().contains("You have one tool of your own, StructuredOutput"), "{}", prompt["task"]);
        assert!(prompt["task"].as_str().unwrap().contains("you can't call them yourself"));
    }

    #[tokio::test]
    async fn images_go_to_the_cli_streamed_after_the_conversation() {
        let fake = Fake::new(
            r#"
printf '%s\n' "$@" > "$0.args"
/bin/cat > "$0.input"
printf '%s\n' '{"type":"system","subtype":"init"}'
printf '%s\n' '{"type":"result","structured_output":{"text":"A dot.","tool_calls":[]},"usage":{"input_tokens":1,"output_tokens":1}}'
"#,
        );
        let input = crate::testing::pictured();
        let request = crate::testing::request(&input, &[], false);
        let events = Backend::new(None).run(&fake.program, &request, Duration::from_secs(5)).await.unwrap();
        assert!(matches!(events.front(), Some(Ok(Event::TextDelta(text))) if text == "A dot."), "read from the streamed result line");
        let args = std::fs::read_to_string(format!("{}.args", fake.program)).unwrap();
        assert!(args.contains("--input-format\nstream-json\n--output-format\nstream-json\n--verbose\n"), "{args}");
        let sent: Value = serde_json::from_str(std::fs::read_to_string(format!("{}.input", fake.program)).unwrap().trim()).unwrap();
        let content = &sent["message"]["content"];
        let prompt: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(prompt["conversation"][2]["output"], "Image a.png, 2×1.\n[image a.png, 2×1] (image 1, after the conversation)");
        assert_eq!(content[1]["text"], "Image 1: [image a.png, 2×1]");
        assert_eq!(content[2]["source"]["data"], "cG5nIQ==");
        // With no image to show, only what it answers is streamed.
        let blind = Request { images: false, ..request };
        let _ = Backend::new(None).run(&fake.program, &blind, Duration::from_secs(5)).await;
        let args = std::fs::read_to_string(format!("{}.args", fake.program)).unwrap();
        assert!(args.contains("--output-format\nstream-json\n--verbose\n") && !args.contains("--input-format"), "{args}");
    }

    #[test]
    fn a_held_back_tool_is_told_of_once_the_search_finds_it() {
        use crate::testing::*;
        let tools = held_back_tools();
        let names = |request: &Request<'_>| shown(request).iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
        let before = [Item::User("file an issue".into())];
        assert_eq!(names(&request(&before, &tools, true)), ["read", crate::TOOL_SEARCH]);
        let after = found_and_called();
        assert_eq!(names(&request(&after, &tools, true)), ["read", "mcp__linear__create_issue", crate::TOOL_SEARCH]);
        let prompt = prompt(&request(&after, &tools, true));
        assert_eq!(prompt["conversation"][2]["output"], "Found: mcp__linear__create_issue. Call them as any other tool.");
        assert_eq!(schema(&request(&before, &tools, true))["properties"]["tool_calls"]["items"]["anyOf"].as_array().unwrap().len(), 2, "only what it's told of, it can call");
        assert_eq!(names(&request(&before, &tools, false)), ["read", "mcp__linear__create_issue"], "nothing held back: all, no search");
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
printf '%s\n' '{"type":"result","structured_output":{"text":"Hello from CLI","tool_calls":[]},"usage":{"input_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4,"output_tokens":5}}'
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
        // A step of rein's conversation still gets no tool of the CLI's own.
        let _ = Backend::new(None).run(&fake.program, &request(), Duration::from_secs(5)).await;
        let args = std::fs::read_to_string(format!("{}.args", fake.program)).unwrap();
        assert!(args.contains("--tools\n\n") && !args.contains("--allowedTools"), "{args}");
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
printf '%s\n' '{"type":"result","structured_output":{"text":"ok","tool_calls":[]}}'
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
        let tools = [crate::Tool {
            loading: crate::Loading::UpFront,
            strict: true,
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type":"object", "properties":{"path":{"type":"string"}}}),
        }];
        let input = [Item::User("read this".into()), Item::ToolResult { call_id: "old".into(), output: "contents".into() }];
        let request = Request { tools: &tools, input: &input, ..request() };
        let events = decode(&json!({"structured_output":{"text":"Checking","tool_calls":[{"name":"read","arguments":{"path":"a.rs"}}]}}), &request).unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(Event::Item(Item::ToolCall(call))) if call.name == "read" && call.arguments == r#"{"path":"a.rs"}"# && !call.call_id.is_empty()))
        );
        assert_eq!(schema(&request)["properties"]["tool_calls"]["items"]["anyOf"][0]["properties"]["name"]["enum"][0], "read");
        assert_eq!(prompt(&request)["conversation"][1]["output"], "contents");
        assert!(decode(&json!({"structured_output":{"text":"","tool_calls":[{"name":"bash","arguments":{}}]}}), &request).is_err());
    }

    #[tokio::test]
    async fn subprocess_failures_and_malformed_replies_do_not_echo_secrets() {
        for body in [
            "echo secret >&2; exit 1",
            "echo '{\"type\":\"result\",\"is_error\":true,\"result\":\"secret\"}'",
            "echo '{\"type\":\"result\",\"structured_output\":\"secret\"}'",
            "echo secret",
        ] {
            let fake = Fake::new(body);
            let err = failure(Backend::new(None).run(&fake.program, &request(), Duration::from_secs(5)).await);
            assert!(!err.to_string().contains("secret"), "{err}");
        }
    }

    /// The error a run ended in: before anything came, or as its answer.
    fn failure(run: Result<VecDeque<Result<Event>>>) -> Error {
        match run {
            Err(e) => e,
            Ok(events) => events.into_iter().find_map(Result::err).expect("the run failed"),
        }
    }

    /// What Claude Code 2.1.318 printed of a Max plan at 28% of its five
    /// hours and 75% of its week.
    const WARNED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1793901600,"rateLimitType":"seven_day","utilization":0.75,"overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"surpassedThreshold":0.75,"unifiedWindows":{"five_hour":{"utilization":0.28,"resetsAt":1793888400,"status":"allowed"},"seven_day":{"utilization":0.75,"resetsAt":1793901600,"status":"allowed_warning","surpassedThreshold":0.75}},"overageEnabled":false},"uuid":"u","session_id":"s"}"#;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[tokio::test]
    async fn a_plans_limits_come_before_its_answer() {
        let fake = Fake::new(&format!(
            "/bin/cat > /dev/null\nprintf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\"}}' '{WARNED}' '{{\"type\":\"result\",\"structured_output\":{{\"text\":\"hi\",\"tool_calls\":[]}}}}'"
        ));
        let events = Backend::new(None).run(&fake.program, &request(), Duration::from_secs(5)).await.unwrap();
        let Some(Ok(Event::Meters(meters))) = events.front() else { panic!("{events:?}") };
        let read: Vec<_> = meters.iter().map(|m| (m.label(), m.reading, m.standing, m.resets)).collect();
        assert_eq!(read, [("5h".into(), Reading::Used(28), Standing::Fine, at(1_793_888_400)), ("7d".into(), Reading::Used(75), Standing::Warned, at(1_793_901_600))]);
        assert!(matches!(&events[1], Ok(Event::TextDelta(text)) if text == "hi"), "then the answer, as ever");
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
        for exit in ["0", "1"] {
            let fake = Fake::new(&format!("/bin/cat > /dev/null\nprintf '%s\\n' '{reached}' '{refused}'\nexit {exit}"));
            let events = Backend::new(None).run(&fake.program, &request(), Duration::from_secs(5)).await.unwrap();
            assert!(matches!(events.front(), Some(Ok(Event::Meters(meters))) if meters[0].standing == Standing::Reached), "{events:?}");
            let err = failure(Ok(events));
            assert_eq!(err.kind(), Some(crate::ErrorKind::UsageLimit), "{err}");
            assert!(err.to_string().contains("Claude plan's 7d limit is reached"), "{err}");
            assert!(!err.retryable(), "waiting a few seconds won't help");
        }
        // Served past a reached window, from overage say, it's an answer.
        let served = r#"{"type":"result","structured_output":{"text":"still here","tool_calls":[]}}"#;
        let fake = Fake::new(&format!("/bin/cat > /dev/null\nprintf '%s\\n' '{reached}' '{served}'"));
        let events = Backend::new(None).run(&fake.program, &request(), Duration::from_secs(5)).await.unwrap();
        assert!(events.iter().all(Result::is_ok), "{events:?}");
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
