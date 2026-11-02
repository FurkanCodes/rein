//! rein as you see it: the real binary, in a terminal kept in memory
//! (`vt100`), talking to a model that follows a script. These tests type
//! keys, change the window's size and read the screen, with no network and
//! no tokens spent.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rein_llm::serde_json::{self, Value, json};

/// What the model does in one turn.
struct Turn {
    out: Vec<Out>,
}

enum Out {
    Text(String),
    /// A call of the tool named, with these arguments.
    Call(&'static str, Value),
}

fn says(text: &str) -> Turn {
    Turn { out: vec![Out::Text(text.to_owned())] }
}

/// A command that changes files, as the model asks for one.
fn runs(command: &str) -> Turn {
    calls("bash", json!({ "command": command, "timeout_seconds": null, "access": "write", "reason": null }))
}

fn calls(tool: &'static str, arguments: Value) -> Turn {
    Turn { out: vec![Out::Call(tool, arguments)] }
}

/// A model server: each request gets the next turn of the script, and
/// every request is kept, to check what rein sent.
struct Model {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    /// Each request's first line and headers, in the same order.
    heads: Arc<Mutex<Vec<String>>>,
    /// Whether each request came gzipped, those turned away included.
    gzipped: Arc<Mutex<Vec<bool>>>,
    /// What rein's environment says besides the defaults: the provider's
    /// key and URL, and settings.
    env: Vec<(String, String)>,
    /// Files in rein's home (`~/.config/rein`), like a providers file.
    home: Vec<(String, String)>,
    /// Requests turned away for having a field this server doesn't take.
    turned_away: Arc<Mutex<usize>>,
    /// Its model list takes a while, as a busy provider's can.
    slow_list: Arc<std::sync::atomic::AtomicBool>,
}

/// The wire format a model server speaks.
#[derive(Clone, Copy, PartialEq)]
enum Wire {
    /// OpenAI's Responses API.
    Responses,
    /// Anthropic's Messages API, with signed thinking in every turn.
    Messages,
    /// Chat Completions, as most other providers speak it.
    Chat,
    /// Gemini's own API, with thought signatures.
    Gemini,
}

impl Model {
    fn start(turns: Vec<Turn>) -> Model {
        Model::serve(turns, true, Wire::Responses, None)
    }

    /// A Chat Completions server set up as Groq in a providers file, that
    /// turns away requests with `rejects` in them, as servers do with
    /// fields they don't know.
    fn chat(turns: Vec<Turn>, rejects: Option<&'static str>) -> Model {
        let mut model = Model::serve(turns, true, Wire::Chat, rejects);
        model.home.push(("providers".into(), format!("[groq]\nurl = {}\n", model.base)));
        model.env.extend([("GROQ_API_KEY".into(), "test-key".into()), ("REIN_MODEL".into(), "groq/llama-test".into())]);
        model
    }

    /// A server that, like many a local one, can't unpack a compressed
    /// body, and says it's a bad request.
    fn without_gzip(turns: Vec<Turn>) -> Model {
        Model::serve(turns, false, Wire::Responses, None)
    }

    /// Claude, as Anthropic serves it, and rein set to use it.
    fn claude(turns: Vec<Turn>) -> Model {
        let mut model = Model::serve(turns, true, Wire::Messages, None);
        let base = model.base.clone();
        model.env.extend([("ANTHROPIC_API_KEY".into(), "test-key".into()), ("ANTHROPIC_BASE_URL".into(), base), ("REIN_MODEL".into(), "claude-opus-5-5".into())]);
        model
    }

    /// Gemini, as Google serves it, and rein set to use it.
    fn gemini(turns: Vec<Turn>) -> Model {
        let mut model = Model::serve(turns, true, Wire::Gemini, None);
        let base = model.base.clone();
        model.env.extend([("GEMINI_API_KEY".into(), "test-key".into()), ("GOOGLE_GEMINI_BASE_URL".into(), base), ("REIN_MODEL".into(), "gemini-3.8-flash".into())]);
        model
    }

    /// Its model list takes a second and a half from now on.
    fn with_slow_model_list(self) -> Model {
        self.slow_list.store(true, Ordering::Relaxed);
        self
    }

    /// Also sets `name` to `value` in rein's environment.
    fn with_env(mut self, name: &str, value: &str) -> Model {
        self.env.push((name.into(), value.into()));
        self
    }

    fn serve(turns: Vec<Turn>, takes_gzip: bool, wire: Wire, rejects: Option<&'static str>) -> Model {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}/v1", listener.local_addr().unwrap().port());
        let (requests, gzipped, heads) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        let (seen, how, heard) = (requests.clone(), gzipped.clone(), heads.clone());
        let turned_away = Arc::new(Mutex::new(0));
        let refused = turned_away.clone();
        let slow_list = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let slow = slow_list.clone();
        std::thread::spawn(move || {
            let mut turns = turns.into_iter();
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { break };
                let Some((head, body)) = read_request(&mut socket) else { continue };
                let reply = if head.starts_with("GET /v1/key ") {
                    // As OpenRouter does: only a good key gets an answer here.
                    let good = head.to_lowercase().contains("authorization: bearer good-key");
                    if good {
                        http(200, "application/json", br#"{"data":{"label":"test"}}"#)
                    } else {
                        http(401, "application/json", br#"{"error":{"message":"No auth credentials found","code":401}}"#)
                    }
                } else if head.starts_with("POST /v1/auth/keys ") {
                    // As OpenRouter does at the end of its browser sign-in.
                    heard.lock().unwrap().push(format!("{head}{}", String::from_utf8_lossy(&body)));
                    http(200, "application/json", br#"{"key":"good-key","user_id":"u1"}"#)
                } else if head.starts_with("GET") {
                    if slow.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1500));
                    }
                    let models = match wire {
                        Wire::Responses => json!({ "data": [{ "id": "test-model" }, { "id": "gpt-6.1-sol" }] }),
                        Wire::Messages => {
                            json!({ "data": [{ "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1_000_000, "max_tokens": 128_000 }], "has_more": false })
                        }
                        Wire::Chat => json!({ "object": "list", "data": [{ "id": "llama-test", "context_window": 131_072 }] }),
                        Wire::Gemini => {
                            json!({ "models": [{ "name": "models/gemini-3.8-flash", "inputTokenLimit": 1_048_576, "outputTokenLimit": 65_536, "supportedGenerationMethods": ["generateContent"] }] })
                        }
                    };
                    http(200, "application/json", models.to_string().as_bytes())
                } else {
                    let gzip = head.lines().any(|line| line.to_ascii_lowercase().starts_with("content-encoding:") && line.contains("gzip"));
                    how.lock().unwrap().push(gzip);
                    let unpacked = if gzip && takes_gzip { gunzip(&body) } else { body.clone() };
                    let rejected = rejects.filter(|field| String::from_utf8_lossy(&unpacked).contains(&format!("\"{field}\"")));
                    if gzip && !takes_gzip {
                        http(400, "application/json", br#"{"detail":"Bad Request"}"#)
                    } else if let Some(field) = rejected {
                        *refused.lock().unwrap() += 1;
                        let error = json!({ "error": { "message": format!("property '{field}' is unsupported"), "type": "invalid_request_error" } });
                        http(400, "application/json", error.to_string().as_bytes())
                    } else {
                        let body = unpacked;
                        let mut seen = seen.lock().unwrap();
                        seen.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                        heard.lock().unwrap().push(head.clone());
                        let turn = turns.next().unwrap_or_else(|| says("(the script has no more turns)"));
                        let stream = match wire {
                            Wire::Responses => sse(&turn),
                            // About what a tokenizer would count, mostly cached.
                            Wire::Messages => messages_sse(&turn, seen.len() - 1, body.len() as u64 / 4),
                            Wire::Chat => chat_sse(&turn),
                            Wire::Gemini => gemini_sse(&turn, seen.len() - 1),
                        };
                        http(200, "text/event-stream", stream.as_bytes())
                    }
                };
                let _ = socket.write_all(&reply);
            }
        });
        Model { base, requests, heads, gzipped, env: Vec::new(), home: Vec::new(), turned_away, slow_list }
    }

    fn turned_away(&self) -> usize {
        *self.turned_away.lock().unwrap()
    }

    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn gzipped(&self) -> Vec<bool> {
        self.gzipped.lock().unwrap().clone()
    }
}

fn gunzip(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body).read_to_end(&mut out).unwrap();
    out
}

fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let length = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|n| n.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
    while data.len() < head_end + length {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some((head, data[head_end..].to_vec()))
}

fn http(status: u16, kind: &str, body: &[u8]) -> Vec<u8> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let mut reply = format!("HTTP/1.1 {status} {reason}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
    reply.extend_from_slice(body);
    reply
}

/// A turn as the Responses API streams it.
fn sse(turn: &Turn) -> String {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let mut out = String::new();
    let mut event = |v: Value| out += &format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    for item in &turn.out {
        match item {
            Out::Text(text) => {
                for piece in text.split_inclusive(' ') {
                    event(json!({ "type": "response.output_text.delta", "delta": piece }));
                }
                let message = json!({ "type": "message", "content": [{ "type": "output_text", "text": text }] });
                event(json!({ "type": "response.output_item.done", "item": message }));
            }
            Out::Call(tool, arguments) => {
                let call_id = format!("call_{}", CALLS.fetch_add(1, Ordering::Relaxed));
                let call = json!({ "type": "function_call", "call_id": call_id, "namespace": "rein", "name": tool, "arguments": arguments.to_string() });
                event(json!({ "type": "response.output_item.done", "item": call }));
            }
        }
    }
    let usage = json!({ "input_tokens": 1200, "input_tokens_details": { "cached_tokens": 1000 }, "output_tokens": 20, "output_tokens_details": { "reasoning_tokens": 0 } });
    event(json!({ "type": "response.completed", "response": { "usage": usage } }));
    out
}

/// A turn as the Messages API streams it: thinking first, signed
/// `sig-<n>` for the `n`th request, so a test can tell which request's
/// answer each thinking block came from.
fn messages_sse(turn: &Turn, n: usize, tokens: u64) -> String {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let mut out = String::new();
    let mut event = |v: Value| out += &format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    let usage = json!({ "input_tokens": tokens / 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": tokens - tokens / 10, "output_tokens": 1 });
    event(json!({ "type": "message_start", "message": { "model": "claude-opus-5-5", "usage": usage } }));
    event(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }));
    event(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "**Working it out**" } }));
    event(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": format!("sig-{n}") } }));
    event(json!({ "type": "content_block_stop", "index": 0 }));
    let mut called = false;
    for (i, item) in turn.out.iter().enumerate() {
        let index = i + 1;
        match item {
            Out::Text(text) => {
                event(json!({ "type": "content_block_start", "index": index, "content_block": { "type": "text", "text": "" } }));
                for piece in text.split_inclusive(' ') {
                    event(json!({ "type": "content_block_delta", "index": index, "delta": { "type": "text_delta", "text": piece } }));
                }
            }
            Out::Call(tool, arguments) => {
                called = true;
                let id = format!("toolu_{}", CALLS.fetch_add(1, Ordering::Relaxed));
                event(json!({ "type": "content_block_start", "index": index, "content_block": { "type": "tool_use", "id": id, "name": tool, "input": {} } }));
                let json = arguments.to_string();
                let (a, b) = json.split_at(json.len() / 2);
                for part in [a, b] {
                    event(json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": part } }));
                }
            }
        }
        event(json!({ "type": "content_block_stop", "index": index }));
    }
    let reason = if called { "tool_use" } else { "end_turn" };
    event(json!({ "type": "message_delta", "delta": { "stop_reason": reason }, "usage": { "output_tokens": 20 } }));
    event(json!({ "type": "message_stop" }));
    out
}

/// A turn as Chat Completions streams it: reasoning, the text in pieces,
/// calls in pieces, then the token counts and `[DONE]`.
fn chat_sse(turn: &Turn) -> String {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let mut out = String::new();
    let mut chunk = |delta: Value, finish: Value| out += &format!("data: {}\n\n", json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] }));
    chunk(json!({ "role": "assistant", "reasoning_content": "**Thinking it over**" }), Value::Null);
    let mut called = 0;
    for item in &turn.out {
        match item {
            Out::Text(text) => {
                for piece in text.split_inclusive(' ') {
                    chunk(json!({ "content": piece }), Value::Null);
                }
            }
            Out::Call(tool, arguments) => {
                let id = format!("call_{}", CALLS.fetch_add(1, Ordering::Relaxed));
                let json = arguments.to_string();
                let (a, b) = json.split_at(json.len() / 2);
                chunk(json!({ "tool_calls": [{ "index": called, "id": id, "type": "function", "function": { "name": tool, "arguments": a } }] }), Value::Null);
                chunk(json!({ "tool_calls": [{ "index": called, "function": { "arguments": b } }] }), Value::Null);
                called += 1;
            }
        }
    }
    chunk(json!({}), json!(if called > 0 { "tool_calls" } else { "stop" }));
    let usage = json!({ "prompt_tokens": 1200, "completion_tokens": 20, "prompt_tokens_details": { "cached_tokens": 600 } });
    out += &format!("data: {}\n\ndata: [DONE]\n\n", json!({ "choices": [], "usage": usage }));
    out
}

/// A turn as Gemini streams it: a thought, the text in pieces, and calls,
/// the first signed `gsig-<n>` for the `n`th request; the stream just ends.
fn gemini_sse(turn: &Turn, n: usize) -> String {
    let mut out = String::new();
    let mut chunk = |parts: Value, finish: Option<&str>| {
        let mut candidate = json!({ "content": { "role": "model", "parts": parts } });
        if let Some(finish) = finish {
            candidate["finishReason"] = json!(finish);
        }
        let usage = json!({ "promptTokenCount": 1000, "cachedContentTokenCount": 250, "candidatesTokenCount": 20, "thoughtsTokenCount": 10 });
        out += &format!("data: {}\r\n\r\n", json!({ "candidates": [candidate], "usageMetadata": usage }));
    };
    chunk(json!([{ "text": "**Pondering**", "thought": true }]), None);
    let mut signed = false;
    for item in &turn.out {
        match item {
            Out::Text(text) => {
                for piece in text.split_inclusive(' ') {
                    chunk(json!([{ "text": piece }]), None);
                }
            }
            Out::Call(tool, arguments) => {
                let mut part = json!({ "functionCall": { "name": tool, "args": arguments } });
                if !std::mem::replace(&mut signed, true) {
                    part["thoughtSignature"] = json!(format!("gsig-{n}"));
                }
                chunk(json!([part]), None);
            }
        }
    }
    chunk(json!([{ "text": "" }]), Some("STOP"));
    out
}

/// What the terminal was asked to do besides drawing.
#[derive(Default)]
struct Seen {
    bells: usize,
    /// What was put on the clipboard (OSC 52), decoded.
    copied: Vec<String>,
}

impl vt100::Callbacks for Seen {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], data: &[u8]) {
        self.copied.push(String::from_utf8(unbase64(data)).unwrap());
    }
}

fn unbase64(data: &[u8]) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let digits: Vec<u8> = data.iter().copied().filter(|&c| c != b'=').map(value).collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &d)| n | u32::from(d) << (18 - 6 * i));
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    out
}

/// rein running in a pseudo-terminal, read through a terminal emulator.
struct Term {
    screen: Arc<Mutex<vt100::Parser<Seen>>>,
    keyboard: File,
    child: Child,
    dir: PathBuf,
}

impl Term {
    fn start(model: &Model, name: &str, rows: u16, cols: u16) -> Term {
        Term::start_with(model, name, rows, cols, &[])
    }

    /// rein in a project that has `files` in it from the start.
    fn start_with(model: &Model, name: &str, rows: u16, cols: u16, files: &[(&str, &str)]) -> Term {
        let dir = std::env::temp_dir().join(format!("rein-screen-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let path = dir.join("project").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Term::spawn(model, dir, rows, cols)
    }

    /// rein again, in the same folder and with the same home, once this one
    /// has left.
    fn again(self, model: &Model) -> Term {
        self.again_with(model, &[])
    }

    /// rein again, as `again` does, started with `args`.
    fn again_with(mut self, model: &Model, args: &[&str]) -> Term {
        self.wait_exit();
        let dir = std::mem::take(&mut self.dir);
        let (rows, cols) = self.screen.lock().unwrap().screen().size();
        Term::spawn_with(model, dir, rows, cols, args)
    }

    fn spawn(model: &Model, dir: PathBuf, rows: u16, cols: u16) -> Term {
        Term::spawn_with(model, dir, rows, cols, &[])
    }

    fn spawn_with(model: &Model, dir: PathBuf, rows: u16, cols: u16, args: &[&str]) -> Term {
        let (project, home) = (dir.join("project"), dir.join("home"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        for (name, text) in &model.home {
            std::fs::create_dir_all(home.join(name).parent().unwrap()).unwrap();
            std::fs::write(home.join(name), text).unwrap();
        }

        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: openpty writes two descriptors into the ints we pass.
        let opened = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
        assert_eq!(opened, 0, "can't open a pseudo-terminal");
        // SAFETY: openpty just gave us these, and nothing else owns them.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

        let mut command = Command::new(env!("CARGO_BIN_EXE_rein"));
        command
            .args(args)
            .current_dir(&project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("REIN_HOME", &home)
            .env("TERM", "xterm-256color")
            .env("OPENAI_API_KEY", "test")
            .env("OPENAI_BASE_URL", &model.base)
            .envs(model.env.iter().map(|(name, value)| (name, value)))
            // Copies go to the terminal (OSC 52), never this computer's clipboard.
            .env("SSH_CONNECTION", "screen-test")
            // Links are shown, never opened in this computer's browser.
            .env("BROWSER", "false")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only calls that are safe between fork and exec.
        unsafe {
            command.pre_exec(|| {
                // Its own session, with this terminal as its own: so it gets
                // SIGWINCH when the window changes size.
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(command);

        let screen = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(rows, cols, 1000, Seen::default())));
        let shown = screen.clone();
        let mut output = File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            while let Ok(n @ 1..) = output.read(&mut buf) {
                shown.lock().unwrap().process(&buf[..n]);
            }
        });
        Term { screen, keyboard: File::from(master), child, dir }
    }

    fn keys(&mut self, keys: &str) {
        self.keyboard.write_all(keys.as_bytes()).unwrap();
    }

    /// The window changes size: the terminal first, then rein hears of it.
    fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.lock().unwrap().screen_mut().set_size(rows, cols);
        let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ reads the winsize we pass.
        unsafe { libc::ioctl(self.keyboard.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    }

    /// The screen's rows as text.
    fn rows(&self) -> Vec<String> {
        let parser = self.screen.lock().unwrap();
        let (_, cols) = parser.screen().size();
        parser.screen().rows(0, cols).collect()
    }

    fn text(&self) -> String {
        self.rows().join("\n")
    }

    /// Waits until the screen shows `text`; fails with the screen if it doesn't.
    fn wait_for(&self, text: &str) {
        self.wait_until(&format!("the screen to show {text:?}"), |screen| screen.contains(text));
    }

    fn wait_until(&self, what: &str, done: impl Fn(&str) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if done(&self.text()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}. The screen:\n{}", self.text());
    }

    fn bells(&self) -> usize {
        self.screen.lock().unwrap().callbacks().bells
    }

    fn wait_bells(&self, bells: usize) {
        let start = Instant::now();
        while self.bells() < bells && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(self.bells(), bells, "the bell rang {} times, not {bells}. The screen:\n{}", self.bells(), self.text());
    }

    /// The text in the input box's first line.
    fn in_box(&self) -> String {
        let rows = self.rows();
        let line = rows.iter().find(|row| row.starts_with("│ ")).cloned().unwrap_or_default();
        line.trim_start_matches("│ ").trim_end_matches('│').trim().trim_start_matches('›').trim().to_owned()
    }

    /// Waits for rein to exit.
    fn wait_exit(&mut self) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if self.child.try_wait().unwrap().is_some() {
                // Whatever it printed last is still on its way.
                std::thread::sleep(Duration::from_millis(100));
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("rein didn't exit. The screen:\n{}", self.text());
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !self.dir.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

const IDLE: &str = "type a message";

/// Long enough to wrap at any width these tests use.
const PARAGRAPH: &str = "A harness is everything around the model: the conversation, the tools, the checks before anything runs, and the screen you read it all on, laid out again whenever the window changes size.";

#[test]
fn an_answer_appears_above_the_box() {
    let model = Model::start(vec![says("Hello there, how can I help?")]);
    let mut term = Term::start(&model, "answer", 24, 80);
    term.wait_for(IDLE);
    term.keys("hi\r");
    term.wait_for("how can I help?");
    term.wait_for(IDLE);
    let screen = term.text();
    assert!(screen.contains("you    hi"), "{screen}");
    assert!(screen.contains("rein   Hello there, how can I help?"), "{screen}");
    assert!(screen.contains("context"), "the status bar is under the box:\n{screen}");
    assert!(model.requests()[0].to_string().contains("\"hi\""), "the model was sent the message");
    assert_eq!(model.gzipped(), [true], "the conversation went gzipped");
    assert_eq!(term.bells(), 0, "you were there: no bell");
    term.keys("/status\r");
    term.wait_for("fewer bytes)");
    let screen = term.text();
    let row = screen.lines().find(|row| row.contains("sent ")).unwrap_or_default();
    assert!(row.contains(" KB as ") && row.contains("(gzip, "), "{screen}");
}

#[test]
fn a_server_that_cant_unpack_gzip_gets_it_plain() {
    let model = Model::without_gzip(vec![says("Hello there."), says("Still here.")]);
    let mut term = Term::start(&model, "plain", 24, 90);
    term.wait_for(IDLE);
    term.keys("hi\r");
    term.wait_for("Hello there.");
    term.wait_for("(this server doesn't take compressed requests, so they go uncompressed)");
    term.wait_for(IDLE);
    term.keys("and again\r");
    term.wait_for("Still here.");
    assert_eq!(model.gzipped(), [true, false, false], "turned away once, then sent as it is");
    assert_eq!(model.requests().len(), 2, "both messages were answered");
    let notes = term.text().matches("doesn't take compressed").count();
    assert_eq!(notes, 1, "said once");
    term.wait_for(IDLE);
    term.keys("/status\r");
    term.wait_for("uncompressed: the server doesn't take compressed requests");
}

#[test]
fn a_command_is_asked_about_whole_and_runs_once_allowed() {
    let command = "echo one && echo two && echo three-four-five-six-seven-eight-nine-ten";
    let model = Model::start(vec![runs(command), says("All three lines printed.")]);
    let mut term = Term::start(&model, "approve", 24, 50);
    term.wait_for(IDLE);
    term.keys("go\r");
    term.wait_for("Allow this command");
    // The whole command is there to judge, though it's wider than the box.
    let screen = term.text();
    for word in command.split(' ') {
        assert!(screen.contains(word), "{word:?} isn't on screen:\n{screen}");
    }
    let command_rows = screen.lines().filter(|row| row.contains("echo") || row.contains("three-four"));
    assert!(command_rows.clone().all(|row| !row.contains('…')), "nothing of it is cut short:\n{screen}");
    // Each answer's key comes first, so a narrow box can't cut it off.
    for key in ["› y", "a    Yes, and", "n    No", "esc  No, and"] {
        assert!(screen.contains(key), "{key:?} isn't on screen:\n{screen}");
    }
    // Keys too soon after a question still go to the box.
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("All three lines printed.");
    let screen = term.text();
    assert!(screen.contains("● Run") && screen.contains("exit 0"), "{screen}");
    let sent = model.requests()[1].to_string();
    assert!(sent.contains("three-four-five-six-seven-eight-nine-ten"), "the output went back to the model: {sent}");
}

#[test]
fn a_narrower_window_rewraps_whole_words() {
    let model = Model::start(vec![says(PARAGRAPH)]);
    let mut term = Term::start(&model, "resize", 24, 100);
    term.wait_for(IDLE);
    term.keys("explain\r");
    term.wait_until("the whole answer", |screen| screen.contains("size.") && screen.contains(IDLE));
    for cols in [40, 63, 100, 31] {
        term.resize(24, cols);
        term.wait_until(&format!("the box at {cols} columns"), |screen| {
            // Drawn again whole: the old box, cut at the new edge, has no corner there.
            screen.lines().any(|row| row.starts_with('╭') && row.ends_with('╮') && row.chars().count() == usize::from(cols))
        });
        let rows = term.rows();
        let start = rows.iter().position(|row| row.starts_with("rein   ")).expect("the answer");
        let answer: Vec<&str> = rows[start..].iter().take_while(|row| !row.trim().is_empty()).map(|row| row.get(7..).unwrap_or("").trim()).collect();
        assert_eq!(answer.join(" "), PARAGRAPH, "at {cols} columns, every word is whole:\n{}", rows.join("\n"));
        assert!(answer.len() > 1 || cols == 100, "it wraps at {cols} columns");
        let boxes = rows.iter().filter(|row| row.contains('╭')).count();
        assert_eq!(boxes, 1, "one box, nothing left of an old one:\n{}", rows.join("\n"));
    }
}

#[test]
fn the_wheel_scrolls_back_and_pgdn_returns() {
    let lines: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    let model = Model::start(vec![says(&lines)]);
    let mut term = Term::start(&model, "scroll", 16, 60);
    term.wait_for(IDLE);
    term.keys("count\r");
    term.wait_for("line 40");
    assert!(!term.text().contains("line 20"));
    for _ in 0..5 {
        term.keys("\x1b[<64;10;5M");
    }
    // Five notches, three rows each.
    term.wait_for("15 more lines");
    let screen = term.text();
    assert!(screen.contains("line 20") && !screen.contains("line 40"), "scrolled back:\n{screen}");
    for _ in 0..3 {
        term.keys("\x1b[6~");
    }
    term.wait_until("the end of the conversation again", |screen| screen.contains("line 40") && !screen.contains("more lines"));
}

/// A turn with two steps planned at once: a slow one, then one that leaves a mark.
fn two_steps() -> Turn {
    Turn {
        out: vec![
            Out::Call("bash", json!({ "command": "sleep 1", "timeout_seconds": null, "access": "write", "reason": null })),
            Out::Call("bash", json!({ "command": "touch planned.txt", "timeout_seconds": null, "access": "write", "reason": null })),
        ],
    }
}

const ALLOWED: (&str, &str) = (".rein/permissions", "allow sleep\nallow touch\n");

#[test]
fn enter_while_rein_works_steers_it() {
    let model = Model::start(vec![two_steps(), says("Switching to the helper.")]);
    let mut term = Term::start_with(&model, "steer", 30, 90, &[ALLOWED]);
    term.wait_for(IDLE);
    term.keys("make the file\r");
    term.wait_for("Running sleep 1");
    term.keys("use the helper instead\r");
    term.wait_for("steering: use the helper instead · goes in at the next step");
    term.wait_for("Switching to the helper.");
    assert!(!term.dir.join("project/planned.txt").exists(), "the step planned before you wrote didn't run");
    // On screen as the model saw it: the step that ran, the one skipped,
    // your message, then what rein did about it.
    let screen = term.text();
    let at = |text: &str| screen.find(text).unwrap_or_else(|| panic!("{text:?} isn't on screen:\n{screen}"));
    assert!(at("sleep 1  exit 0") < at("touch planned.txt  skipped"), "{screen}");
    assert!(at("touch planned.txt  skipped") < at("you    use the helper instead"), "{screen}");
    assert!(at("you    use the helper instead") < at("rein   Switching to the helper."), "{screen}");
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one turn, steered");
    let input = requests[1]["input"].as_array().unwrap();
    assert_eq!(input.last().unwrap()["content"], "use the helper instead", "it went in last, after both results");
    assert!(input.iter().any(|item| item["output"].as_str().is_some_and(|out| out.starts_with("Not run: the user sent a message"))));
}

#[test]
fn tab_while_rein_works_waits_for_the_end_of_the_turn() {
    let model = Model::start(vec![two_steps(), says("Made it."), says("Committed.")]);
    let mut term = Term::start_with(&model, "queue", 30, 90, &[ALLOWED]);
    term.wait_for(IDLE);
    term.keys("make the file\r");
    term.wait_for("Running sleep 1");
    term.keys("and then commit it\t");
    term.wait_for("after this turn: and then commit it");
    term.wait_for("Committed.");
    assert!(term.dir.join("project/planned.txt").exists(), "every step ran");
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "the turn finished, then the message went as the next");
    assert!(!requests[1].to_string().contains("commit it"));
    assert_eq!(requests[2]["input"].as_array().unwrap().last().unwrap()["content"], "and then commit it");
}

#[test]
fn typing_a_slash_lists_the_commands() {
    let model = Model::start(Vec::new());
    let mut term = Term::start(&model, "slash", 24, 80);
    term.wait_for(IDLE);
    term.keys("/mo");
    term.wait_for("list the models");
    let screen = term.text();
    assert!(screen.contains("/mode") && screen.contains("/model"), "{screen}");
    assert!(!screen.contains("/status"), "only the ones that match:\n{screen}");
}

#[test]
fn leaving_leaves_the_conversation_in_the_terminal() {
    let model = Model::start(vec![says("Bye for now.")]);
    let mut term = Term::start(&model, "leave", 30, 80);
    term.wait_for(IDLE);
    term.keys("hello\r");
    term.wait_for("Bye for now.");
    term.wait_for(IDLE);
    term.keys("\x04");
    term.wait_exit();
    let screen = term.text();
    assert!(!screen.contains('╭'), "the box is gone:\n{screen}");
    assert!(screen.contains("you    hello") && screen.contains("rein   Bye for now."), "{screen}");
    assert!(screen.contains("rein -c continues this conversation"), "{screen}");
}

#[test]
fn what_you_sent_comes_back_in_the_next_run() {
    let model = Model::start(vec![says("On it."), says("Running them."), says("Fixed.")]);
    let mut term = Term::start(&model, "history", 24, 80);
    term.wait_for(IDLE);
    term.keys("fix the parser please\r");
    term.wait_for("On it.");
    term.wait_for(IDLE);
    term.keys("run the tests\r");
    term.wait_for("Running them.");
    term.wait_for(IDLE);
    term.keys("\x04");

    let mut term = term.again(&model);
    term.wait_for(IDLE);
    term.keys("\x1b[A");
    term.wait_until("the last message in the box", |_| term.in_box() == "run the tests");
    term.keys("\x1b[A");
    term.wait_until("the one before", |_| term.in_box() == "fix the parser please");
    term.keys("\x15");
    term.wait_for(IDLE);

    // Ctrl-R finds it by a word, wherever it is in the message.
    term.keys("\x12");
    term.wait_for("finding a message you sent");
    term.keys("pars");
    term.wait_until("only the message with the word", |screen| screen.contains("› fix the parser please") && !screen.contains("run the tests"));
    term.keys("\r");
    term.wait_until("it in the box", |screen| term.in_box() == "fix the parser please" && !screen.contains("finding"));
    assert_eq!(model.requests().len(), 2, "picked, not sent: nothing goes without you seeing it");
    term.keys("\r");
    term.wait_for("Fixed.");
    assert!(model.requests()[2].to_string().contains("fix the parser please"));
}

#[test]
fn copy_puts_the_last_answer_on_the_clipboard() {
    let model = Model::start(vec![says("It's a Rust workspace."), says("Run `cargo test --all`, then **commit**.")]);
    let mut term = Term::start(&model, "copy", 24, 80);
    term.wait_for(IDLE);
    term.keys("what is this\r");
    term.wait_for("Rust workspace.");
    term.wait_for(IDLE);
    term.keys("how do I check it\r");
    term.wait_for("then commit.");
    term.wait_for(IDLE);
    term.keys("/copy\r");
    // No clipboard command here: the terminal is asked, and rein says so.
    term.wait_for("asked the terminal to copy rein's last answer (1 line)");
    let copied = term.screen.lock().unwrap().callbacks().copied.clone();
    assert_eq!(copied, ["Run `cargo test --all`, then **commit**."], "as the model wrote it, Markdown and all");
}

#[test]
fn the_bell_rings_when_rein_needs_you_and_youre_away() {
    let model = Model::start(vec![runs("echo hi"), says("Done.")]);
    let mut term = Term::start(&model, "bell", 24, 80);
    term.wait_for(IDLE);
    // The window is in front: a question doesn't ring.
    term.keys("\x1b[I");
    term.keys("go\r");
    term.wait_for("Allow this command");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(term.bells(), 0, "you were looking");
    // You go to another window while it waits: it rings, once.
    term.keys("\x1b[O");
    term.wait_bells(1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(term.bells(), 1);
    term.keys("y");
    term.wait_for("Done.");
    // Done, and you're still away: your turn.
    term.wait_bells(2);
}

#[test]
fn a_search_runs_without_asking() {
    let grep = json!({ "pattern": r"fn parse_\w+", "path": null, "glob": "*.rs", "ignore_case": false, "output": "lines", "context": null });
    let model = Model::start(vec![calls("grep", grep), says("It's in src/parser.rs, line 2.")]);
    let mut term = Term::start(&model, "search", 24, 80);
    let project = term.dir.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/parser.rs"), "use x;\nfn parse_list() {}\n").unwrap();
    std::fs::write(project.join("notes.md"), "fn parse_list in prose\n").unwrap();
    term.wait_for(IDLE);
    term.keys("where is parse_list\r");
    term.wait_for("line 2.");
    let screen = term.text();
    assert!(screen.contains(r#"● Search "fn parse_\w+" (*.rs)  1 match in 1 file"#), "{screen}");
    assert!(screen.contains("└ src/parser.rs"), "{screen}");
    assert!(!screen.contains("Allow"), "a search is never asked about:\n{screen}");
    let tools = model.requests()[0]["tools"].to_string();
    assert!(tools.contains("\"grep\"") && tools.contains("\"glob\""), "the model is told about both: {tools}");
    let sent = model.requests()[1].to_string();
    assert!(sent.contains(r"1 match in 1 file\nsrc/parser.rs\n2:fn parse_list() {}"), "the model got the line: {sent}");
}

fn read(path: &str) -> Out {
    Out::Call("read", json!({ "path": path, "start_line": null, "line_count": null }))
}

#[test]
fn an_at_points_at_a_file_which_goes_with_the_message() {
    let model = Model::start(vec![says("It panics on an empty list.")]);
    let files = [("src/parser.rs", "fn parse() {}\nfn parse_list() {}\n"), ("src/main.rs", "fn main() {}\n"), ("README.md", "# x\n")];
    let mut term = Term::start_with(&model, "mention", 24, 80, &files);
    term.wait_for(IDLE);
    term.keys("why does @READ");
    term.wait_for("› README.md");
    term.keys("\x1b");
    term.wait_until("the list closed", |screen| !screen.contains("› README.md"));
    term.keys("\x15why does @pars");
    term.wait_for("› src/parser.rs");
    term.keys("\t");
    term.wait_until("the path in the box", |_| term.in_box() == "why does @src/parser.rs");
    term.keys("panic?\r");
    term.wait_for("empty list.");
    let screen = term.text();
    // Under your message, as yours: no label of rein's on its line.
    assert!(screen.contains("you    why does @src/parser.rs panic?\n       ● Read   src/parser.rs  2 lines"), "{screen}");
    assert!(screen.contains("rein   It panics on an empty list."), "{screen}");
    let input = model.requests()[0]["input"].to_string();
    assert!(input.contains(r#"fn parse() {}\nfn parse_list() {}\n"#), "the model got the file with the message: {input}");
}

#[test]
fn reads_in_a_row_share_a_line() {
    let files = [("a.txt", "1\n"), ("b.txt", "1\n2\n"), ("c.txt", "1\n2\n3\n")];
    let together = Turn { out: vec![read("a.txt"), read("b.txt")] };
    let model = Model::start(vec![together, Turn { out: vec![read("c.txt")] }, says("All read.")]);
    let mut term = Term::start_with(&model, "reads", 24, 90, &files);
    term.wait_for(IDLE);
    term.keys("read them\r");
    term.wait_for("All read.");
    let screen = term.text();
    assert!(screen.contains("● Read   a.txt, b.txt, c.txt  3 files · 6 lines"), "{screen}");
    assert_eq!(screen.matches("● Read").count(), 1, "one line for the three, asked for together or not:\n{screen}");
}

#[test]
fn claude_answers_through_anthropics_own_api() {
    let model = Model::claude(vec![says("Hello from Claude.")]);
    let mut term = Term::start(&model, "claude", 24, 90);
    term.wait_for(IDLE);
    term.keys("hi\r");
    term.wait_for("Hello from Claude.");
    term.wait_for(IDLE);
    let screen = term.text();
    assert!(screen.contains("claude-opus-5-5"), "the bar names the model:\n{screen}");
    assert!(screen.contains("✻ Working it out"), "its thinking shows:\n{screen}");
    assert!(screen.contains("cache 90%"), "Claude's cache counts, made rein's:\n{screen}");
    let head = model.heads()[0].to_lowercase();
    assert!(head.starts_with("post /v1/messages "), "{head}");
    assert!(head.contains("x-api-key: test-key") && head.contains("anthropic-version: 2026-10-21"), "{head}");
    let request = &model.requests()[0];
    assert_eq!(request["model"], "claude-opus-5-5");
    assert_eq!(request["thinking"], json!({ "type": "adaptive", "display": "summarized" }));
    assert_eq!(request["system"][0]["cache_control"], json!({ "type": "ephemeral" }), "the system prompt is cached");
}

/// `v` without its cache marks, which may move between requests: they
/// aren't part of what thinking is signed with.
fn unmarked(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(map.iter().filter(|(k, _)| *k != "cache_control").map(|(k, v)| (k.clone(), unmarked(v))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(unmarked).collect()),
        other => other.clone(),
    }
}

/// The signatures of the thinking blocks a request sends back, in order.
fn signatures(request: &Value) -> Vec<String> {
    let messages = request["messages"].as_array().into_iter().flatten();
    let blocks = messages.flat_map(|m| m["content"].as_array().into_iter().flatten());
    blocks.filter(|b| b["type"] == "thinking").map(|b| b["signature"].as_str().unwrap().to_owned()).collect()
}

/// Anthropic's own check: Claude's thinking is signed with the system
/// prompt, the tools and every message before it, and a request that
/// sends it back after anything else is turned away. So each thinking
/// block a request carries has to follow exactly the request it was the
/// answer to.
fn assert_thinking_follows_what_it_was_signed_with(requests: &[Value]) {
    for (i, request) in requests.iter().enumerate() {
        let messages = request["messages"].as_array().unwrap();
        for (m, message) in messages.iter().enumerate() {
            for (b, block) in message["content"].as_array().unwrap().iter().enumerate().filter(|(_, b)| b["type"] == "thinking") {
                let signature = block["signature"].as_str().unwrap();
                let j: usize = signature.strip_prefix("sig-").unwrap().parse().unwrap();
                let signed = &requests[j];
                assert!(j < i && b == 0, "request {i}: {signature} out of place");
                assert_eq!(unmarked(&request["system"]), unmarked(&signed["system"]), "request {i}: {signature} after another system prompt");
                assert_eq!(request["tools"], signed["tools"], "request {i}: {signature} after other tools");
                let before: Vec<Value> = messages[..m].iter().map(unmarked).collect();
                let then: Vec<Value> = signed["messages"].as_array().unwrap().iter().map(unmarked).collect();
                assert_eq!(before, then, "request {i} sends {signature} after another conversation than request {j}'s");
            }
        }
    }
}

#[test]
fn claude_thinking_always_goes_back_after_what_it_was_signed_with() {
    let line = "z".repeat(990);
    let big: String = (1..=25).map(|i| format!("{i:03} {line}\n")).collect();
    let read = |start: Value, count: Value| calls("read", json!({ "path": "big.txt", "start_line": start, "line_count": count }));
    let model = Model::claude(vec![
        read(json!(null), json!(null)),
        says("It's big."),
        read(json!(1), json!(6)),
        says("Same start."),
        says("Goal: read big.txt."),
        says("You're welcome."),
        says("Bye."),
    ])
    // Under the third request, over the second: see below.
    .with_env("REIN_COMPACT_AT", "9000");
    let mut term = Term::start_with(&model, "claude-compact", 30, 100, &[("big.txt", &big)]);
    term.wait_for(IDLE);
    term.keys("read big.txt\r");
    term.wait_for("It's big.");
    term.wait_for(IDLE);
    // The second read makes the first stale, and the conversation too big:
    // the first read's output is retired before the next request.
    term.keys("read the start again\r");
    term.wait_for("Same start.");
    term.wait_for(IDLE);
    assert!(term.text().contains("no summary needed, 1 stale tool output retired"), "{}", term.text());
    term.keys("/compact\r");
    term.wait_for("(summarized");
    term.wait_for(IDLE);
    term.keys("thanks\r");
    term.wait_for("You're welcome.");
    term.wait_for(IDLE);
    term.keys("bye\r");
    term.wait_for("Bye.");

    let requests = model.requests();
    assert_eq!(requests.len(), 7);
    assert_thinking_follows_what_it_was_signed_with(&requests);
    // Thinking from before the retired output stays; after it, it goes.
    assert_eq!(signatures(&requests[2]), ["sig-0", "sig-1"]);
    assert_eq!(signatures(&requests[3]), ["sig-0"]);
    assert_eq!(signatures(&requests[4]), ["sig-0", "sig-3"], "the summary request is the conversation as it was");
    assert_eq!(signatures(&requests[5]), Vec::<String>::new(), "after a summary nothing before it holds");
    assert_eq!(signatures(&requests[6]), ["sig-5"]);
}

#[test]
fn a_conversation_moves_between_providers_and_continues_where_it_was() {
    let claude = Model::claude(vec![says("From Claude."), says("Claude again.")]);
    let gpt = Model::start(vec![says("From GPT."), says("GPT again.")]).with_env("ANTHROPIC_API_KEY", "test-key").with_env("ANTHROPIC_BASE_URL", &claude.base);
    let mut term = Term::start(&gpt, "switch", 24, 100);
    term.wait_for(IDLE);
    term.keys("hi\r");
    term.wait_for("From GPT.");
    term.wait_for(IDLE);
    // Every provider's models in one menu: narrowed by typing, picked with Enter.
    term.keys("/model\r");
    term.wait_for("choose a model");
    term.wait_for("claude-opus-5-5");
    assert!(term.text().contains("gpt-6.1-sol"), "{}", term.text());
    term.keys("opus");
    term.wait_until("only Claude left", |screen| !screen.contains("test-model") && screen.contains("claude-opus-5-5"));
    term.keys("\r");
    term.wait_for("now using claude-opus-5-5");
    term.keys("and you?\r");
    term.wait_for("From Claude.");
    term.wait_for(IDLE);
    // Claude was told everything said so far, as Claude's messages.
    let to_claude = &claude.requests()[0];
    assert_eq!(to_claude["model"], "claude-opus-5-5");
    let said: Vec<String> = to_claude["messages"].as_array().unwrap().iter().map(|m| format!("{} {}", m["role"].as_str().unwrap(), m["content"])).collect();
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(said[0].starts_with("user ") && said[0].contains("\"hi\""), "{said:?}");
    assert!(said[1].starts_with("assistant ") && said[1].contains("From GPT."), "{said:?}");
    assert!(said[2].starts_with("user ") && said[2].contains("and you?"), "{said:?}");

    term.keys("/model gpt-6.1-sol\r");
    term.wait_for("now using gpt-6.1-sol");
    term.keys("back to you\r");
    term.wait_for("GPT again.");
    term.wait_for(IDLE);
    let to_gpt = gpt.requests()[1].to_string();
    assert!(to_gpt.contains("From Claude.") && !to_gpt.contains("sig-0"), "Claude's words go to GPT, its signed thinking doesn't: {to_gpt}");

    term.keys("/model claude-opus-5-5\r");
    term.wait_for("now using claude-opus-5-5");
    term.keys("/exit\r");
    // -c goes on with the provider the conversation was left with.
    let mut term = term.again_with(&gpt, &["-c"]);
    term.wait_for(IDLE);
    assert!(term.text().contains("claude-opus-5-5"), "{}", term.text());
    term.keys("one more\r");
    term.wait_for("Claude again.");
    assert_eq!(claude.requests().len(), 2);
    assert_eq!(signatures(&claude.requests()[1]), ["sig-0"], "its own thinking, after the same conversation");
}

/// rein run once, outside a terminal, with `input` piped in.
fn run(model: &Model, home: &std::path::Path, args: &[&str], input: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rein"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("REIN_HOME", home)
        .env("ANTHROPIC_BASE_URL", &model.base)
        // Never this computer's browser.
        .env("BROWSER", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

#[test]
fn a_key_is_checked_where_only_a_good_one_gets_an_answer() {
    // OpenRouter lists its models for anyone, so that proves nothing.
    let model = Model::chat(vec![], None);
    let home = std::env::temp_dir().join(format!("rein-login-or-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("providers"), format!("[openrouter]\nurl = {}\n", model.base)).unwrap();
    let (ok, said) = run(&model, &home, &["login", "openrouter", "--key"], "bad-key\n");
    assert!(!ok && said.contains("that key didn't work") && said.contains("nothing was saved"), "{said}");
    assert!(!home.join("keys.json").exists());
    let (ok, said) = run(&model, &home, &["login", "openrouter", "--key"], "good-key\n");
    assert!(ok && said.contains("Saved. Your OpenRouter key works"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn ctrl_c_at_the_key_prompt_shows_typing_again() {
    let model = Model::claude(vec![]);
    let dir = std::env::temp_dir().join(format!("rein-screen-{}-secret", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut term = Term::spawn_with(&model, dir, 24, 100, &["login", "anthropic"]);
    term.wait_for("Paste your Anthropic API key");
    let echoing = |term: &Term| {
        // SAFETY: an all-zero termios is valid, and tcgetattr fills it.
        let mut now: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(term.keyboard.as_raw_fd(), &mut now) }, 0);
        now.c_lflag & libc::ECHO != 0
    };
    let start = Instant::now();
    while echoing(&term) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!echoing(&term), "the key isn't shown as it's typed");
    term.keys("sk-ant-half\x03");
    term.wait_exit();
    assert!(echoing(&term), "the terminal shows what's typed again");
    assert!(!term.text().contains("sk-ant-half"));
}

#[test]
fn a_saved_key_is_checked_kept_private_used_and_forgotten() {
    let model = Model::claude(vec![]);
    let home = std::env::temp_dir().join(format!("rein-login-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let (ok, said) = run(&model, &home, &["login", "anthropic"], "sk-ant-test\n");
    assert!(ok && said.contains("Saved. Your Anthropic key works: 1 model."), "{said}");
    let keys = home.join("keys.json");
    assert!(std::fs::read_to_string(&keys).unwrap().contains("sk-ant-test"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&keys).unwrap().permissions().mode() & 0o777, 0o600, "only you can read it");
    assert!(model.requests().is_empty(), "checking the key lists models; it sends no message");

    let (ok, said) = run(&model, &home, &["models", "anthropic"], "");
    assert!(ok && said.contains("Using your saved Anthropic key with") && said.contains("claude-opus-5-5"), "{said}");

    let (ok, said) = run(&model, &home, &["login", "nobody"], "key\n");
    assert!(!ok && said.contains("no provider called nobody"), "{said}");
    let (ok, said) = run(&model, &home, &["logout", "anthropic"], "");
    assert!(ok && said.contains("Forgot your anthropic key."), "{said}");
    let (ok, said) = run(&model, &home, &["models", "anthropic"], "");
    assert!(!ok && said.contains("no key for Anthropic: set ANTHROPIC_API_KEY, or run `rein login anthropic`"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_chat_completions_server_works_and_what_it_turns_away_is_left_out() {
    let model = Model::chat(vec![calls("glob", json!({ "pattern": "*.txt", "path": null })), says("Two text files.")], Some("reasoning_effort")).with_env("REIN_EFFORT", "low");
    let mut term = Term::start_with(&model, "chat", 24, 100, &[("a.txt", "a"), ("b.txt", "b")]);
    term.wait_for(IDLE);
    assert!(term.text().contains("groq/llama-test"), "{}", term.text());
    term.keys("which text files are there?\r");
    term.wait_for("Two text files.");
    term.wait_for(IDLE);
    let screen = term.text();
    assert!(screen.contains("✻ Thinking it over"), "{screen}");
    assert!(screen.contains("cache 50%"), "{screen}");
    let head = model.heads()[0].to_lowercase();
    assert!(head.starts_with("post /v1/chat/completions ") && head.contains("authorization: bearer test-key"), "{head}");
    // Turned away once for reasoning_effort, then never sent it again.
    assert_eq!(model.turned_away(), 1);
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.get("reasoning_effort").is_none()));
    let messages = requests[1]["messages"].as_array().unwrap();
    let call = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(call["tool_calls"][0]["function"]["name"], "glob");
    assert_eq!(call["reasoning_content"], "**Thinking it over**", "its reasoning goes back with it");
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(result["tool_call_id"], call["tool_calls"][0]["id"]);
    assert!(result["content"].as_str().unwrap().contains("a.txt"), "{result}");
}

#[test]
fn gemini_gets_its_thought_signatures_back_on_the_same_parts() {
    let model = Model::gemini(vec![calls("glob", json!({ "pattern": "*.txt", "path": null })), says("Two text files."), says("It says a.")]);
    let mut term = Term::start_with(&model, "gemini", 24, 100, &[("a.txt", "a"), ("b.txt", "b")]);
    term.wait_for(IDLE);
    assert!(term.text().contains("gemini-3.8-flash"), "{}", term.text());
    term.keys("which text files are there?\r");
    term.wait_for("Two text files.");
    term.wait_for(IDLE);
    let screen = term.text();
    assert!(screen.contains("✻ Pondering") && screen.contains("cache 25%"), "{screen}");
    let head = model.heads()[0].to_lowercase();
    assert!(head.starts_with("post /v1/models/gemini-3.8-flash:streamgeneratecontent?alt=sse ") && head.contains("x-goog-api-key: test-key"), "{head}");
    let contents = model.requests()[1]["contents"].as_array().unwrap().clone();
    let call = contents.iter().find(|c| c["role"] == "model").unwrap();
    assert_eq!(call["parts"][0]["thoughtSignature"], "gsig-0", "its signature, on the call it came with");
    let result = &contents[2]["parts"][0]["functionResponse"];
    assert_eq!((result["name"].clone(), result["id"].clone()), (json!("glob"), call["parts"][0]["functionCall"]["id"].clone()));
    // A file you point at is read by rein, not Gemini: its call says so.
    term.keys("what does @a.txt say?\r");
    term.wait_for("It says a.");
    let contents = model.requests()[2]["contents"].as_array().unwrap().clone();
    let mention = contents.iter().rev().find(|c| c["role"] == "model" && c["parts"][0].get("functionCall").is_some()).unwrap();
    assert_eq!(mention["parts"][0]["thoughtSignature"], "skip_thought_signature_validator");
}

#[test]
fn openrouter_sign_in_happens_in_the_browser_and_rein_keeps_the_key_it_makes() {
    use std::io::BufRead;
    let model = Model::chat(vec![], None);
    let home = std::env::temp_dir().join(format!("rein-login-browser-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("providers"), format!("[openrouter]\nurl = {}\n", model.base)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rein"))
        .args(["login", "openrouter"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("REIN_HOME", &home)
        // A browser that can't open: rein shows the link instead.
        .env("BROWSER", "false")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut shown = String::new();
    let link = loop {
        let mut line = String::new();
        assert!(said.read_line(&mut line).unwrap() > 0, "no link shown: {shown}");
        shown += &line;
        if let Some(at) = line.find("https://openrouter.ai/auth?") {
            break rein_llm::reqwest::Url::parse(line[at..].trim()).unwrap();
        }
    };
    let param = |name: &str| link.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned()).unwrap();
    assert_eq!((param("code_challenge_method"), param("key_label")), ("S256".to_owned(), "rein".to_owned()));
    // The browser comes back with a code, as OpenRouter sends it.
    let callback = rein_llm::reqwest::Url::parse(&param("callback_url")).unwrap();
    let mut browser = TcpStream::connect(("127.0.0.1", callback.port().unwrap())).unwrap();
    write!(browser, "GET {}?code=c1&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n", callback.path(), param("state")).unwrap();
    let mut page = String::new();
    browser.read_to_string(&mut page).unwrap();
    assert!(page.starts_with("HTTP/1.1 200") && page.contains("rein is connected to OpenRouter"), "{page}");

    let mut rest = String::new();
    said.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success(), "{shown}{rest}");
    assert!(rest.contains("Signed in. OpenRouter made rein a key of its own"), "{rest}");
    assert!(std::fs::read_to_string(home.join("keys.json")).unwrap().contains("good-key"));
    let traded = model.heads().into_iter().find(|h| h.starts_with("POST /v1/auth/keys")).unwrap();
    assert!(traded.contains("\"code\":\"c1\"") && traded.contains("code_verifier"), "{traded}");

    let (ok, said) = run(&model, &home, &["logout", "openrouter"], "");
    assert!(ok && said.contains("still works there until you delete it"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn signing_in_from_the_conversation_makes_a_provider_ready_to_switch_to() {
    let claude = Model::claude(vec![says("Hello from Claude.")]);
    let gpt = Model::start(vec![says("From GPT.")]).with_env("ANTHROPIC_BASE_URL", &claude.base);
    let mut term = Term::start(&gpt, "login-here", 40, 100);
    term.wait_for(IDLE);
    // /login alone: a menu of who to sign in to, and what pays for each now.
    term.keys("/login\r");
    term.wait_for("sign in to a provider");
    let screen = term.text();
    let row = |name: &str| screen.lines().find(|l| l.trim_start().trim_start_matches('›').trim_start().starts_with(name)).unwrap_or_default().to_owned();
    assert!(row("openai").contains("$OPENAI_API_KEY") && row("openai").trim_start().starts_with('›'), "it starts on the one in use:\n{screen}");
    assert!(row("anthropic").contains("no key yet") && row("chatgpt").contains("ChatGPT plan"), "{screen}");
    assert!(!screen.contains("ollama"), "nothing to sign in to there:\n{screen}");

    // Picked from the menu; a key typed in the box shows as dots, and Esc
    // leaves without one.
    term.keys("anthr\r");
    term.wait_for("Paste your Anthropic API key");
    term.keys("sk-ant-half");
    term.wait_for("•••••••••••");
    assert!(!term.text().contains("sk-ant-half"), "{}", term.text());
    term.keys("\x1b");
    term.wait_for("(cancelled; nothing was saved)");
    term.wait_for(IDLE);
    assert!(!term.dir.join("home/keys.json").exists());
    assert!(!term.text().contains("sk-ant-half"));

    term.keys("/login anthropic\r");
    term.wait_for("Paste your Anthropic API key");
    term.keys("sk-ant-test\r");
    term.wait_for("Saved. Your Anthropic key works");
    term.wait_for("/model anthropic/<model> switches to it");
    assert!(!term.text().contains("sk-ant-test"), "{}", term.text());
    assert!(std::fs::read_to_string(term.dir.join("home/keys.json")).unwrap().contains("sk-ant-test"));

    // Ready at once: no restart.
    term.keys("/model claude-opus-5-5\r");
    term.wait_for("now using claude-opus-5-5");
    term.keys("hi\r");
    term.wait_for("Hello from Claude.");
    assert!(claude.heads()[0].to_lowercase().contains("x-api-key: sk-ant-test"), "{}", claude.heads()[0]);

    term.keys("/logout\r");
    term.wait_for("forget which");
    term.keys("\r");
    term.wait_for("Forgot your anthropic key.");
    assert!(!std::fs::read_to_string(term.dir.join("home/keys.json")).unwrap().contains("sk-ant-test"));
}

#[test]
fn a_menu_is_chosen_from_with_the_arrows_and_enter() {
    let model = Model::start(vec![]);
    let mut term = Term::start(&model, "menu", 30, 100);
    term.wait_for(IDLE);
    term.keys("/mode\r");
    // It starts on the mode in use, marked. (Waited for, not looked at
    // once: the title can be drawn before the list is.)
    term.wait_for("› ask•");
    let screen = term.text();
    assert!(!screen.contains("edits: file"), "each says what it is once:\n{screen}");
    // Down, and down again from the last comes back to the first.
    term.keys("\x1b[B");
    term.wait_for("› edits");
    term.keys("\x1b[B\x1b[B\x1b[B\x1b[B");
    term.wait_for("› ask•");
    term.keys("\x1b[A\x1b[A\x1b[A\x1b[A");
    term.wait_for("› edits");
    term.keys("\r");
    term.wait_for("✓ edits: file changes in this folder");
    term.wait_until("the bar says edits", |screen| screen.lines().last().is_some_and(|bar| bar.contains("edits")));

    // Esc changes nothing; what's typed narrows the list.
    term.keys("/effort\r");
    term.wait_for("how hard the model thinks");
    term.keys("\x1b");
    term.wait_for(IDLE);
    assert!(!term.text().contains("reasoning effort:"), "{}", term.text());
    term.keys("/effort\r");
    term.wait_for("how hard the model thinks");
    term.keys("xh");
    term.wait_until("only xhigh", |screen| screen.contains("› xhigh") && !screen.contains("minimal"));
    term.keys("\r");
    term.wait_for("✓ reasoning effort: xhigh");
}

#[test]
fn esc_while_rein_looks_something_up_doesnt_stop_the_next_turn() {
    let claude = Model::claude(vec![]);
    let gpt = Model::start(vec![says("Still here.")]).with_env("ANTHROPIC_API_KEY", "test-key").with_env("ANTHROPIC_BASE_URL", &claude.base);
    let mut term = Term::start(&gpt, "stale-stop", 30, 100);
    term.wait_for(IDLE);
    let claude = claude.with_slow_model_list();
    term.keys("/model\r");
    term.wait_for("Looking up the other providers' models");
    // Esc while nothing can be stopped...
    term.keys("\x1b");
    term.wait_for("choose a model");
    term.keys("\x1b");
    term.wait_for(IDLE);
    // ...mustn't be kept for the next turn.
    term.keys("hi\r");
    term.wait_for("Still here.");
    assert!(!term.text().contains("(interrupted"), "{}", term.text());
    drop(claude);
}

#[test]
fn undo_in_a_new_conversation_doesnt_reach_into_the_last_one() {
    let model = Model::start(vec![calls("write", json!({ "path": "a.txt", "content": "changed\n" })), says("Wrote it.")]).with_env("REIN_MODE", "edits");
    let mut term = Term::start_with(&model, "undo-new", 30, 100, &[("a.txt", "original\n")]);
    term.wait_for(IDLE);
    term.keys("change a.txt\r");
    term.wait_for("Wrote it.");
    term.wait_for(IDLE);
    term.keys("/new\r");
    term.wait_for("a fresh conversation");
    term.keys("/undo\r");
    term.wait_for("nothing to undo");
    assert_eq!(std::fs::read_to_string(term.dir.join("project/a.txt")).unwrap(), "changed\n", "the last conversation's change stays");
}

/// A command as the model asks for one: what it needs, and why.
fn bash(command: &str, access: &str, reason: Option<&str>) -> Out {
    Out::Call("bash", json!({ "command": command, "timeout_seconds": null, "access": access, "reason": reason }))
}

/// What rein sent back for each step, in the latest request.
fn results(model: &Model) -> Vec<String> {
    let requests = model.requests();
    let input = requests.last().and_then(|request| request["input"].as_array()).cloned().unwrap_or_default();
    input.iter().filter_map(|item| item["output"].as_str().map(str::to_owned)).collect()
}

#[cfg(target_os = "macos")]
#[test]
fn a_command_that_only_looks_runs_without_asking_and_changes_nothing() {
    let model = Model::start(vec![Turn { out: vec![bash("cat notes.txt", "look", None), bash("echo changed > notes.txt", "look", None)] }, says("Read them.")]);
    let mut term = Term::start_with(&model, "sandbox-look", 30, 100, &[("notes.txt", "kept\n")]);
    term.wait_for(IDLE);
    term.keys("read the notes\r");
    term.wait_for("Read them.");
    assert!(!term.text().contains("Allow this"), "nothing was asked:\n{}", term.text());
    let results = results(&model);
    assert_eq!(results[0], "kept\n[exit code 0]");
    assert!(results[1].contains("Operation not permitted") && results[1].contains("It ran in the sandbox with look access"), "{results:?}");
    assert_eq!(std::fs::read_to_string(term.dir.join("project/notes.txt")).unwrap(), "kept\n");
}

#[cfg(target_os = "macos")]
#[test]
fn going_past_the_sandbox_asks_and_says_why() {
    let fetch = bash("echo fetched", "network", Some("to download the crates"));
    let model = Model::start(vec![Turn { out: vec![fetch] }, says("Fetched.")]).with_env("REIN_MODE", "edits");
    let mut term = Term::start(&model, "sandbox-network", 30, 100);
    term.wait_for(IDLE);
    term.keys("fetch the crates\r");
    term.wait_for("Allow this command to use the network?");
    term.wait_for("Why: to download the crates");
    // Keys too soon after a question still go to the box.
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Fetched.");
    assert!(term.text().contains("echo fetched (with network)"), "the transcript says it went past:\n{}", term.text());
    assert_eq!(results(&model)[0], "fetched\n[exit code 0]");
}

#[cfg(target_os = "macos")]
#[test]
fn read_only_runs_what_looks_and_refuses_the_rest() {
    let model = Model::start(vec![Turn { out: vec![bash("ls", "look", None), bash("touch made.txt", "write", None)] }, says("Looked.")]).with_env("REIN_MODE", "read-only");
    let mut term = Term::start_with(&model, "sandbox-read-only", 30, 100, &[("a.txt", "a\n")]);
    term.wait_for(IDLE);
    term.keys("look around\r");
    term.wait_for("Looked.");
    let results = results(&model);
    assert_eq!(results[0], "a.txt\n[exit code 0]");
    assert!(results[1].starts_with("rein is in read-only mode"), "{results:?}");
    assert!(!term.dir.join("project/made.txt").exists() && !term.text().contains("Allow this"), "{}", term.text());
}

#[cfg(target_os = "macos")]
#[test]
fn auto_never_asks_and_refuses_what_would_go_past_the_sandbox() {
    let past = bash("docker ps", "outside", Some("to see the containers"));
    let model = Model::start(vec![Turn { out: vec![bash("touch made.txt", "write", None), past] }, says("Made it.")]).with_env("REIN_MODE", "auto");
    let mut term = Term::start(&model, "sandbox-auto", 30, 100);
    term.wait_for(IDLE);
    term.keys("make the file\r");
    term.wait_for("Made it.");
    let results = results(&model);
    assert_eq!(results[0], "[exit code 0]");
    assert!(results[1].starts_with("rein is in auto mode, where nothing asks"), "{results:?}");
    assert!(term.dir.join("project/made.txt").exists() && !term.text().contains("Allow this"), "{}", term.text());
    term.wait_for(IDLE);
    term.keys("/status\r");
    term.wait_for("on: macOS Seatbelt");
}

#[cfg(target_os = "macos")]
#[test]
fn keys_and_secrets_stay_out_of_commands() {
    let mut model = Model::start(vec![Turn { out: vec![bash("echo key=${OPENAI_API_KEY:-none}", "look", None), bash("cat ~/.ssh/id_test", "look", None)] }, says("Checked.")]);
    model.home.push((".ssh/id_test".into(), "secret\n".into()));
    let mut term = Term::start(&model, "sandbox-secrets", 30, 100);
    term.wait_for(IDLE);
    term.keys("check\r");
    term.wait_for("Checked.");
    let results = results(&model);
    // rein has a key in its environment; the command doesn't.
    assert_eq!(results[0], "key=none\n[exit code 0]");
    assert!(results[1].contains("Operation not permitted") && !results[1].lines().any(|line| line == "secret"), "{results:?}");
}

#[test]
fn with_the_sandbox_off_commands_ask_first() {
    let model = Model::start(vec![Turn { out: vec![bash("ls", "look", None)] }, says("Listed.")]).with_env("REIN_SANDBOX", "off");
    let mut term = Term::start(&model, "sandbox-off", 30, 100);
    term.wait_for("Sandbox: off in the settings, so commands ask before they run");
    term.wait_for(IDLE);
    term.keys("list\r");
    term.wait_for("Allow this command?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Listed.");
}

#[test]
fn shift_tab_goes_through_the_modes_auto_among_them() {
    let model = Model::start(Vec::new());
    let mut term = Term::start(&model, "mode-cycle", 30, 100);
    term.wait_for(IDLE);
    for mode in ["edits: ", "auto: nothing asks", "read-only: ", "ask: "] {
        term.keys("\x1b[Z");
        term.wait_for(mode);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_command_cant_read_what_you_type() {
    // rein's terminal is the command's too: reading it would take your keys.
    let model = Model::start(vec![Turn { out: vec![bash("head -c 1 /dev/tty", "look", None)] }, says("Tried.")]);
    let mut term = Term::start(&model, "sandbox-tty", 30, 100);
    term.wait_for(IDLE);
    term.keys("read the terminal\r");
    term.wait_for("Tried.");
    let results = results(&model);
    assert!(results[0].contains("Operation not permitted"), "{results:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_command_cant_read_reins_keys_from_its_environment() {
    // macOS gives any process of yours another's environment (sysctl
    // KERN_PROCARGS2), sandbox or not: rein mustn't have its keys there.
    let probe = r#"import ctypes, sys
libc = ctypes.CDLL(None)
mib = (ctypes.c_int * 3)(1, 49, int(sys.argv[1]))
size = ctypes.c_size_t(1 << 20)
buf = ctypes.create_string_buffer(size.value)
read = libc.sysctl(mib, 3, buf, ctypes.byref(size), None, 0) == 0
print("leaked" if b"OPENAI_API_KEY=test" in buf.raw[: size.value] else ("kept" if read else "unread"))"#;
    let command = format!("if command -v python3 >/dev/null; then python3 -I -c '{probe}' $PPID; else echo no-python; fi");
    let model = Model::start(vec![Turn { out: vec![bash(&command, "look", None)] }, says("Probed."), says("Still signed in.")]);
    let mut term = Term::start(&model, "sandbox-environment", 30, 100);
    term.wait_for(IDLE);
    term.keys("probe\r");
    term.wait_for("Probed.");
    let results = results(&model);
    if results[0].starts_with("no-python") {
        return;
    }
    assert_eq!(results[0], "kept\n[exit code 0]", "{results:?}");
    // rein itself still has its key: the next request goes out with it.
    term.wait_for(IDLE);
    term.keys("again\r");
    term.wait_for("Still signed in.");
}

#[test]
fn a_command_outside_the_sandbox_still_gets_your_keys() {
    let model = Model::start(vec![Turn { out: vec![bash("echo key=${OPENAI_API_KEY:-none}", "outside", Some("to call the API"))] }, says("Done.")]).with_env("REIN_MODE", "trust");
    let mut term = Term::start(&model, "outside-environment", 30, 100);
    term.wait_for(IDLE);
    term.keys("run it\r");
    term.wait_for("Done.");
    assert_eq!(results(&model)[0], "key=test\n[exit code 0]");
}
