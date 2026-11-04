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
    /// Text citing a page, as a model that searched the web writes it.
    Cited(&'static str, &'static str),
}

impl Out {
    /// What it says, where it's text: cited text is text to every fake but
    /// the Responses one.
    fn text(&self) -> &str {
        match self {
            Out::Text(text) => text,
            Out::Cited(text, _) => text,
            Out::Call(..) => "",
        }
    }
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
    /// The model list it gives, when not its wire's usual.
    models: Arc<Mutex<Option<Value>>>,
    /// A request with this in it is turned away with that error, as a
    /// model turns down an effort level it doesn't take.
    turns_down: Arc<Mutex<Option<(&'static str, Value)>>>,
    /// The page it gives as the MCP Registry, when rein is told it's that.
    registry: Arc<Mutex<Option<Value>>>,
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

    /// Lists `models` instead of its usual.
    fn with_models(self, models: Value) -> Model {
        *self.models.lock().unwrap() = Some(models);
        self
    }

    /// Turns away requests that have `level` as their effort, as OpenAI
    /// does for a level the model doesn't take.
    fn turning_down(self, level: &'static str) -> Model {
        let error = json!({ "error": { "message": format!("Unsupported value: '{level}' is not supported with this model."), "type": "invalid_request_error", "param": "reasoning.effort", "code": "unsupported_value" } });
        let needle: &'static str = Box::leak(format!("\"effort\":\"{level}\"").into_boxed_str());
        *self.turns_down.lock().unwrap() = Some((needle, error));
        self
    }

    /// Answers as the MCP Registry with `page`, and rein looks servers up here.
    fn with_registry(self, page: Value) -> Model {
        *self.registry.lock().unwrap() = Some(page);
        let base = self.base.trim_end_matches("/v1").to_owned();
        self.with_env("REIN_MCP_REGISTRY", &base)
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
        let models = Arc::new(Mutex::new(None::<Value>));
        let listed_now = models.clone();
        let turns_down = Arc::new(Mutex::new(None::<(&'static str, Value)>));
        let turning_now = turns_down.clone();
        let registry = Arc::new(Mutex::new(None::<Value>));
        let listing = registry.clone();
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
                } else if let Some(reply) = far(&head, &body) {
                    reply
                } else if head.starts_with("GET /v0.1/servers?") {
                    heard.lock().unwrap().push(head.lines().next().unwrap_or_default().to_owned());
                    let page = listing.lock().unwrap().clone().unwrap_or_else(|| json!({ "servers": [] }));
                    http(200, "application/json", page.to_string().as_bytes())
                } else if head.starts_with("GET") {
                    if slow.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1500));
                    }
                    let models = match (listed_now.lock().unwrap().clone(), wire) {
                        (Some(models), _) => models,
                        (None, Wire::Responses) => json!({ "data": [{ "id": "test-model" }, { "id": "gpt-6.1-sol" }] }),
                        (None, Wire::Messages) => {
                            json!({ "data": [{ "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1_000_000, "max_tokens": 128_000 }], "has_more": false })
                        }
                        (None, Wire::Chat) => json!({ "object": "list", "data": [{ "id": "llama-test", "context_window": 131_072 }] }),
                        (None, Wire::Gemini) => {
                            json!({ "models": [{ "name": "models/gemini-3.8-flash", "inputTokenLimit": 1_048_576, "outputTokenLimit": 65_536, "supportedGenerationMethods": ["generateContent"] }] })
                        }
                    };
                    http(200, "application/json", models.to_string().as_bytes())
                } else {
                    let gzip = head.lines().any(|line| line.to_ascii_lowercase().starts_with("content-encoding:") && line.contains("gzip"));
                    how.lock().unwrap().push(gzip);
                    let unpacked = if gzip && takes_gzip { gunzip(&body) } else { body.clone() };
                    let rejected = rejects.filter(|field| String::from_utf8_lossy(&unpacked).contains(&format!("\"{field}\"")));
                    let turned_down = turning_now.lock().unwrap().clone().filter(|(needle, _)| String::from_utf8_lossy(&unpacked).contains(needle));
                    if gzip && !takes_gzip {
                        http(400, "application/json", br#"{"detail":"Bad Request"}"#)
                    } else if let Some((_, error)) = turned_down {
                        *refused.lock().unwrap() += 1;
                        http(400, "application/json", error.to_string().as_bytes())
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
        Model { base, requests, heads, gzipped, env: Vec::new(), home: Vec::new(), turned_away, slow_list, models, turns_down, registry }
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

/// An MCP server at a URL, `/mcp` here, and the sign-in in front of it, as
/// MCP's spec has them: without its token, a 401 that says where to sign
/// in. With it, it has one tool, `hello`. `None` for what isn't its.
fn far(head: &str, body: &[u8]) -> Option<Vec<u8>> {
    let line = head.lines().next().unwrap_or_default();
    let host = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("host:").map(|host| host.trim().to_owned())).unwrap_or_default();
    let base = format!("http://{host}");
    let json = |status: u16, value: Value, extra: &[(&str, &str)]| {
        let text = value.to_string();
        let extra: String = extra.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
        let mut reply = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", text.len()).into_bytes();
        reply.extend_from_slice(text.as_bytes());
        reply
    };
    if line.starts_with("GET /.well-known/oauth-protected-resource/mcp ") {
        return Some(json(200, json!({ "resource": format!("{base}/mcp"), "authorization_servers": [base] }), &[]));
    }
    if line.starts_with("GET /.well-known/oauth-authorization-server ") {
        let metadata = json!({ "issuer": base, "authorization_endpoint": format!("{base}/authorize"), "token_endpoint": format!("{base}/token"),
                               "registration_endpoint": format!("{base}/register"), "code_challenge_methods_supported": ["S256"] });
        return Some(json(200, metadata, &[]));
    }
    if line.starts_with("POST /register ") {
        return Some(json(201, json!({ "client_id": "rein-far-client" }), &[]));
    }
    if line.starts_with("POST /token ") {
        return Some(json(200, json!({ "access_token": "FAKE-far-token", "refresh_token": "FAKE-far-refresh", "expires_in": 3600 }), &[]));
    }
    if !line.starts_with("POST /mcp ") {
        return None;
    }
    if !head.to_ascii_lowercase().contains("authorization: bearer fake-far-token") {
        let challenge = format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#);
        return Some(json(401, json!({ "error": "invalid_token" }), &[("www-authenticate", &challenge)]));
    }
    let message: Value = serde_json::from_slice(body).unwrap_or_default();
    let answer = |result: Value| json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
    Some(match message["method"].as_str().unwrap_or_default() {
        "initialize" => {
            json(200, answer(json!({ "protocolVersion": "2025-11-25", "capabilities": { "tools": {} }, "serverInfo": { "name": "far" } })), &[("mcp-session-id", "far-1")])
        }
        "tools/list" => json(200, answer(json!({ "tools": [{ "name": "hello", "description": "Says hello.", "inputSchema": { "type": "object" } }] })), &[]),
        "tools/call" => json(200, answer(json!({ "content": [{ "type": "text", "text": "hello from far away" }] })), &[]),
        _ => b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
    })
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
            Out::Cited(text, url) => {
                let annotation = json!({ "type": "url_citation", "url": url, "title": "The page", "start_index": 0, "end_index": text.len() });
                let message = json!({ "type": "message", "content": [{ "type": "output_text", "text": text, "annotations": [annotation] }] });
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
            Out::Text(_) | Out::Cited(..) => {
                let text = item.text();
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
            Out::Text(_) | Out::Cited(..) => {
                let text = item.text();
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
            Out::Text(_) | Out::Cited(..) => {
                let text = item.text();
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
    /// Each title the tab was given (OSC 0 or 1: a tab shows the icon
    /// name, not the window's title).
    titles: Vec<String>,
    /// What the tab's progress bar was told (OSC 9;4), like `3;0`.
    progress: Vec<String>,
    /// What the tab's status under its title was told (iTerm2's OSC 21337).
    statuses: Vec<String>,
}

impl vt100::Callbacks for Seen {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], data: &[u8]) {
        self.copied.push(String::from_utf8(unbase64(data)).unwrap());
    }

    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, name: &[u8]) {
        self.titles.push(String::from_utf8_lossy(name).into_owned());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.first() == Some(&&b"21337"[..]) {
            self.statuses.push(params[1..].iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>().join(";"));
        }
        if params.first() == Some(&&b"9"[..]) && params.get(1) == Some(&&b"4"[..]) {
            self.progress.push(params[2..].iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>().join(";"));
        }
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
        Term::spawn_in(model, dir, &project, &home, rows, cols, args)
    }

    /// rein in `project` with `home` as its own folder, which other reins
    /// may share: tabs side by side. `dir` goes when it's done.
    fn spawn_in(model: &Model, dir: PathBuf, project: &std::path::Path, home: &std::path::Path, rows: u16, cols: u16, args: &[&str]) -> Term {
        let (project, home) = (project.to_owned(), home.to_owned());
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
            // Asking, as most tests are about what asks; one that wants
            // rein's own default sets it empty.
            .env("REIN_MODE", "ask")
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

    /// The tab's titles so far.
    fn titles(&self) -> Vec<String> {
        self.screen.lock().unwrap().callbacks().titles.clone()
    }

    fn statuses(&self) -> Vec<String> {
        self.screen.lock().unwrap().callbacks().statuses.clone()
    }

    fn progress(&self) -> Vec<String> {
        self.screen.lock().unwrap().callbacks().progress.clone()
    }

    fn wait_title(&self, wanted: &str) {
        self.wait_title_like(wanted, |title| title == wanted);
    }

    /// Waits for the tab's title to start with `start`: one that moves,
    /// like `⠋ api · 3s`, is never the same twice.
    fn wait_title_from(&self, start: &str) {
        self.wait_title_like(start, |title| title.starts_with(start));
    }

    fn wait_title_like(&self, wanted: &str, like: impl Fn(&str) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if self.titles().last().is_some_and(|title| like(title)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the title never became {wanted:?}: {:?}", self.titles());
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

/// The wordmark's first and last rows, as the screen shows them.
const MARK_TOP: &str = " █▀▀▄ █▀▀▀ █ █▄  █";
const MARK_BOTTOM: &str = " ▀  ▀ ▀▀▀▀ ▀ ▀   ▀";
/// The strip's row: under the wordmark's three and a row of space.
const STRIP: usize = 4;

/// Long enough to wrap at any width these tests use.
const PARAGRAPH: &str = "A harness is everything around the model: the conversation, the tools, the checks before anything runs, and the screen you read it all on, laid out again whenever the window changes size.";

#[test]
fn a_conversation_starts_with_reins_wordmark_and_what_it_starts_with() {
    let long = (1..=40).map(|n| format!("Line {n}.")).collect::<Vec<_>>().join("\n\n");
    let model = Model::start(vec![says(&long)]);
    let mut term = Term::start(&model, "logo", 24, 80);
    term.wait_for(IDLE);
    let at_top = |rows: &[String]| {
        rows[0].starts_with(&format!("{MARK_TOP}   gpt-6.1-sol · OPENAI_API_KEY · v"))
            && rows[1].contains("project")
            && rows[2].starts_with(MARK_BOTTOM)
            && rows[3].starts_with("━━━━")
    };
    assert!(at_top(&term.rows()), "the wordmark at the very top, with what it started with and a line under:\n{}", term.text());
    let text = term.text();
    assert!(!text.contains("Using OPENAI_API_KEY") && !text.contains("You're project in"), "what it says beside the logo isn't said again:\n{text}");
    // A conversation longer than the window scrolls under it.
    term.keys("go\r");
    term.wait_for("Line 40.");
    assert!(at_top(&term.rows()), "still at the top:\n{}", term.text());
    // Too narrow for it beside the wordmark: under it.
    let model = Model::start(vec![]);
    let narrow = Term::start(&model, "logo-narrow", 24, 26);
    narrow.wait_for(IDLE);
    let rows = narrow.rows();
    let first = rows.iter().position(|row| row.trim_end() == MARK_TOP).unwrap_or_else(|| panic!("no wordmark alone:\n{}", narrow.text()));
    assert!(rows[first + 3].starts_with(" gpt-6.1-sol · OPENAI"), "what it uses under it, the model kept:\n{}", narrow.text());
}

#[test]
fn a_click_before_anything_is_said_is_nothing_to_select() {
    let model = Model::start(vec![says("Still here.")]);
    let mut term = Term::start(&model, "click", 24, 80);
    term.wait_for(IDLE);
    // A press, a drag and a release over the empty conversation.
    term.keys("\x1b[<0;10;5M\x1b[<32;20;6M\x1b[<0;20;6m");
    term.keys("hi\r");
    term.wait_for("Still here.");
    assert!(term.child.try_wait().unwrap().is_none(), "rein is still running:\n{}", term.text());
}

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
fn a_key_typed_after_logins_provider_is_saved_and_never_shown_or_kept() {
    let mut model = Model::chat(vec![says("Hi."), two_steps(), says("Made it.")], None);
    let base = model.base.clone();
    model.home.push(("providers".into(), format!("[groq]\nurl = {base}\n\n[openrouter]\nurl = {base}\n")));
    let mut term = Term::start_with(&model, "login-key", 30, 100, &[ALLOWED]);
    term.wait_for(IDLE);
    term.keys("hello\r");
    term.wait_for("Hi.");
    term.wait_for(IDLE);
    // OpenRouter answers only a good key, and only that one is saved.
    term.keys("/login openrouter good-key\r");
    term.wait_for("Saved. Your OpenRouter key works");
    term.wait_for(IDLE);
    assert!(!term.text().contains("good-key"), "neither shown again nor said back:\n{}", term.text());
    // ↑ brings back what was sent before it.
    term.keys("\x1b[A");
    term.wait_until("the message before it in the box", |_| term.in_box() == "hello");
    term.keys("\x15");
    term.keys("/login openrouter good-key and-more\r");
    term.wait_for("/login takes a provider's name and, if you like, its key");
    term.wait_for(IDLE);

    // Typed while rein works, it waits for the end of the turn, without its key.
    term.keys("make the file\r");
    term.wait_for("Running sleep 1");
    term.keys("/login openrouter good-key\r");
    // While it's typed, the key is in the box, as what you type is: only
    // outside it, above the box, is it shown when it shouldn't be.
    let outside_the_box = |screen: &str| screen.lines().any(|line| line.contains("good-key") && !line.contains('│'));
    term.wait_until("it waiting, or the key above the box", |screen| screen.contains("after this turn: /login openrouter (key hidden)") || outside_the_box(screen));
    assert!(!outside_the_box(&term.text()), "{}", term.text());
    term.wait_for("Made it.");
    term.wait_until("saved again, after the turn", |screen| screen.matches("Saved. Your OpenRouter key works").count() == 2);
    term.wait_for(IDLE);
    assert!(!term.text().contains("good-key"), "{}", term.text());
    term.keys("\x04");
    term.wait_exit();

    assert!(!model.requests().iter().any(|request| request.to_string().contains("good-key")), "the model never saw it");
    let files = every_file(&term.dir);
    assert!(files.iter().any(|file| file.starts_with(term.dir.join("home/history"))), "what was sent is kept: {files:?}");
    for file in files {
        let has = std::fs::read_to_string(&file).unwrap_or_default().contains("good-key");
        assert_eq!(has, file.ends_with("keys.json"), "{} {} the key", file.display(), if has { "has" } else { "doesn't have" });
    }
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

#[test]
fn a_key_a_tool_comes_across_never_reaches_the_model_or_the_saved_conversation() {
    // The project's own settings, with the key rein uses in them.
    let files = [("deploy.env", "ANTHROPIC_API_KEY=test-key\nREGION=eu\n")];
    let model = Model::claude(vec![Turn { out: vec![read("deploy.env")] }, says("Read it.")]);
    let mut term = Term::start_with(&model, "hidden-key", 24, 90, &files);
    term.wait_for(IDLE);
    term.keys("what's in deploy.env?\r");
    term.wait_for("Read it.");
    let sent = model.requests()[1].to_string();
    assert!(sent.contains("ANTHROPIC_API_KEY=[a key rein hides]") && sent.contains("REGION=eu"), "{sent}");
    assert!(!sent.contains("test-key"), "{sent}");
    let home = term.dir.join("home");
    let saved: Vec<PathBuf> = walk(&home).into_iter().filter(|file| std::fs::read_to_string(file).is_ok_and(|text| text.contains("test-key"))).collect();
    assert!(saved.is_empty(), "the key is in {saved:?}");
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().flat_map(|entry| if entry.path().is_dir() { walk(&entry.path()) } else { vec![entry.path()] }).collect()
}

#[test]
fn what_rein_runs_starts_with_core_dumps_off() {
    // Started where a crash would dump its memory, keys and all, to a file.
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit fills the struct, which lives for the call.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &raw mut limit) }, 0);
    if limit.rlim_max == 0 {
        return;
    }
    let on = libc::rlimit { rlim_cur: limit.rlim_max, rlim_max: limit.rlim_max };
    let check = calls("bash", json!({ "command": "ulimit -c", "timeout_seconds": null, "access": "look", "reason": null }));
    let model = Model::claude(vec![check, says("Checked.")]);
    let dir = std::env::temp_dir().join(format!("rein-core-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (project, home) = (dir.join("project"), dir.join("home"));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rein"));
    command
        .args(["--mode", "trust", "how big may a core dump be?"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("REIN_HOME", &home)
        .env("BROWSER", "false")
        .env("SSH_CONNECTION", "test")
        .envs(model.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null());
    // SAFETY: between fork and exec, setrlimit only reads `on`, a copy the
    // closure owns, and making an error from errno allocates nothing.
    unsafe {
        command.pre_exec(move || if libc::setrlimit(libc::RLIMIT_CORE, &raw const on) == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) });
    }
    let out = command.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let sent = model.requests()[1].to_string();
    assert!(sent.contains(r"0\n[exit code 0]") && !sent.contains("unlimited"), "{sent}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_key_never_goes_over_plain_http_to_another_computer() {
    let home = std::env::temp_dir().join(format!("rein-plain-http-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let rein = |args: &[&str], key: Option<&str>, input: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rein"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("REIN_HOME", &home)
            .env("BROWSER", "false")
            .env("SSH_CONNECTION", "test")
            // A documentation address: what's refused is never tried.
            .env("ANTHROPIC_BASE_URL", "http://192.0.2.1/v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(key) = key {
            command.env("ANTHROPIC_API_KEY", key);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    };
    let refused = "rein won't send your Anthropic key to http://192.0.2.1/v1: that's plain http";
    let (ok, said) = rein(&["models", "anthropic"], Some("sk-ant-FAKE"), "");
    assert!(!ok && said.contains(refused), "{said}");
    let (ok, said) = rein(&["login", "anthropic", "--key"], None, "sk-ant-FAKE\n");
    assert!(!ok && said.contains(refused) && !said.contains("FAKE"), "{said}");
    assert!(!home.join("keys.json").exists(), "nothing was saved");
    let _ = std::fs::remove_dir_all(&home);
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
fn claude_code_can_be_selected_from_the_login_menu() {
    let model = Model::start(vec![]);
    let mut term = Term::start(&model, "login-claude-code-menu", 40, 110);
    term.wait_for(IDLE);
    term.keys("/login\r");
    term.wait_for("sign in to a provider");
    term.keys("claude-code");
    term.wait_for("CLI login or setup-token");
    term.keys("\r");
    term.wait_for("press Enter to use an existing");
    term.keys("\r");
    term.wait_for("Claude Code enabled");
    term.wait_for(IDLE);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(term.dir.join("home/claude-code.json")).unwrap()).unwrap();
    assert!(saved["token"].is_null());
    assert!(model.requests().is_empty(), "selecting CLI login doesn't call a model");
}

#[test]
fn claude_code_login_saves_private_tokens_or_selects_the_clis_login() {
    use std::os::unix::fs::PermissionsExt;
    let model = Model::start(Vec::new());
    let home = std::env::temp_dir().join(format!("rein-login-claude-code-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let (ok, said) = run(&model, &home, &["login", "claude-code"], "test-subscription-token\n");
    assert!(ok && said.contains("Claude Code enabled") && !said.contains("test-subscription-token"), "{said}");
    let path = home.join("claude-code.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["token"], "test-subscription-token");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let (ok, said) = run(&model, &home, &["models", "claude-code"], "");
    assert!(ok && said.contains("sonnet") && said.contains("opus"), "{said}");
    assert!(model.requests.lock().unwrap().is_empty());
    let (ok, said) = run(&model, &home, &["logout", "claude-code"], "");
    assert!(ok && said.contains("does not sign the Claude CLI out"), "{said}");
    assert!(!path.exists());
    let (ok, said) = run(&model, &home, &["login", "claude-code"], "\n");
    assert!(ok, "{said}");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved["token"].is_null());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn a_claude_sign_in_an_earlier_rein_made_as_claude_code_is_deleted_and_said_once() {
    let mut model = Model::start(Vec::new());
    model.home.push(("claude-oauth.json".into(), r#"{"access_token":"FAKE-access","refresh_token":"FAKE-refresh","expires_at":9999999999}"#.into()));
    let mut term = Term::start(&model, "borrowed-sign-in", 30, 120);
    term.wait_for("Deleted the Claude sign-in an earlier rein made as Claude Code");
    term.wait_for(IDLE);
    assert!(!term.dir.join("home/claude-oauth.json").exists(), "its tokens don't linger");
    term.keys("\x04");
    // Started again with nothing more put in its home.
    model.home.clear();
    let term = term.again(&model);
    term.wait_for(IDLE);
    assert!(!term.text().contains("Deleted the Claude sign-in"), "said once:\n{}", term.text());

    // Any other command deletes it too, and nothing pays for Claude with it.
    let home = term.dir.join("home");
    std::fs::write(home.join("claude-oauth.json"), "{}").unwrap();
    let (ok, said) = run(&model, &home, &["providers"], "");
    assert!(ok && said.contains("Deleted the Claude sign-in") && !home.join("claude-oauth.json").exists(), "{said}");
    let anthropic = said.lines().find(|line| line.starts_with("anthropic ")).unwrap_or_default();
    assert!(anthropic.ends_with("no key yet: $ANTHROPIC_API_KEY or `rein login anthropic`"), "{said}");
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
fn a_damaged_credential_file_says_where_but_never_whats_in_it() {
    let model = Model::start(Vec::new());
    let home = std::env::temp_dir().join(format!("rein-damaged-{}", std::process::id()));
    for (file, args) in [("keys.json", &["providers"][..]), ("chatgpt.json", &["login"]), ("claude-code.json", &["models", "claude-code"])] {
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(file), "\"sk-test-FAKE-bare-string\"").unwrap();
        let (ok, said) = run(&model, &home, args, "");
        assert!(!ok && said.contains(&format!("{file} isn't as rein wrote it (line 1, column 26)")), "{file}: {said}");
        assert!(!said.contains("FAKE") && !said.contains("response"), "{file}: {said}");
    }
    let _ = std::fs::remove_dir_all(&home);
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

    // A key typed where the provider's name goes isn't said back.
    let (ok, said) = run(&model, &home, &["login", "sk-ant-FAKE-in-place-of-a-name"], "key\n");
    assert!(!ok && said.contains("there's no provider by that name; rein knows chatgpt, openai, anthropic") && !said.contains("FAKE"), "{said}");
    let (ok, said) = run(&model, &home, &["logout", "anthropic"], "");
    assert!(ok && said.contains("Forgot your anthropic key."), "{said}");
    let (ok, said) = run(&model, &home, &["models", "anthropic"], "");
    assert!(!ok && said.contains("no key for Anthropic: set ANTHROPIC_API_KEY, or run `rein login anthropic`"), "{said}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Every file under `dir`, in its folders too.
fn every_file(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() { files.extend(every_file(&path)) } else { files.push(path) }
    }
    files
}

#[test]
fn a_key_typed_after_logins_provider_is_saved_once_it_works_and_goes_nowhere_else() {
    let model = Model::chat(vec![says("This should never be asked for.")], None);
    let dir = std::env::temp_dir().join(format!("rein-login-key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (home, project) = (dir.join("home"), dir.join("project"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(home.join("providers"), format!("[openrouter]\nurl = {}\n", model.base)).unwrap();
    let rein = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_rein"))
            .args(args)
            .current_dir(&project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("REIN_HOME", &home)
            // A model that would answer, were anything sent to it.
            .env("GROQ_API_KEY", "test")
            .env("BROWSER", "false")
            .env("SSH_CONNECTION", "screen-test")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    };
    // OpenRouter answers only a good key: another isn't saved.
    let (ok, said) = rein(&["login", "openrouter", "sk-or-v1-FAKE-bad"]);
    assert!(!ok && said.contains("that key didn't work") && said.contains("nothing was saved") && !said.contains("FAKE"), "{said}");
    assert!(!home.join("keys.json").exists());
    for args in [&["login", "openrouter", "good-key"][..], &["login", "openrouter", "--key", "good-key"]] {
        let (ok, said) = rein(args);
        assert!(ok && said.contains("Saved. Your OpenRouter key works") && !said.contains("good-key"), "{args:?}: {said}");
    }
    assert!(std::fs::read_to_string(home.join("keys.json")).unwrap().contains("good-key"));
    // More than a name and a key is turned down, and never said back.
    for args in [&["login", "openrouter", "good-key", "and-more"][..], &["login", "--key", "good-key"], &["logout", "openrouter", "good-key"], &["logout"]] {
        let (ok, said) = rein(args);
        assert!(!ok && !said.contains("good-key"), "{args:?} is turned down: {said}");
    }
    assert!(model.requests().is_empty(), "nothing went to the model: {:?}", model.requests());
    for file in every_file(&dir) {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        assert!(!text.contains("FAKE"), "{} has the bad key", file.display());
        assert_eq!(text.contains("good-key"), file.ends_with("keys.json"), "{}", file.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
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

/// A list opens over the box, as menus over a box at the bottom of a
/// window do: the best match right on the box, the rest going up from it,
/// and the bar still under the box. The box doesn't move.
#[test]
fn lists_open_over_the_box_the_best_match_next_to_it() {
    let model = Model::start(Vec::new());
    let mut term = Term::start(&model, "lists-up", 30, 100);
    term.wait_for(IDLE);
    let row = |screen: &str, text: &str| screen.lines().position(|line| line.contains(text)).unwrap_or_else(|| panic!("no {text:?}:\n{screen}"));
    let border = |screen: &str| screen.lines().position(|line| line.trim_start().starts_with('╭')).unwrap_or_else(|| panic!("no box:\n{screen}"));
    let at_rest = border(&term.text());
    // Three match: up and down go different ways.
    term.keys("/m");
    term.wait_for("› /mode");
    term.wait_for("/mcp");
    let screen = term.text();
    assert_eq!(row(&screen, "› /mode"), border(&screen) - 1, "the first match right on the box:\n{screen}");
    assert!(row(&screen, "/model") < row(&screen, "› /mode"), "the next one over it:\n{screen}");
    assert_eq!(border(&screen), at_rest, "the box stayed where it was:\n{screen}");
    assert!(screen.lines().last().unwrap_or_default().contains("context"), "the bar under the box:\n{screen}");
    term.keys("\x1b[A");
    term.wait_for("› /model");
    term.keys("\x15");
    term.keys("/mode\r");
    term.wait_for("› ask•");
    let screen = term.text();
    assert_eq!(row(&screen, "trust"), border(&screen) - 1, "a picker's first right on the box too:\n{screen}");
    assert!(row(&screen, "edits") < row(&screen, "› ask•") && row(&screen, "› ask•") < row(&screen, "trust"), "{screen}");
    term.keys("\x1b");
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
    // The list goes up from the box: up, and up again from the last comes
    // back to the first, next to the box.
    term.keys("\x1b[A");
    term.wait_for("› edits");
    term.keys("\x1b[A\x1b[A\x1b[A\x1b[A");
    term.wait_for("› ask•");
    term.keys("\x1b[B\x1b[B\x1b[B\x1b[B");
    term.wait_for("› edits");
    term.keys("\r");
    term.wait_for("✓ edits: file changes in this folder");
    // Asked whether to keep it: no, as most of the time.
    term.wait_for("Start in edits from now on?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("n");
    term.wait_until("the bar says edits", |screen| screen.lines().last().is_some_and(|bar| bar.contains("edits")));
    assert!(!term.dir.join("home/config").exists(), "nothing kept");

    // Esc changes nothing; what's typed narrows the list.
    term.keys("/effort\r");
    term.wait_for("how hard gpt-6.1-sol thinks");
    term.keys("\x1b");
    term.wait_for(IDLE);
    assert!(!term.text().contains("reasoning effort:"), "{}", term.text());
    term.keys("/effort\r");
    term.wait_for("how hard gpt-6.1-sol thinks");
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

#[test]
fn what_a_command_did_is_shown_under_it_and_undo_puts_it_back() {
    let turns = vec![Turn { out: vec![bash("echo two > a.txt && mv b.txt c.txt", "write", None)] }, says("Tidied."), says("Noted.")];
    let model = Model::start(turns).with_env("REIN_MODE", "trust");
    let mut term = Term::start_with(&model, "undo-command", 30, 120, &[("a.txt", "one\n"), ("b.txt", "keep me\n")]);
    term.wait_for(IDLE);
    term.keys("tidy up\r");
    term.wait_for("↳ changed a.txt, made c.txt, removed b.txt");
    term.wait_for("Tidied.");
    term.wait_for(IDLE);
    // One turn to undo: straight to the question, which says what it does.
    term.keys("/undo\r");
    term.wait_for("Undo what rein did after “tidy up”?");
    term.wait_for("Puts back a.txt and b.txt · takes away c.txt, which rein made");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("✓ put back a.txt and b.txt; took away c.txt");
    let project = term.dir.join("project");
    assert_eq!(std::fs::read_to_string(project.join("a.txt")).unwrap(), "one\n");
    assert_eq!(std::fs::read_to_string(project.join("b.txt")).unwrap(), "keep me\n");
    assert!(!project.join("c.txt").exists());
    // The model is told with the next message.
    term.wait_for(IDLE);
    term.keys("thanks\r");
    term.wait_for("Noted.");
    let told = model.requests().last().map(|request| request["input"].to_string()).unwrap_or_default();
    assert!(told.contains("[rein: the user undid what your last turn did to the project's files, back to before \\\"tidy up\\\". Back as they were: a.txt, b.txt."), "{told}");
    term.keys("/undo\r");
    term.wait_for("nothing to undo");
}

#[test]
fn undo_picks_the_turn_to_go_back_to_and_works_after_rein_starts_again() {
    let turns = vec![
        Turn { out: vec![bash("echo two > a.txt", "write", None)] },
        says("First done."),
        Turn { out: vec![bash("echo three > a.txt && echo new > b.txt", "write", None)] },
        says("Second done."),
    ];
    let model = Model::start(turns).with_env("REIN_MODE", "trust");
    let mut term = Term::start_with(&model, "undo-pick", 30, 120, &[("a.txt", "one\n")]);
    term.wait_for(IDLE);
    term.keys("the first\r");
    term.wait_for("First done.");
    term.wait_for(IDLE);
    term.keys("the second\r");
    term.wait_for("Second done.");
    term.wait_for(IDLE);
    term.keys("\x04");
    // Continued: what it kept is there still.
    let mut term = term.again_with(&model, &["-c"]);
    term.wait_for(IDLE);
    term.keys("/undo\r");
    // The latest next to the box; up to the one before.
    term.wait_for("› “the second”");
    term.wait_for("“the first”");
    term.keys("\x1b[A");
    term.wait_for("› “the first”");
    term.keys("\r");
    term.wait_for("Undo rein's last 2 turns, back to before “the first”?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("✓ put back a.txt; took away b.txt");
    let project = term.dir.join("project");
    assert_eq!(std::fs::read_to_string(project.join("a.txt")).unwrap(), "one\n");
    assert!(!project.join("b.txt").exists());
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
    for mode in ["edits: ", "auto: nothing asks", "read-only: ", "trust: ", "ask: "] {
        term.keys("\x1b[Z");
        term.wait_for(mode);
    }
}

#[test]
fn rein_starts_in_trust_mode_and_asks_before_deleting() {
    let turns = vec![Turn { out: vec![bash("touch made.txt", "write", None)] }, Turn { out: vec![bash("rm made.txt", "write", None)] }, says("Made it, and took it away.")];
    // Rein's own default, with nothing set.
    let model = Model::start(turns).with_env("REIN_MODE", "");
    let mut term = Term::start(&model, "trust-default", 30, 120);
    term.wait_for("rein runs everything without asking, but deleting (rm, git clean, git push --force and the like) asks first");
    term.wait_for(IDLE);
    assert!(term.text().lines().last().unwrap_or_default().contains("trust"), "the bar says so:\n{}", term.text());
    let rules = std::fs::read_to_string(term.dir.join("home/permissions")).unwrap();
    assert!(rules.lines().any(|line| line == "ask rm") && rules.lines().any(|line| line == "ask git push --force"), "{rules}");
    term.keys("make a file, then take it away\r");
    term.wait_for("Your rule `ask rm` asks first");
    assert!(term.dir.join("project/made.txt").exists(), "made without asking");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Made it, and took it away.");
    assert!(!term.dir.join("project/made.txt").exists());
    assert_eq!(results(&model), ["[exit code 0]", "[exit code 0]"]);
    term.wait_for(IDLE);
    term.keys("\x04");
    // Said once, the first time; your rules are kept as they are.
    let term = term.again(&model);
    term.wait_for(IDLE);
    assert!(!term.text().contains("rein runs everything"), "{}", term.text());
}

#[test]
fn permissions_adds_your_rules_and_takes_them_out() {
    let turns = vec![Turn { out: vec![bash("touch made.txt", "write", None)] }, says("Touched it."), Turn { out: vec![bash("rm made.txt", "write", None)] }, says("Removed it.")];
    let model = Model::start(turns).with_env("REIN_MODE", "");
    let mut term = Term::start(&model, "permissions", 30, 120);
    term.wait_for(IDLE);
    // Added from the list: "+ add" is first, next to the box.
    term.keys("/permissions\r");
    term.wait_for("› + add");
    term.wait_for("ask rm");
    term.keys("\r");
    term.wait_for("A rule, like ask git push");
    term.keys("ask touch\r");
    term.wait_for("`ask touch` is one of your rules now, for every project");
    let rules = std::fs::read_to_string(term.dir.join("home/permissions")).unwrap();
    assert!(rules.ends_with("ask dropdb\nask touch\n") && rules.starts_with("# Your rules"), "added at the end, the rest kept:\n{rules}");
    term.wait_for(IDLE);
    term.keys("make it\r");
    term.wait_for("Your rule `ask touch` asks first");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Touched it.");
    term.wait_for(IDLE);
    // Taken out from the list, once you say so.
    term.keys("/permissions\r");
    term.wait_for("› + add");
    term.keys("ask rmdir");
    term.wait_for("› ask rmdir");
    term.keys("\r");
    term.wait_for("Take `ask rmdir` out of your rules?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("`ask rmdir` is out of your rules");
    term.wait_for(IDLE);
    // And as typed: then rm runs without asking.
    term.keys("/permissions remove ask rm\r");
    term.wait_for("`ask rm` is out of your rules");
    term.wait_for(IDLE);
    let rules = std::fs::read_to_string(term.dir.join("home/permissions")).unwrap();
    assert!(!rules.lines().any(|line| line == "ask rm" || line == "ask rmdir") && rules.lines().any(|line| line == "ask unlink"), "{rules}");
    assert!(rules.starts_with("# Your rules") && rules.ends_with("ask touch\n"), "the rest kept:\n{rules}");
    term.keys("take it away\r");
    term.wait_for("Removed it.");
    assert!(!term.dir.join("project/made.txt").exists() && !term.text().contains("`ask rm` asks"), "{}", term.text());
    term.wait_for(IDLE);
    term.keys("/permissions remove ask rm\r");
    term.wait_for("`ask rm` isn't one of your rules; /permissions lists them");
}

#[test]
fn a_mode_chosen_can_be_kept_for_new_conversations() {
    let model = Model::start(Vec::new()).with_env("REIN_MODE", "");
    let mut term = Term::start(&model, "mode-kept", 30, 120);
    term.wait_for(IDLE);
    term.keys("/mode edits\r");
    term.wait_for("Start in edits from now on?");
    term.wait_for("New conversations start in trust now");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("new conversations start in edits");
    term.wait_for(IDLE);
    let settings = std::fs::read_to_string(term.dir.join("home/config")).unwrap();
    assert_eq!(settings, "mode = edits\n");
    term.keys("\x04");
    let mut term = term.again(&model);
    term.wait_for("Mode: edits (");
    term.wait_for(IDLE);
    // Kept already: nothing to ask.
    term.keys("/mode edits\r");
    term.wait_for("✓ edits: ");
    term.wait_for(IDLE);
    assert!(!term.text().contains("from now on?"), "{}", term.text());
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
    // KERN_PROCARGS2), and rein mustn't have its keys there; the sandbox
    // doesn't let a command ask for rein's at all.
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
    assert_eq!(results[0], "unread\n[exit code 0]", "{results:?}");
    // rein itself still has its key: the next request goes out with it.
    term.wait_for(IDLE);
    term.keys("again\r");
    term.wait_for("Still signed in.");
}

#[test]
fn a_providers_own_key_variable_is_kept_from_commands_too() {
    // A name no rule takes for a key's: only the providers file says it is.
    let mut model = Model::start(vec![Turn { out: vec![bash("echo \"[$ACME_LLM]\"", "look", None)] }, says("Looked.")]).with_env("ACME_LLM", "FAKE-acme-0123");
    model.home.push(("providers".into(), "[openai]\nkey_env = ACME_LLM\n".into()));
    let mut term = Term::start(&model, "provider-key-env", 30, 100);
    term.wait_for(IDLE);
    if term.text().contains("Sandbox: none") {
        return;
    }
    term.keys("look\r");
    term.wait_for("Looked.");
    assert_eq!(results(&model)[0], "[]\n[exit code 0]");
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

#[test]
fn a_model_going_round_in_circles_is_stopped_and_you_can_go_on() {
    let same = || Turn { out: vec![bash("echo same", "look", None)] };
    let mut turns: Vec<Turn> = (0..5).map(|_| same()).collect();
    turns.push(says("Trying something else."));
    let model = Model::start(turns).with_env("REIN_MODE", "trust");
    let mut term = Term::start(&model, "circles", 30, 120);
    term.wait_for(IDLE);
    term.keys("work\r");
    term.wait_for("stopped going round in circles: Run echo same, 5 times in a row with the same result");
    assert_eq!(model.requests().len(), 5, "a sixth step never went");
    let told = results(&model);
    assert!(told[2].contains("same call, with the same result, several times in a row"), "the model was told first: {told:?}");
    term.wait_for(IDLE);
    term.keys("continue\r");
    term.wait_for("Trying something else.");
}

#[test]
fn auto_mode_stops_at_max_steps_and_says_where_it_got_to() {
    let steps = Turn { out: vec![bash("echo one", "look", None)] };
    let more = Turn { out: vec![bash("echo two", "look", None)] };
    let model = Model::start(vec![steps, more, says("Did one and two; three is left.")]).with_env("REIN_MODE", "auto").with_env("REIN_MAX_STEPS", "2");
    let mut term = Term::start(&model, "max-steps", 30, 120);
    term.wait_for(IDLE);
    term.keys("do all three\r");
    term.wait_for("Did one and two; three is left.");
    term.wait_for("(stopped after 2 steps, the most a turn may take when nobody's watching; type continue to go on)");
    let told = results(&model);
    assert!(told.last().is_some_and(|last| last.contains("that was the last step this turn may take")), "{told:?}");
    term.wait_for(IDLE);
    term.keys("/status\r");
    term.wait_for("at most 2 a turn, as nobody's watching (auto mode)");
}

#[test]
fn a_task_left_to_run_that_doesnt_finish_exits_with_an_error() {
    let model =
        Model::start(vec![Turn { out: vec![bash("echo one", "look", None)] }, says("Started; the rest is left.")]).with_env("REIN_MODE", "trust").with_env("REIN_MAX_STEPS", "1");
    let dir = std::env::temp_dir().join(format!("rein-screen-{}-unfinished", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut term = Term::spawn_with(&model, dir, 24, 120, &["do", "it", "all"]);
    term.wait_exit();
    let status = term.child.try_wait().unwrap().expect("exited");
    assert!(!status.success(), "{}", term.text());
    let screen = term.text();
    assert!(screen.contains("Started; the rest is left.") && screen.contains("the task isn't finished"), "{screen}");
}

#[test]
fn switching_to_auto_mode_brings_the_step_limit_and_back_again() {
    let model = Model::start(Vec::new());
    let mut term = Term::start(&model, "steps-mode", 30, 120);
    term.wait_for(IDLE);
    term.keys("/status\r");
    term.wait_for("no limit while you watch; 200 in auto mode");
    term.wait_for(IDLE);
    // ask, edits, auto.
    for mode in ["edits: ", "auto: nothing asks"] {
        term.keys("\x1b[Z");
        term.wait_for(mode);
    }
    term.keys("/status\r");
    term.wait_for("at most 200 a turn, as nobody's watching (auto mode)");
}

#[test]
fn claude_code_subprocess_keeps_reins_tools_and_uses_subscription_auth() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("rein-screen-{}-claude-code", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(dir.join("project")).unwrap();
    std::fs::write(dir.join("project/example.txt"), "subprocess fixture contents\n").unwrap();
    let program = bin.join("claude");
    std::fs::write(
        &program,
        r#"#!/bin/sh
[ -z "${CLAUDE_CODE_OAUTH_TOKEN+set}" ] || exit 12
[ "$(/bin/cat "/dev/fd/$CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR")" = subscription-test ] || exit 10
[ -z "$OPENAI_API_KEY$ANTHROPIC_API_KEY$ANTHROPIC_AUTH_TOKEN$ANTHROPIC_BASE_URL" ] || exit 11
pwd -P >> "$0.pwd"
printf '%s\n' "$@" > "$0.args"
/bin/cat > "$0.input"
if [ -f "$0.called" ]; then
    printf '%s' '{"structured_output":{"text":"Read through rein, not Claude tools.","tool_calls":[]}}'
else
    : > "$0.called"
    printf '%s' '{"structured_output":{"text":"Inspecting the fixture.","tool_calls":[{"name":"read","arguments":{"path":"example.txt","start_line":null,"line_count":null}}]}}'
fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let model = Model::start(Vec::new())
        .with_env("PATH", bin.to_str().unwrap())
        .with_env("REIN_MODEL", "claude-code/sonnet")
        .with_env("CLAUDE_CODE_OAUTH_TOKEN", "subscription-test")
        .with_env("ANTHROPIC_BASE_URL", "https://must-not-be-used.invalid")
        .with_env("ANTHROPIC_API_KEY", "must-not-be-passed");
    let mut term = Term::spawn(&model, dir, 35, 120);
    term.wait_for(IDLE);
    term.keys("read example.txt\r");
    term.wait_for("Read through rein, not Claude tools.");
    term.wait_for(IDLE);
    let input = std::fs::read_to_string(bin.join("claude.input")).unwrap();
    assert!(input.contains("subprocess fixture contents"), "{input}");
    assert!(model.requests.lock().unwrap().is_empty(), "no native HTTP requests");
    assert!(!term.text().contains("subscription-test"));
    // Never in the project, whose `.claude` settings could send the token
    // elsewhere; and never reading a project's settings at all.
    let project = term.dir.join("project").canonicalize().unwrap();
    let places = std::fs::read_to_string(bin.join("claude.pwd")).unwrap();
    assert_eq!(places.lines().count(), 2, "{places}");
    assert!(places.lines().all(|place| !std::path::Path::new(place).starts_with(&project) && !std::path::Path::new(place).exists()), "{places}");
    let args = std::fs::read_to_string(bin.join("claude.args")).unwrap();
    assert!(args.contains("--setting-sources\nuser\n"), "{args}");
}

/// A project and a home for reins in several tabs, as one user's terminals
/// share them. Under /tmp, short: a socket's path can't be long.
fn tabs(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = PathBuf::from(format!("/tmp/rp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    (base.clone(), base.join("project"), base.join("home"))
}

#[test]
fn a_message_from_another_tab_starts_a_turn_there() {
    let (base, project, home) = tabs("msg");
    let api_model = Model::start(vec![says("Adding a /health endpoint now.")]);
    let web_model = Model::start(vec![calls("message", json!({ "to": "api", "text": "please add a /health endpoint" })), says("Asked api.")]);
    let api = Term::spawn_in(&api_model, base.clone(), &project, &home, 30, 100, &["--name", "api"]);
    api.wait_for(IDLE);
    api.wait_title("○ api");
    let mut web = Term::spawn_in(&web_model, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    // In, and it sees api: its strip shows it.
    web.wait_until("web to see api", |_| web.rows().iter().any(|row| row.contains(" api ○")));
    web.keys("ask api for a health endpoint\r");
    web.wait_for("Asked api.");
    api.wait_for("✉ from web");
    api.wait_for("Adding a /health endpoint now.");
    let heard = api_model.requests()[0].to_string();
    assert!(heard.contains("please add a /health endpoint") && heard.contains("not from the user"), "framed as another rein's: {heard}");
    assert!(web_model.requests()[1].to_string().contains("Sent to api."), "the model heard it went");
    let spinning: Vec<String> = api.titles().into_iter().filter(|title| title.contains(" api · ") && title.chars().next().is_some_and(|c| ('⠀'..='⣿').contains(&c))).collect();
    assert!(!spinning.is_empty(), "the tab's spinner turned while it worked: {:?}", api.titles());
    api.wait_title_from("✓ api · done ");
}

#[test]
fn a_task_handed_to_another_tab_comes_back_with_its_result() {
    let (base, project, home) = tabs("task");
    let write = calls("write", json!({ "path": "health.txt", "content": "ok\n" }));
    let api_model = Model::start(vec![write, says("Added the endpoint.")]);
    let web_model = Model::start(vec![calls("delegate", json!({ "to": "api", "task": "add a /health endpoint" })), says("Handed it to api."), says("api is done with it.")]);
    let api = Term::spawn_in(&api_model, base.clone(), &project, &home, 30, 100, &["--name", "api", "--mode", "edits"]);
    api.wait_for(IDLE);
    let mut web = Term::spawn_in(&web_model, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    web.keys("hand it to api\r");
    web.wait_for("Handed it to api.");
    api.wait_for("Added the endpoint.");
    api.wait_for("the result went back to web");
    web.wait_for("api is done with it.");
    let result = web_model.requests()[2].to_string();
    assert!(result.contains("is done") && result.contains("health.txt") && result.contains("Added the endpoint."), "{result}");
    assert!(api_model.requests()[0].to_string().contains("A task for you"), "{}", api_model.requests()[0]);
    assert_eq!(std::fs::read_to_string(project.join("health.txt")).unwrap(), "ok\n");
}

#[test]
fn a_question_can_be_answered_from_the_dashboard_in_another_tab() {
    let (base, project, home) = tabs("ask");
    let api_model = Model::start(vec![runs("touch made.txt"), says("Made it.")]);
    let mut api = Term::spawn_in(&api_model, base.clone(), &project, &home, 30, 100, &["--name", "api"]);
    api.wait_for(IDLE);
    api.keys("make a file\r");
    api.wait_for("Allow this command");
    api.wait_title("◆ api · needs you");
    let idle = Model::start(vec![]);
    let mut dashboard = Term::spawn_in(&idle, base.clone(), &project, &home, 30, 120, &["peers"]);
    dashboard.wait_for("1 needs you");
    dashboard.wait_for("touch made.txt");
    dashboard.keys("y");
    dashboard.wait_for("told api yes");
    api.wait_for("answered yes from another tab");
    api.wait_for("Made it.");
    assert!(project.join("made.txt").exists(), "it ran once answered");
    dashboard.keys("q");
    dashboard.wait_exit();
}

#[test]
fn a_sandboxed_command_cant_read_who_the_other_reins_are() {
    let (base, project, home) = tabs("wall");
    let peek = format!("cat {}/peers/*.json", home.display());
    let model = Model::start(vec![calls("bash", json!({ "command": peek, "timeout_seconds": null, "access": "look", "reason": null })), says("Looked.")]);
    let other = Model::start(vec![]);
    let api = Term::spawn_in(&other, base.clone(), &project, &home, 30, 100, &["--name", "api"]);
    api.wait_for(IDLE);
    let mut web = Term::spawn_in(&model, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    if web.text().contains("Sandbox: none") {
        return;
    }
    web.keys("peek\r");
    web.wait_for("Looked.");
    let result = model.requests()[1].to_string();
    assert!(!result.contains("token") && !result.contains(".sock"), "the peers' files stay out of a command's reach: {result}");
}

#[test]
fn with_peers_on_hold_what_another_tab_sends_waits_for_you_to_let_it_in() {
    let (base, project, home) = tabs("held");
    let api_model = Model::start(vec![says("Looking at it.")]).with_env("REIN_PEERS", "hold");
    let mut api = Term::spawn_in(&api_model, base.clone(), &project, &home, 30, 100, &["--name", "api"]);
    api.wait_for(IDLE);
    let idle = Model::start(vec![]);
    let mut web = Term::spawn_in(&idle, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    web.keys("/delegate api run the deploy script\r");
    web.wait_for("api keeps it for its user to let in");
    std::thread::sleep(Duration::from_millis(300));
    assert!(api_model.requests().is_empty(), "nothing went in on its own");
    api.keys("/inbox\r");
    api.wait_for("A task for you, handed over by web");
    api.keys("\r");
    api.wait_for("Looking at it.");
    assert!(api_model.requests()[0].to_string().contains("run the deploy script"));
}

#[test]
fn a_rein_in_trust_mode_holds_what_another_tab_sends_until_you_let_it_in() {
    let (base, project, home) = tabs("inbox");
    let api_model = Model::start(vec![says("Hello, front.")]);
    let mut api = Term::spawn_in(&api_model, base.clone(), &project, &home, 30, 100, &["--name", "api", "--mode", "trust"]);
    api.wait_for(IDLE);
    let idle = Model::start(vec![]);
    let mut web = Term::spawn_in(&idle, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    web.keys("/name front\r");
    web.wait_for("other reins call this one front now");
    web.wait_title("○ front");
    web.keys("/send api hello there\r");
    web.wait_for("api keeps it for its user to let in");
    std::thread::sleep(Duration::from_millis(300));
    assert!(api_model.requests().is_empty(), "nothing went in on its own");
    api.keys("/inbox\r");
    api.wait_for("let which in");
    api.wait_for("hello there");
    api.keys("\r");
    api.wait_for("✉ from front");
    api.wait_for("Hello, front.");
    assert!(api_model.requests()[0].to_string().contains("hello there"));
}

#[test]
fn closing_the_tab_takes_the_rein_out_of_the_peers_folder() {
    let (base, project, home) = tabs("hangup");
    let model = Model::start(vec![]);
    let mut api = Term::spawn_in(&model, base.clone(), &project, &home, 30, 100, &["--name", "api"]);
    api.wait_for(IDLE);
    let peers = home.join("peers");
    let files = || std::fs::read_dir(&peers).map(|dir| dir.count()).unwrap_or(0);
    assert_eq!(files(), 2, "its socket and who it is");
    // SAFETY: kill only sends a signal to the rein this test started.
    unsafe { libc::kill(api.child.id() as i32, libc::SIGHUP) };
    api.wait_exit();
    assert_eq!(files(), 0, "nothing is left of it");
}

#[test]
fn the_tab_shows_a_bar_while_rein_works_and_another_while_it_waits_for_you() {
    let model = Model::start(vec![runs("touch made.txt"), says("Made it.")]).with_env("TERM_PROGRAM", "ghostty");
    let mut term = Term::start(&model, "progress", 24, 80);
    term.wait_for(IDLE);
    term.keys("make a file\r");
    term.wait_for("Allow this command");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Made it.");
    term.wait_for(IDLE);
    term.wait_until("the bar to go", |_| term.progress().last().is_some_and(|p| p == "0;0"));
    assert_eq!(term.progress(), ["3;0", "4;100", "3;0", "0;0"], "working, waiting for you, working, done");

    // A terminal that isn't known to draw it is never sent it.
    let plain = Model::start(vec![says("Hi.")]);
    let mut term = Term::start(&plain, "no-progress", 24, 80);
    term.wait_for(IDLE);
    term.keys("hi\r");
    term.wait_for("Hi.");
    assert!(term.progress().is_empty(), "{:?}", term.progress());
}

#[test]
fn in_iterm2_the_tab_shows_the_name_with_its_state_in_colour_under_it() {
    let (base, project, home) = tabs("status");
    let model = Model::start(vec![says("Hello.")]).with_env("TERM_PROGRAM", "iTerm.app").with_env("TERM_PROGRAM_VERSION", "3.7.6");
    let mut api = Term::spawn_in(&model, base.clone(), &project, &home, 30, 100, &["--name", "api"]);
    api.wait_for(IDLE);
    api.wait_title("○ api");
    api.keys("hi\r");
    api.wait_for("Hello.");
    api.wait_for(IDLE);
    api.wait_until("the tab to say done", |_| api.statuses().last().is_some_and(|s| s == "status=done;indicator=#3fb950;status-color=#3fb950"));
    assert!(api.statuses().contains(&"status=working;indicator=#ffa500;status-color=#ffa500".to_owned()), "{:?}", api.statuses());
    api.keys("/exit\r");
    api.wait_exit();
    assert_eq!(api.statuses().last().map(String::as_str), Some("status=;indicator=;status-color="), "leaving clears it");
    assert!(api.progress().contains(&"3;0".to_owned()), "the bar while it works: {:?}", api.progress());
}

#[test]
fn a_strip_across_the_top_shows_every_rein_live() {
    let (base, project, home) = tabs("strip");
    let api_model = Model::start(vec![runs("touch made.txt"), says("Made it.")]);
    let mut api = Term::spawn_in(&api_model, base.clone(), &project, &home, 30, 100, &["--name", "api"]);
    api.wait_for(IDLE);
    assert!(!api.rows().iter().any(|row| row.contains(" api ○")), "alone, there's no strip:\n{}", api.text());
    let idle = Model::start(vec![]);
    let web = Term::spawn_in(&idle, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    web.wait_until("the strip in web", |_| web.rows()[STRIP].starts_with("  api ○    web ○"));
    api.wait_until("the same strip in api", |_| api.rows()[STRIP] == web.rows()[STRIP]);
    // The logo stays over it, its own line given up for the strip's, and
    // a row of space between them.
    assert!(web.rows()[0].starts_with(MARK_TOP) && web.rows()[STRIP - 2].starts_with(MARK_BOTTOM) && web.rows()[STRIP - 1].trim().is_empty(), "{}", web.text());
    // A line under it, heavy only under the tab's own.
    let heavy = |row: &str| row.chars().position(|c| c == '━');
    assert!(web.rows()[STRIP + 1].chars().all(|c| c == '━' || c == '─'), "a line under it: {:?}", web.rows()[STRIP + 1]);
    assert_eq!(heavy(&api.rows()[STRIP + 1]), Some(1), "under api, the first: {:?}", api.rows()[STRIP + 1]);
    assert_eq!(heavy(&web.rows()[STRIP + 1]), Some(1 + " api ○ ".chars().count() + 2), "under web, the second: {:?}", web.rows()[STRIP + 1]);
    api.keys("make a file\r");
    api.wait_for("Allow this command");
    web.wait_until("web to see api needs you", |_| web.rows()[STRIP].contains("api ◆ needs you"));
    std::thread::sleep(Duration::from_millis(700));
    api.keys("y");
    api.wait_for("Made it.");
    web.wait_until("web to see api is done", |_| web.rows()[STRIP].contains("api ✓ done"));
    api.wait_until("the strip in api too", |_| api.rows()[STRIP].starts_with("  api ✓ done") && api.rows()[STRIP].contains("web ○"));
}

/// Three models as Sign in with ChatGPT lists them: one with three levels
/// and a default, one with two, one that takes none.
fn models_with_levels() -> Value {
    json!({ "models": [
        { "slug": "gpt-6.1-sol", "visibility": "list", "default_reasoning_level": "low", "supported_reasoning_levels": [
            { "effort": "low", "description": "Fast responses" }, { "effort": "medium", "description": "Balanced" }, { "effort": "max", "description": "Deepest" },
        ] },
        { "slug": "small", "visibility": "list", "default_reasoning_level": "medium", "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "medium" }] },
        { "slug": "plain", "visibility": "list", "supported_reasoning_levels": [] },
    ] })
}

fn bar_says(term: &mut Term, what: &str) {
    term.wait_until(&format!("the bar says {what}"), |screen| screen.lines().last().is_some_and(|bar| bar.contains(what)));
}

#[test]
fn effort_follows_the_model_its_levels_its_default_and_what_you_chose() {
    let model = Model::start(vec![says("One."), says("Two."), says("Three.")]).with_models(models_with_levels());
    let mut term = Term::start(&model, "efforts", 30, 110);
    term.wait_for(IDLE);
    // Nothing chosen: the bar says what it thinks at, its own default.
    bar_says(&mut term, "gpt-6.1-sol low");
    term.keys("/effort\r");
    term.wait_for("how hard gpt-6.1-sol thinks");
    term.wait_for("Deepest");
    let screen = term.text();
    assert!(screen.contains("the model's own: low") && screen.contains("Fast responses"), "{screen}");
    assert!(!screen.contains("minimal") && !screen.contains("xhigh"), "only its own levels:\n{screen}");
    term.keys("max\r");
    term.wait_for("✓ reasoning effort: max");
    bar_says(&mut term, "gpt-6.1-sol max");
    term.keys("hello\r");
    term.wait_for("One.");
    term.wait_for(IDLE);
    // Another model: your level stays yours, and it gets the closest it has.
    term.keys("/model openai/small\r");
    term.wait_for("small has no max, so it gets medium");
    bar_says(&mut term, "small medium");
    term.keys("again\r");
    term.wait_for("Two.");
    term.wait_for(IDLE);
    // One that takes none has nothing to choose.
    term.keys("/model openai/plain\r");
    term.wait_for("max waits for a model that does");
    term.keys("/effort\r");
    term.wait_for("it thinks as it will");
    // Back where it was: what you chose.
    term.keys("/model openai/gpt-6.1-sol\r");
    bar_says(&mut term, "gpt-6.1-sol max");
    term.keys("once more\r");
    term.wait_for("Three.");
    term.wait_for(IDLE);
    let efforts: Vec<Value> = model.requests().iter().map(|r| r["reasoning"]["effort"].clone()).collect();
    assert_eq!(efforts, [json!("max"), json!("medium"), json!("max")]);
}

#[test]
fn a_level_the_model_hasnt_got_is_said_at_the_start_and_the_closest_sent() {
    let model = Model::start(vec![says("Done.")]).with_models(models_with_levels()).with_env("REIN_EFFORT", "minimal");
    let mut term = Term::start(&model, "effort-start", 24, 100);
    term.wait_for("gpt-6.1-sol has no minimal, so it gets low");
    term.wait_for(IDLE);
    bar_says(&mut term, "gpt-6.1-sol low");
    term.keys("hi\r");
    term.wait_for("Done.");
    assert_eq!(model.requests()[0]["reasoning"]["effort"], "low");
}

#[test]
fn a_level_the_model_turns_down_is_said_once_and_not_offered_again() {
    // A list that says nothing of levels, as with an API key.
    let model = Model::start(vec![says("One."), says("Two.")]).turning_down("minimal").with_env("REIN_EFFORT", "minimal");
    let mut term = Term::start(&model, "turned-down", 30, 110);
    term.wait_for(IDLE);
    bar_says(&mut term, "gpt-6.1-sol minimal");
    term.keys("hello\r");
    term.wait_for("One.");
    term.wait_for("doesn't take effort minimal, so it thought as it does by default");
    term.wait_for(IDLE);
    term.keys("again\r");
    term.wait_for("Two.");
    term.wait_for(IDLE);
    assert_eq!(model.turned_away(), 1, "turned down once, then never sent again");
    assert!(model.requests().iter().all(|r| r["reasoning"]["effort"].is_null()), "{:?}", model.requests());
    assert_eq!(term.text().matches("doesn't take effort minimal").count(), 1, "said once:\n{}", term.text());
    assert!(term.text().lines().last().is_some_and(|bar| !bar.contains("minimal")), "{}", term.text());
    term.keys("/effort\r");
    term.wait_for("how hard gpt-6.1-sol thinks");
    // Its last item, so the list is drawn.
    term.wait_for("the most thinking there is");
    let rows = term.text();
    assert!(!rows.lines().any(|row| row.trim_start_matches(['›', ' ']).starts_with("minimal")), "not offered again:\n{rows}");
    term.keys("\x1b");
}

/// The words of a sub-agent's start script's last line, after `exec env
/// NAME=value…` and the program: rein's arguments, as a new tab would run
/// them.
fn child_args(script: &str) -> Vec<String> {
    let last = script.lines().last().unwrap_or_default();
    let mut words = Vec::new();
    let mut rest = last;
    while let Some(start) = rest.find('\'') {
        let after = &rest[start + 1..];
        let end = after.find('\'').unwrap_or(after.len());
        words.push(after[..end].to_owned());
        rest = &after[(end + 1).min(after.len())..];
    }
    // The variables' values come first, then the program.
    words.into_iter().skip_while(|word| word != env!("CARGO_BIN_EXE_rein")).skip(1).collect()
}

#[test]
fn a_sub_agent_starts_in_a_new_tab_works_where_you_see_it_and_reports_back() {
    let (base, project, home) = tabs("spawn");
    let opened = base.join("opened");
    let spawn = calls("spawn", json!({ "role": "tests", "task": "run the test suite", "model": null, "edits": false }));
    let parent_model = Model::start(vec![spawn, says("Started tests."), says("tests is done: all of them pass.")])
        .with_env("REIN_TAB_COMMAND", &format!("echo {{command}} >> {}", opened.display()));
    let mut web = Term::spawn_in(&parent_model, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    web.keys("get the tests run\r");
    web.wait_for("Start a sub-agent in a new tab?");
    web.wait_for("tests ·");
    std::thread::sleep(Duration::from_millis(700));
    web.keys("y");
    web.wait_for("sub-agent tests started in a new tab");
    web.wait_for("Started tests.");
    // What the "terminal" was asked to run: a script, with no task in it.
    let line = std::fs::read_to_string(&opened).unwrap();
    let script_path = line.trim().strip_prefix("/bin/sh ").unwrap().to_owned();
    let script = std::fs::read_to_string(&script_path).unwrap();
    assert!(!script.contains("run the test suite"), "the task isn't on its command line:\n{script}");
    // A terminal app's tab has the app's PATH, not your shell's: without
    // this one, the sub-agent couldn't find `claude` or `cargo`.
    let path = std::env::var("PATH").unwrap_or_default();
    assert!(script.contains(&format!(" PATH='{path}' ")), "the sub-agent finds programs where its parent does:\n{script}");
    assert!(!script.contains("OPENAI_API_KEY"), "and no key goes with it:\n{script}");
    let args = child_args(&script);
    assert_eq!(&args[..2], ["--name", "tests"], "{args:?}");
    assert!(args.contains(&"--child".to_owned()), "{args:?}");

    let child_model = Model::start(vec![says("All of them pass.")]);
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut tests = Term::spawn_in(&child_model, base.clone(), &project, &home, 30, 100, &words);
    tests.wait_for("A sub-agent of web");
    tests.wait_for("All of them pass.");
    let asked = child_model.requests()[0].to_string();
    assert!(asked.contains("run the test suite") && asked.contains("A task for you"), "{asked}");
    let tools = child_model.requests()[0]["tools"].to_string();
    assert!(tools.contains("\"delegate\"") && !tools.contains("\"spawn\""), "a sub-agent can't start sub-agents: {tools}");
    web.wait_for("tests is done: all of them pass.");
    assert!(parent_model.requests()[2].to_string().contains("All of them pass."), "the result came back");
    // Its result read, the sub-agent goes, and its tab with it.
    tests.wait_exit();
}

#[test]
fn a_sub_agent_you_say_no_to_never_opens() {
    let (base, project, home) = tabs("decline");
    let opened = base.join("opened");
    let spawn = calls("spawn", json!({ "role": "docs", "task": "write the docs", "model": null, "edits": false }));
    let parent_model = Model::start(vec![spawn, says("Started docs."), says("docs is done.")]).with_env("REIN_TAB_COMMAND", &format!("echo {{command}} >> {}", opened.display()));
    let mut web = Term::spawn_in(&parent_model, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    web.keys("write docs\r");
    web.wait_for("Start a sub-agent in a new tab?");
    std::thread::sleep(Duration::from_millis(700));
    web.keys("n");
    web.wait_for("Started docs.");
    assert!(!opened.exists(), "no means no tab");
    assert!(parent_model.requests()[1].to_string().contains("The user declined to start this sub-agent."));
}

#[test]
fn a_sub_agent_you_typed_into_stays_when_its_parent_is_done_with_it() {
    let (base, project, home) = tabs("adopt");
    std::fs::create_dir_all(project.join(".rein")).unwrap();
    std::fs::write(project.join(ALLOWED.0), ALLOWED.1).unwrap();
    let opened = base.join("opened");
    let spawn = calls("spawn", json!({ "role": "lint", "task": "lint it", "model": null, "edits": false }));
    let parent_model = Model::start(vec![spawn, says("Started lint."), says("lint is done.")]).with_env("REIN_TAB_COMMAND", &format!("echo {{command}} >> {}", opened.display()));
    let mut web = Term::spawn_in(&parent_model, base.clone(), &project, &home, 30, 100, &["--name", "web"]);
    web.wait_for(IDLE);
    web.keys("lint it\r");
    web.wait_for("Start a sub-agent in a new tab?");
    std::thread::sleep(Duration::from_millis(700));
    web.keys("y");
    web.wait_for("Started lint.");
    let script = std::fs::read_to_string(std::fs::read_to_string(&opened).unwrap().trim().strip_prefix("/bin/sh ").unwrap()).unwrap();
    let args = child_args(&script);
    let child_model =
        Model::start(vec![calls("bash", json!({ "command": "sleep 1", "timeout_seconds": null, "access": "write", "reason": null })), says("Linted, and the tests too.")]);
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut lint = Term::spawn_in(&child_model, base.clone(), &project, &home, 30, 100, &words);
    lint.wait_for("Running sleep 1");
    lint.keys("run the tests too\r");
    lint.wait_for("Linted, and the tests too.");
    web.wait_for("lint is done.");
    lint.wait_for("it's yours now");
    std::thread::sleep(Duration::from_millis(500));
    assert!(lint.child.try_wait().unwrap().is_none(), "it stays: you typed into it");
}

#[test]
fn spawn_starts_a_sub_agent_you_name_without_asking() {
    let (base, project, home) = tabs("slash-spawn");
    let opened = base.join("opened");
    let lead_model = Model::start(vec![says("tests says they pass.")]).with_env("REIN_TAB_COMMAND", &format!("echo {{command}} >> {}", opened.display()));
    let mut lead = Term::spawn_in(&lead_model, base.clone(), &project, &home, 30, 100, &["--name", "lead"]);
    lead.wait_for(IDLE);
    lead.keys("/spawn tests run the test suite\r");
    lead.wait_for("sub-agent tests started in a new tab");
    assert!(!lead.text().contains("Start a sub-agent in a new tab?"), "you asked for it: nothing to ask");
    let script = std::fs::read_to_string(std::fs::read_to_string(&opened).unwrap().trim().strip_prefix("/bin/sh ").unwrap()).unwrap();
    let args = child_args(&script);
    let child_model = Model::start(vec![says("They pass.")]);
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut tests = Term::spawn_in(&child_model, base.clone(), &project, &home, 30, 100, &words);
    tests.wait_for("They pass.");
    assert!(child_model.requests()[0].to_string().contains("run the test suite"));
    lead.wait_for("tests says they pass.");
    tests.wait_exit();
}

/// Asks `/mcp` until it shows `text`, as a server takes a moment to start,
/// leaving the list of servers to pick from each time with Esc.
fn wait_for_mcp(term: &mut Term, text: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        term.keys("/mcp\r");
        term.wait_for(PICKING);
        let shown = term.text().contains(text);
        term.keys("\x1b");
        term.wait_for(IDLE);
        if shown {
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("/mcp never showed {text:?}. The screen:\n{}", term.text());
}

/// What the box says while a list waits for a pick.
const PICKING: &str = "type to narrow the list";

/// Picks, in the list on screen, the item `typed` narrows it to.
fn pick(term: &mut Term, typed: &str) {
    term.wait_for(PICKING);
    term.keys(typed);
    term.wait_until(&format!("the list narrowed to {typed:?}"), |screen| !screen.contains(PICKING));
    term.keys("\r");
}

/// Your own MCP server starts with rein, in the sandbox, and its tools are
/// the model's from the first message: one your mcp.json allows runs, the
/// rest ask, and the server can't read your home folder.
#[cfg(target_os = "macos")]
#[test]
fn an_mcp_server_of_yours_starts_in_the_sandbox_and_its_tools_ask_unless_allowed() {
    let secret = std::env::temp_dir().join(format!("rein-screen-{}-mcp-yours", std::process::id())).join("home/.ssh/id_test");
    let calls = Turn {
        out: vec![
            Out::Call("mcp__fake__echo", json!({ "text": "hi" })),
            Out::Call("mcp__fake__has", json!({ "name": "OPENAI_API_KEY" })),
            Out::Call("mcp__fake__peek", json!({ "path": secret })),
        ],
    };
    let mut model = Model::start(vec![calls, says("Both done.")]);
    model.home.push(("mcp.json".into(), json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"], "allow": ["echo", "has"] } } }).to_string()));
    model.home.push((".ssh/id_test".into(), "FAKE-mcp-screen-secret\n".into()));
    let mut term = Term::start_with(&model, "mcp-yours", 30, 110, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    term.keys("use the server\r");
    term.wait_for("Let fake's peek run?");
    // Keys too soon after a question still go to the box.
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Both done.");
    let text = term.text();
    assert!(text.contains("MCP servers: fake (5 tools)"), "{text}");
    assert!(text.contains(r#"MCP    fake: echo {"text":"hi"}"#), "{text}");
    assert_eq!(results(&model), ["hi", "unset", "blocked"], "echo and has ran without asking; rein's key isn't the server's; peek asked, and couldn't read yours");
    let tools = model.requests()[0]["tools"][0]["tools"].as_array().cloned().unwrap_or_default();
    let echo = tools.iter().find(|tool| tool["name"] == "mcp__fake__echo").expect("offered from the first request");
    assert_eq!(echo["strict"], false, "a server's schema isn't held to strict mode");
    wait_for_mcp(&mut term, "● 5 tools · sandbox (network)");
}

/// A server whose tools are held back is named in the tool search, not
/// shown: the model finds the tool it needs, which is added to its tools
/// where it found it, and calls it. `/mcp` says they're found when needed.
#[cfg(target_os = "macos")]
#[test]
fn a_held_back_servers_tool_is_found_with_the_search_and_then_called() {
    let mut model = Model::start(vec![calls("tool_search", json!({ "query": "echo it back", "limit": null })), calls("mcp__fake__echo", json!({ "text": "hi" })), says("Echoed.")]);
    model.home.push(("mcp.json".into(), json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"], "allow": ["echo"], "tools": "search" } } }).to_string()));
    let mut term = Term::start_with(&model, "mcp-found", 30, 110, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    term.keys("echo hi\r");
    term.wait_for("Echoed.");
    let text = term.text();
    assert!(text.contains("MCP servers: fake (5 tools, found when needed)"), "{text}");
    assert!(text.contains("Find tools echo it back"), "{text}");
    assert!(text.contains("found fake: echo"), "{text}");
    let requests = model.requests();
    assert!(servers_tools(&requests[0]).is_empty(), "none shown: {:?}", servers_tools(&requests[0]));
    let tools = requests[0]["tools"][0]["tools"].as_array().cloned().unwrap_or_default();
    let search = tools.iter().find(|tool| tool["name"] == "tool_search").expect("the search, which rein runs");
    assert!(search["description"].as_str().unwrap().contains("- fake: echo, "), "{search}");
    let input = requests[1]["input"].as_array().cloned().unwrap_or_default();
    let added = input.iter().find(|item| item["type"] == "additional_tools").expect("what it found, added where it found it");
    assert_eq!(added["tools"][0]["tools"][0]["name"], "mcp__fake__echo");
    let input = requests[2]["input"].as_array().cloned().unwrap_or_default();
    assert!(input.iter().any(|item| item["type"] == "function_call" && item["name"] == "mcp__fake__echo" && item["namespace"] == "mcp"), "called where it was loaded");
    assert_eq!(results(&model), ["Found: mcp__fake__echo. Call them as any other tool.", "hi"]);
    assert!(requests.iter().all(|request| request["tools"] == requests[0]["tools"]), "the same tools all conversation, for the cache");
    wait_for_mcp(&mut term, "● 5 tools · found when needed");
}

/// `/mcp` changes when the model is shown a server's tools: picked, it's
/// kept in your mcp.json, and the next message holds them back.
#[cfg(target_os = "macos")]
#[test]
fn mcp_chooses_when_the_model_is_shown_a_servers_tools() {
    let mut model = Model::start(vec![says("Shown."), says("Held back.")]);
    model.home.push(("mcp.json".into(), json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"] } } }).to_string()));
    let mut term = Term::start_with(&model, "mcp-offered", 30, 110, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    term.keys("one\r");
    term.wait_for("Shown.");
    assert!(term.text().contains("MCP servers: fake (5 tools)"), "a few small tools: shown\n{}", term.text());
    term.wait_for(IDLE);
    term.keys("/mcp\r");
    pick(&mut term, "fake");
    term.wait_for("now: from the start");
    pick(&mut term, "choose when");
    term.wait_for("fake: show the model its tools when");
    pick(&mut term, "once it finds");
    term.wait_for("✓ the model is shown fake's tools once it finds them, from your next message");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(term.dir.join("home/mcp.json")).unwrap()).unwrap();
    assert_eq!(file["mcpServers"]["fake"], json!({ "command": "sh", "args": ["server.sh"], "tools": "search" }), "kept in your file, the rest as it was");
    term.wait_for(IDLE);
    term.keys("two\r");
    term.wait_for("Held back.");
    let requests = model.requests();
    assert!(servers_tools(&requests[0]).contains(&"mcp__fake__echo".to_owned()));
    assert!(servers_tools(&requests[1]).is_empty(), "held back from the next message: {:?}", servers_tools(&requests[1]));
    let names: Vec<Value> = requests[1]["tools"][0]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].clone()).collect();
    assert!(names.contains(&json!("tool_search")));
    wait_for_mcp(&mut term, "● 5 tools · found when needed");
}

/// The model asks rein's `web_search`; the model in use searches for it,
/// from OpenAI's index outside trust, and the step says what it went by.
/// `/search off` takes the tool away, kept in your settings.
#[test]
fn web_search_runs_on_the_model_in_use_and_says_what_it_went_by() {
    let page = "https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/";
    let turns = vec![
        calls("web_search", json!({ "query": "the latest stable Rust release" })),
        Turn { out: vec![Out::Cited("Rust 1.99.1, released 2026-10-15.", page)] },
        says("It's 1.99.1."),
        says("No search now."),
    ];
    // Not OpenAI's default model: the one in use is the one that searches.
    let model = Model::start(turns).with_env("REIN_MODEL", "test-model");
    let mut term = Term::start_with(&model, "web-search", 30, 110, &[]);
    term.wait_for(IDLE);
    term.keys("what's the latest rust?\r");
    term.wait_for("It's 1.99.1.");
    let text = term.text();
    assert!(text.contains("the latest stable Rust release"), "{text}");
    assert!(text.contains("1 source: blog.rust-lang.org · test-model, cached"), "{text}");
    assert!(text.contains("Rust 1.99.1, released 2026-10-15."), "{text}");
    let requests = model.requests();
    let offered: Vec<Value> = requests[0]["tools"][0]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].clone()).collect();
    assert!(offered.contains(&json!("web_search")), "{offered:?}");
    assert_eq!(requests[1]["tools"], json!([{ "type": "web_search", "external_web_access": false }]), "a search of its own, from the index: not trust");
    assert_eq!(requests[1]["model"], "test-model");
    assert!(requests[1]["input"][0]["content"].as_str().unwrap().starts_with("the latest stable Rust release\n\n(Today is "));
    assert!(results(&model)[0].starts_with("[web search by test-model, its provider's index:"), "{:?}", results(&model));
    term.wait_for(IDLE);
    term.keys("/search off\r");
    term.wait_for("✓ the model can't search the web now");
    assert!(std::fs::read_to_string(term.dir.join("home/config")).unwrap().contains("search = off"));
    term.keys("and now?\r");
    term.wait_for("No search now.");
    let offered: Vec<Value> = model.requests()[3]["tools"][0]["tools"].as_array().unwrap().iter().map(|tool| tool["name"].clone()).collect();
    assert!(!offered.contains(&json!("web_search")), "taken away: {offered:?}");
}

/// A 1×1 PNG, as a screenshot tool would send a bigger one.
const DOT: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0d, 0x49,
    0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc, 0xff, 0x9f, 0xa1, 0x1e, 0, 0x07, 0x82, 0x02, 0x7f, 0x3d, 0xc8, 0x48, 0xef, 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
    0x82,
];

/// An image a tool gives reaches the model inside that tool's result: a
/// file `read` opens, and a server's screenshot. The conversation's file
/// names where each is kept, never holding its bytes.
#[cfg(target_os = "macos")]
#[test]
fn an_image_a_tool_gives_reaches_the_model_inside_its_result() {
    let turn =
        Turn { out: vec![Out::Call("read", json!({ "path": "dot.png", "start_line": null, "line_count": null })), Out::Call("mcp__fake__echo", json!({ "text": "picture" }))] };
    let mut model = Model::start(vec![turn, says("Two dots.")]);
    model.home.push(("mcp.json".into(), json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"], "allow": ["echo"], "access": "outside" } } }).to_string()));
    let mut term = Term::start_with(&model, "images", 30, 110, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    std::fs::write(term.dir.join("project/dot.png"), DOT).unwrap();
    term.wait_for(IDLE);
    term.keys("what's in them?\r");
    term.wait_for("Two dots.");
    let text = term.text();
    assert!(text.contains("Read   dot.png  image, 1×1"), "{text}");
    let requests = model.requests();
    let input = requests[1]["input"].as_array().cloned().unwrap_or_default();
    let outputs: Vec<&Value> = input.iter().filter(|item| item["type"] == "function_call_output").map(|item| &item["output"]).collect();
    assert_eq!(outputs.len(), 2, "{input:?}");
    for output in outputs {
        let parts = output.as_array().unwrap_or_else(|| panic!("an image in it: {output}"));
        let image = parts.iter().find(|part| part["type"] == "input_image").unwrap();
        assert!(image["image_url"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR"));
    }
    let sessions = term.dir.join("home/sessions");
    let mut kept = Vec::new();
    let mut stack = vec![sessions];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() { stack.push(entry.path()) } else { kept.push(entry.path()) }
        }
    }
    let conversation = kept.iter().find(|path| path.extension().is_some_and(|e| e == "jsonl")).expect("the conversation's file");
    let saved = std::fs::read_to_string(conversation).unwrap();
    assert!(saved.contains("\"type\":\"image\"") && !saved.contains("iVBOR"), "named, never its bytes: {saved}");
    assert_eq!(kept.iter().filter(|path| path.extension().is_some_and(|e| e == "png")).count(), 1, "the same dot kept once: {kept:?}");
}

/// Ctrl-V pastes an image from the clipboard; over SSH, where the
/// clipboard is on the other computer, rein says so, and what's typed stays.
#[test]
fn ctrl_v_over_ssh_says_the_clipboard_is_elsewhere() {
    let model = Model::start(Vec::new());
    let mut term = Term::start_with(&model, "paste-ssh", 30, 110, &[]);
    term.wait_for(IDLE);
    term.keys("look at ");
    term.keys("\x16");
    term.wait_for("nothing pasted: the clipboard is on the computer you're connecting from");
    assert!(term.text().contains("look at"), "what's typed stays\n{}", term.text());
    assert!(model.requests().is_empty(), "nothing sent");
}

/// Your skills are offered to the model, which loads one when a task fits;
/// `/name` calls one with your message. A project's are used only once
/// you've said yes to them.
#[test]
fn skills_are_loaded_by_the_model_and_called_by_name() {
    let load = calls("skill", json!({ "name": "graphs", "file": null, "start_line": null }));
    let mut model = Model::start(vec![load, says("Drawn with boxes."), says("Reviewed.")]);
    model.home.push((".agents/skills/graphs/SKILL.md".into(), "---\nname: graphs\ndescription: Draw a project's modules as a graph.\n---\nUse boxes and arrows.\n".into()));
    let skill = "---\nname: review\ndescription: How this project is reviewed.\n---\nCheck the tests.\n";
    let mut term = Term::start_with(&model, "skills", 30, 110, &[(".agents/skills/review/SKILL.md", skill)]);
    term.wait_for("Use this project's skills: review?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for(IDLE);
    term.keys("draw the modules\r");
    term.wait_for("Drawn with boxes.");
    let text = term.text();
    assert!(text.contains("Skill  graphs  loaded, 1 line"), "{text}");
    let requests = model.requests();
    let tools = requests[0]["tools"][0]["tools"].as_array().cloned().unwrap_or_default();
    let tool = tools.iter().find(|tool| tool["name"] == "skill").expect("offered");
    assert_eq!(tool["parameters"]["properties"]["name"]["enum"], json!(["review", "graphs"]), "the project's first, once said yes to");
    assert!(tool["description"].as_str().unwrap().contains("- review (the project's): How this project is reviewed."));
    assert!(results(&model)[0].contains("Use boxes and arrows."));
    term.wait_for(IDLE);
    term.keys("/review the last change\r");
    term.wait_for("Reviewed.");
    let input = model.requests()[2]["input"].as_array().cloned().unwrap_or_default();
    assert!(
        input.iter().any(|item| item["type"] == "function_call_output" && item["output"].as_str().is_some_and(|o| o.contains("Check the tests."))),
        "called with the message: {input:?}"
    );
    let saved = std::fs::read_to_string(term.dir.join("home/skills-trusted.json")).unwrap();
    assert!(saved.contains("project"), "the yes kept: {saved}");
}

/// A project's .mcp.json may be anyone's: its server starts only once
/// you've said yes to it as it is, and is asked about again once it
/// changes.
#[cfg(target_os = "macos")]
#[test]
fn a_projects_mcp_server_starts_only_once_you_say_yes_to_it_as_it_is() {
    let model = Model::start(Vec::new());
    let file = |args: Value| json!({ "mcpServers": { "fake": { "command": "sh", "args": args } } }).to_string();
    let mut term = Term::start_with(&model, "mcp-project", 30, 110, &[("server.sh", rein_mcp::FAKE_SERVER), (".mcp.json", &file(json!(["server.sh"])))]);
    term.wait_for("Start fake, an MCP server this project's .mcp.json names?");
    term.wait_for("sh server.sh");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("n");
    term.wait_for(IDLE);
    // Said no, it waits in /mcp for a yes, without starting rein again.
    term.keys("/mcp\r");
    term.wait_for("⏸ waiting for your yes · the project's");
    pick(&mut term, "fake");
    pick(&mut term, "start");
    term.wait_for("Start fake, an MCP server");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("fake  5 tools: echo");
    term.wait_for(IDLE);
    term.keys("/exit\r");

    let mut term = term.again(&model);
    term.wait_for(IDLE);
    wait_for_mcp(&mut term, "● 5 tools");
    assert!(!term.text().contains("Start fake"), "said yes to as it is: not asked again\n{}", term.text());
    term.keys("/exit\r");

    std::fs::write(term.dir.join("project/.mcp.json"), file(json!(["server.sh", "--changed"]))).unwrap();
    let term = term.again(&model);
    term.wait_for("Start fake, an MCP server");
    term.wait_for("sh server.sh --changed");
}

/// The names of the servers' tools a request offered.
fn servers_tools(request: &Value) -> Vec<String> {
    let mut names = Vec::new();
    let mut stack = vec![&request["tools"]];
    while let Some(value) = stack.pop() {
        match value {
            Value::Array(items) => stack.extend(items),
            Value::Object(fields) => {
                names.extend(fields.get("name").and_then(Value::as_str).filter(|name| name.starts_with("mcp__")).map(str::to_owned));
                stack.extend(fields.get("tools"));
            }
            _ => {}
        }
    }
    names
}

/// `/mcp add` writes your mcp.json and starts the server, saying how it
/// did; `/mcp allow` lets a tool run without asking, and `/mcp remove`
/// stops it: each from the next message on, with nothing to edit by hand.
#[cfg(target_os = "macos")]
#[test]
fn mcp_add_allow_and_remove_change_the_servers_without_editing_a_file() {
    let model = Model::start(vec![Turn { out: vec![Out::Call("mcp__fake__echo", json!({ "text": "hi" }))] }, says("Echoed."), says("No tools now.")]);
    let mut term = Term::start_with(&model, "mcp-add", 30, 130, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    term.keys("/mcp\r");
    term.wait_for("No MCP servers yet. /mcp add <name> looks one up in the MCP Registry");
    // "+ add" takes the JSON a server's README gives, pasted as it is.
    pick(&mut term, "add");
    term.wait_for("A name to look up in the MCP Registry, a URL, a command, or JSON from a README:");
    term.keys("\x1b[200~{ \"mcpServers\": {\n  \"fake\": { \"command\": \"sh\", \"args\": [\"server.sh\"] }\n} }\x1b[201~");
    term.keys("\r");
    term.wait_for("✓ fake added to ~/mcp.json");
    term.wait_for("fake  5 tools: echo, peek, given, has, quit");
    term.wait_for(IDLE);
    term.keys("/mcp allow fake nope\r");
    term.wait_for("fake has no tool called nope; its tools are echo, peek, given, has, quit");
    term.keys("/mcp allow fake echo\r");
    term.wait_for("✓ these of fake's tools run: echo without asking now");
    term.wait_for(IDLE);
    let path = term.dir.join("home/mcp.json");
    let file = || serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file(), json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"], "allow": ["echo"] } } }));

    term.keys("use it\r");
    term.wait_for("Echoed.");
    assert!(!term.text().contains("Let fake's echo run?"), "allowed: not asked\n{}", term.text());
    assert_eq!(results(&model), ["hi"]);
    term.wait_for(IDLE);
    term.keys("/mcp remove fake\r");
    term.wait_for("✓ fake is stopped, and out of ~/mcp.json");
    term.keys("and now?\r");
    term.wait_for("No tools now.");
    let requests = model.requests();
    assert!(servers_tools(&requests[0]).contains(&"mcp__fake__echo".to_owned()), "{:?}", servers_tools(&requests[0]));
    assert_eq!(servers_tools(&requests[2]), Vec::<String>::new(), "removed: offered no more");
    assert_eq!(file(), json!({ "mcpServers": {} }));
}

/// A server that doesn't start in the sandbox is offered to run outside
/// it, there and then: a yes writes it in your mcp.json and starts it, and
/// nothing needs typing again.
#[cfg(target_os = "macos")]
#[test]
fn a_server_that_fails_in_the_sandbox_is_offered_to_run_outside_it() {
    let model = Model::start(Vec::new());
    let mut term = Term::start_with(&model, "mcp-outside", 30, 130, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    std::fs::create_dir_all(term.dir.join("home/.ssh")).unwrap();
    std::fs::write(term.dir.join("home/.ssh/id_test"), "FAKE-mcp-outside-secret").unwrap();
    term.wait_for(IDLE);
    // It reads its settings in your home folder as it starts.
    let starts = format!("cat {} > /dev/null && exec sh server.sh", term.dir.join("home/.ssh/id_test").display());
    term.keys(&format!("/mcp add fake sh -c '{starts}'\r"));
    term.wait_for("fake  didn't start: it stopped");
    term.wait_for("Run fake outside the sandbox?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("fake  5 tools: echo");
    term.wait_for(IDLE);
    let file: Value = serde_json::from_str(&std::fs::read_to_string(term.dir.join("home/mcp.json")).unwrap()).unwrap();
    assert_eq!(file["mcpServers"]["fake"]["access"], "outside", "{file}");
    wait_for_mcp(&mut term, "● 5 tools · outside the sandbox");
}

/// `/mcp` lists the servers to pick one and change it: which of its tools
/// run without asking, on and off, each kept in your mcp.json.
#[cfg(target_os = "macos")]
#[test]
fn mcp_lets_you_pick_a_server_and_change_it_without_typing_its_name() {
    let mut model = Model::start(Vec::new());
    model.home.push(("mcp.json".into(), json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"] } } }).to_string()));
    let mut term = Term::start_with(&model, "mcp-panel", 30, 130, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    wait_for_mcp(&mut term, "● 5 tools");
    let path = term.dir.join("home/mcp.json");
    let file = || serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap()["mcpServers"]["fake"].clone();

    term.keys("/mcp\r");
    pick(&mut term, "fake");
    term.wait_for("5 tools: echo, peek, given, has, quit");
    assert!(term.text().contains("fake sh server.sh"), "picked, what it runs:\n{}", term.text());
    pick(&mut term, "choose the tools");
    term.wait_for("fake: which tools run without asking");
    pick(&mut term, "echo");
    term.wait_until("echo marked", |screen| screen.contains("✓ echo"));
    pick(&mut term, "has");
    term.wait_until("has marked", |screen| screen.contains("✓ has"));
    pick(&mut term, "done");
    term.wait_for("✓ these of fake's tools run: echo, has without asking now");
    term.wait_for(IDLE);
    assert_eq!(file()["allow"], json!(["echo", "has"]));

    term.keys("/mcp\r");
    pick(&mut term, "fake");
    pick(&mut term, "turn it off");
    term.wait_for("✓ fake is off: it stays in ~/mcp.json, and /mcp turns it on");
    term.wait_for(IDLE);
    assert_eq!(file()["disabled"], true);
    term.keys("/mcp\r");
    term.wait_for("– off · sandbox (network)");
    pick(&mut term, "fake");
    pick(&mut term, "turn it on");
    term.wait_for("fake  5 tools: echo");
    term.wait_for(IDLE);
    assert_eq!(file().get("disabled"), None, "on again: {}", file());
    assert_eq!(file()["allow"], json!(["echo", "has"]), "what it let run is kept");
}

/// `/mcp` lists each server once, in the list to pick from, a line each
/// cut to the window: how it's doing, its tools counted, and where it runs.
/// Picked, it shows its tools by name, or why it didn't start in full.
#[cfg(target_os = "macos")]
#[test]
fn mcp_lists_each_server_once_a_line_each_cut_to_the_window() {
    let mut model = Model::start(Vec::new());
    let gone = "./Some App.app/Contents/SharedSupport/Helper.app/Contents/MacOS/Helper";
    model
        .home
        .push(("mcp.json".into(), json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"] }, "gone": { "command": gone, "access": "outside" } } }).to_string()));
    let mut term = Term::start_with(&model, "mcp-lines", 30, 60, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    wait_for_mcp(&mut term, "● 5 tools");
    term.keys("/mcp\r");
    term.wait_for(PICKING);
    let text = term.text();
    term.keys("\x1b");
    term.wait_for(IDLE);
    assert!(!text.contains("yours ·") && !text.contains("mcp__<server>"), "no table above the list: it would say it all twice\n{text}");
    let line = text.lines().rfind(|line| line.contains("gone")).unwrap_or_else(|| panic!("{text}"));
    assert!(line.trim_end().ends_with('…') && line.contains("✗ its command"), "one line, cut: {line:?}\n{text}");
    let fake = text.lines().rfind(|line| line.contains("fake")).unwrap_or_default();
    assert!(fake.trim_end().ends_with("● 5 tools · sandbox (network)"), "its tools counted, not named, and where it runs: {fake:?}");
    term.keys("/mcp\r");
    pick(&mut term, "gone");
    term.wait_until("why, in full", |screen| screen.replace('\n', "").contains("Helper, isn't there"));
}

/// A server that needs a variable you haven't set asks for it as rein
/// starts, in dots: kept in rein's folder, filled in from then on, and
/// hidden wherever it shows up.
#[cfg(target_os = "macos")]
#[test]
fn a_key_a_server_needs_is_asked_for_kept_and_hidden() {
    let mut model = Model::start(vec![Turn { out: vec![Out::Call("mcp__fake__given", json!({}))] }, says("Got it.")]);
    let server = json!({ "command": "sh", "args": ["server.sh"], "env": { "GIVEN": "${REIN_SCREEN_FAKE_TOKEN}" }, "allow": ["given"] });
    model.home.push(("mcp.json".into(), json!({ "mcpServers": { "fake": server } }).to_string()));
    let mut term = Term::start_with(&model, "mcp-key", 30, 130, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for("fake needs REIN_SCREEN_FAKE_TOKEN to start, and it isn't set.");
    term.wait_for("Paste REIN_SCREEN_FAKE_TOKEN");
    term.keys("FAKE-mcp-token-456");
    term.wait_until("the key in dots", |screen| !screen.contains("FAKE-mcp-token-456") && screen.contains("••••"));
    term.keys("\r");
    term.wait_for("✓ REIN_SCREEN_FAKE_TOKEN is kept for fake");
    term.wait_for(IDLE);
    term.keys("what's given?\r");
    term.wait_for("Got it.");
    assert_eq!(results(&model), ["[a key rein hides]"]);
    term.keys("/exit\r");

    let mut term = term.again(&model);
    term.wait_for(IDLE);
    wait_for_mcp(&mut term, "● 5 tools");
    assert!(!term.text().contains("needs REIN_SCREEN_FAKE_TOKEN"), "kept: not asked again\n{}", term.text());
    term.keys("\x04");
    term.wait_exit();
    assert!(!model.requests().iter().any(|request| request.to_string().contains("FAKE-mcp-token-456")), "the model never saw it");
    for file in every_file(&term.dir) {
        let has = std::fs::read_to_string(&file).unwrap_or_default().contains("FAKE-mcp-token-456");
        assert_eq!(has, file == term.dir.join("home/mcp-keys.json"), "{} {} the key", file.display(), if has { "has" } else { "doesn't have" });
    }
}

/// A folder with a stand-in for `npx` in it, which starts the project's
/// fake server whatever it's asked to fetch: so a server added from the
/// registry starts without the internet, as it would with it.
fn fake_npx(name: &str) -> (PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let bin = std::env::temp_dir().join(format!("rein-screen-{}-{name}-bin", std::process::id()));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("npx"), "#!/bin/sh\nexec sh server.sh\n").unwrap();
    std::fs::set_permissions(bin.join("npx"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    (bin, path)
}

/// `/mcp add <word>` looks the server up in the MCP Registry, shows who
/// published each it finds, the real one first, asks for what it needs,
/// shows what it runs, and once you say yes, adds it pinned to its version
/// and starts it, its key asked for in dots and kept out of your mcp.json.
#[cfg(target_os = "macos")]
#[test]
fn a_server_is_added_by_name_from_the_mcp_registry() {
    let package = |id: &str| {
        json!([{ "registryType": "npm", "identifier": id, "version": "1.0.0", "transport": { "type": "stdio" },
        "environmentVariables": [{ "name": "ACME_TOKEN", "isSecret": true, "isRequired": true }, { "name": "ACME_SITE", "isRequired": true, "description": "your site" }] }])
    };
    let page = json!({ "servers": [
        { "server": { "name": "io.github.copycat/thing-mcp", "description": "Not the one", "packages": package("@copycat/thing-mcp") } },
        { "server": { "name": "io.github.acme/thing-mcp", "description": "Acme's own", "repository": { "url": "https://github.com/acme/thing-mcp" }, "packages": package("@thing/mcp") } },
        { "server": { "name": "ai.smithery/thing", "remotes": [{ "type": "streamable-http", "url": "https://x.example/mcp" }] } }
    ] });
    let (bin, path) = fake_npx("registry");
    let model = Model::start(Vec::new()).with_registry(page).with_env("PATH", &path);
    let mut term = Term::start_with(&model, "mcp-registry", 34, 140, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    // Alone, /mcp add asks: a name is looked up.
    term.keys("/mcp add\r");
    term.wait_for("A name to look up in the MCP Registry");
    term.keys("thing\r");
    term.wait_for("add which thing server");
    term.wait_for("smithery.ai/thing");
    let list = term.text();
    let (real, copy) = (list.find("acme/thing-mcp").unwrap(), list.find("copycat/thing-mcp").unwrap());
    assert!(real > copy, "the one whose package is its own comes first, right over the box:\n{list}");
    assert!(list.contains("npm @thing/mcp@1.0.0 · Acme's own"), "{list}");
    assert!(list.contains("at x.example"), "one reached at a URL is listed too:\n{list}");
    term.keys("\r");
    term.wait_for("thing needs ACME_SITE (your site):");
    term.keys("https://acme.example\r");
    term.wait_for("Add acme/thing-mcp as thing?");
    term.wait_for("Published by github.com/acme");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("thing needs ACME_TOKEN to start, and it isn't set.");
    term.keys("FAKE-acme-token-321\r");
    term.wait_for("thing  5 tools: echo");
    term.wait_for(IDLE);
    let file: Value = serde_json::from_str(&std::fs::read_to_string(term.dir.join("home/mcp.json")).unwrap()).unwrap();
    assert_eq!(
        file["mcpServers"]["thing"],
        json!({ "command": "npx", "args": ["-y", "@thing/mcp@1.0.0"], "env": { "ACME_SITE": "https://acme.example", "ACME_TOKEN": "${ACME_TOKEN}" } }),
        "pinned, and the key a variable of yours"
    );
    assert!(std::fs::read_to_string(term.dir.join("home/mcp-keys.json")).unwrap().contains("FAKE-acme-token-321"));
    assert!(model.heads().iter().any(|head| head.starts_with("GET /v0.1/servers?search=%2Fthing&")), "{:?}", model.heads());
    let _ = std::fs::remove_dir_all(bin);
}

/// `/mcp import` finds the servers you set up in other apps, offers each
/// to bring in, one another app has off left unmarked, and adds those you
/// keep, each app's way of naming your variables read as rein's.
#[cfg(target_os = "macos")]
#[test]
fn mcp_import_brings_your_servers_from_other_apps() {
    let mut model = Model::start(Vec::new());
    let cursor = json!({ "mcpServers": { "fake": { "command": "sh", "args": ["server.sh"], "env": { "GIVEN": "${env:REIN_IMPORT_GIVEN}" } }, "far": { "url": "http://127.0.0.1:9/mcp" }, "old": { "type": "sse", "url": "http://127.0.0.1:9/sse" }, "a__b": { "command": "x" } } });
    model.home.push((".cursor/mcp.json".into(), cursor.to_string()));
    model.home.push((".codex/config.toml".into(), "[mcp_servers.other]\ncommand = \"sh\"\nargs = [\"server.sh\"]\nenabled = false\n".into()));
    let mut term = Term::start_with(&model, "mcp-import", 34, 140, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    term.keys("/mcp import\r");
    term.wait_for("a__b from Cursor isn't brought in: a server's name may have letters");
    term.wait_for("bring in which servers");
    let list = term.text();
    assert!(list.contains("✓ fake") && list.contains("from Cursor · sh server.sh (with GIVEN)"), "{list}");
    assert!(list.contains("✓ far") && list.contains("from Cursor · http://127.0.0.1:9/mcp"), "one at a URL is brought in too\n{list}");
    assert!(list.contains("✓ old") && list.contains("from Cursor · http://127.0.0.1:9/sse"), "and one over SSE\n{list}");
    assert!(list.contains("  other") && list.contains("off in Codex"), "off there: not marked\n{list}");
    pick(&mut term, "done");
    term.wait_for("fake needs REIN_IMPORT_GIVEN to start, and it isn't set.");
    term.keys("FAKE-import-given\r");
    term.wait_for("fake  5 tools: echo");
    term.wait_for(IDLE);
    let file: Value = serde_json::from_str(&std::fs::read_to_string(term.dir.join("home/mcp.json")).unwrap()).unwrap();
    assert_eq!(file["mcpServers"]["fake"], json!({ "command": "sh", "args": ["server.sh"], "env": { "GIVEN": "${REIN_IMPORT_GIVEN}" } }));
    assert!(file["mcpServers"].get("other").is_none(), "left unmarked: not brought in");
}

/// A server at a URL that wants a sign-in is offered one as it's added:
/// rein registers itself, you sign in in the browser, it starts, and its
/// tools are the model's. The token is kept only in rein's folder, and
/// never reaches the model.
#[cfg(target_os = "macos")]
#[test]
fn a_server_at_a_url_is_signed_in_to_in_the_browser_and_its_tools_work() {
    let model = Model::start(vec![Turn { out: vec![Out::Call("mcp__far__hello", json!({}))] }, says("Far said hello.")]);
    let base = model.base.trim_end_matches("/v1").to_owned();
    // Wide, so the link to sign in is on one line.
    let mut term = Term::start(&model, "mcp-far", 34, 420);
    term.wait_for(IDLE);
    term.keys(&format!("/mcp add far {base}/mcp\r"));
    term.wait_for("Sign in to far?");
    term.wait_for(&format!("rein opens {base} in your browser"));
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Open this link to sign in:");
    term.wait_until("the link", |screen| screen.contains("/authorize?"));
    let text = term.text();
    let line = text.lines().find(|line| line.contains("/authorize?")).unwrap();
    let link = rein_llm::reqwest::Url::parse(line[line.find("http").unwrap()..].trim()).unwrap();
    let param = |name: &str| link.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned()).unwrap_or_default();
    assert_eq!((param("client_id"), param("code_challenge_method"), param("resource")), ("rein-far-client".to_owned(), "S256".to_owned(), format!("{base}/mcp")));
    let back = rein_llm::reqwest::Url::parse(&param("redirect_uri")).unwrap();
    let mut browser = TcpStream::connect(("127.0.0.1", back.port().unwrap())).unwrap();
    write!(browser, "GET {}?code=c1&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n", back.path(), param("state")).unwrap();
    let mut page = String::new();
    browser.read_to_string(&mut page).unwrap();
    assert!(page.contains("rein is signed in"), "{page}");
    term.wait_for("✓ signed in to far");
    term.wait_for("far  1 tool: hello");
    term.wait_for(IDLE);
    term.keys("say hello\r");
    term.wait_for("Let far's hello run?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Far said hello.");
    assert_eq!(results(&model), ["hello from far away"]);
    let file: Value = serde_json::from_str(&std::fs::read_to_string(term.dir.join("home/mcp.json")).unwrap()).unwrap();
    assert_eq!(file["mcpServers"]["far"], json!({ "type": "http", "url": format!("{base}/mcp") }));
    term.keys("\x04");
    term.wait_exit();
    assert!(!model.requests().iter().any(|request| request.to_string().contains("FAKE-far-token")), "the model never saw it");
    for file in every_file(&term.dir) {
        let has = std::fs::read_to_string(&file).unwrap_or_default().contains("FAKE-far-token");
        assert_eq!(has, file == term.dir.join("home/mcp-tokens.json"), "{} {} the token", file.display(), if has { "has" } else { "doesn't have" });
    }
}

/// A server added with a key written in is kept in your mcp.json and
/// nowhere else: the line isn't shown again or kept for ↑, and what the
/// server says has the key hidden.
#[cfg(target_os = "macos")]
#[test]
fn a_server_added_with_its_key_keeps_it_out_of_the_screen_history_and_conversation() {
    let model = Model::start(vec![Turn { out: vec![Out::Call("mcp__fake__given", json!({}))] }, says("Got it.")]);
    let mut term = Term::start_with(&model, "mcp-add-key", 30, 130, &[("server.sh", rein_mcp::FAKE_SERVER)]);
    term.wait_for(IDLE);
    term.keys("/mcp\r");
    term.wait_for("No MCP servers yet");
    term.wait_for(PICKING);
    term.keys("\x1b");
    term.wait_for(IDLE);
    term.keys("/mcp add fake GIVEN=postgres://me:FAKE-mcp-pw@db/x sh server.sh\r");
    term.wait_for("fake  5 tools");
    term.wait_for(IDLE);
    assert!(term.text().contains("you    /mcp add … (what may be a key hidden)"), "said again, without it:\n{}", term.text());
    term.keys("\x1b[A");
    term.wait_until("the line before it in the box", |_| term.in_box() == "/mcp");
    term.keys("\x15");
    term.keys("what's given?\r");
    term.wait_for("Let fake's given run?");
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("Got it.");
    assert_eq!(results(&model), ["[a key rein hides]"]);
    assert!(!term.text().contains("FAKE-mcp-pw"), "{}", term.text());
    term.keys("\x04");
    term.wait_exit();
    assert!(!model.requests().iter().any(|request| request.to_string().contains("FAKE-mcp-pw")), "the model never saw it");
    for file in every_file(&term.dir) {
        let has = std::fs::read_to_string(&file).unwrap_or_default().contains("FAKE-mcp-pw");
        assert_eq!(has, file == term.dir.join("home/mcp.json"), "{} {} the key", file.display(), if has { "has" } else { "doesn't have" });
    }
}
