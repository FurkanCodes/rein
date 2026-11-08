//! rein's tools, served to Claude Code as an MCP server on this computer,
//! so the model calls them as it calls any tool of its own: told only to
//! answer in a JSON shape that named them, it wrote calls as text, and a
//! tool found partway through was a list in words. rein runs every call
//! itself (its rules, the sandbox, `/undo`): a call is held here until rein
//! has, then answered with what the tool gave.
//!
//! Plain HTTP on 127.0.0.1, the way an MCP server at a URL is spoken to
//! (`POST` a JSON-RPC message, `GET` a stream of what the server says by
//! itself), and only with the token rein gave the CLI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;

use crate::Result;

/// The longest request head taken, and the longest body: a call's
/// arguments can hold a whole file to write.
const LONGEST_HEAD: u64 = 64 * 1024;
const LONGEST_BODY: usize = 64 * 1024 * 1024;

/// How long a call that found tools waits for the CLI to read the new
/// list, so the model can call them in its next step.
const RELISTED: Duration = Duration::from_secs(3);

/// The server, for as long as it's kept: dropped, it stops listening, and
/// every call still held is answered as stopped.
pub(super) struct Bridge {
    url: String,
    shared: Arc<Shared>,
    _serving: JoinSet<()>,
}

struct Shared {
    token: String,
    tools: Mutex<Vec<Value>>,
    calls: Mutex<HashMap<String, Call>>,
    /// Counts each change to the tools: every open stream says it.
    version: watch::Sender<u64>,
    /// The version the CLI last read the list at.
    listed: watch::Sender<u64>,
}

/// A call of the model's, by its id.
enum Call {
    /// The CLI asked for it and waits for this.
    Waiting(oneshot::Sender<Value>),
    /// rein's answer came first: the CLI asks for it next.
    Answered(Value),
}

impl Bridge {
    /// Serves `tools` (MCP tool definitions) on a port of its own.
    pub(super) async fn start(tools: Vec<Value>) -> Result<Bridge> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
        let shared =
            Arc::new(Shared { token: crate::oauth::random_token()?, tools: Mutex::new(tools), calls: Mutex::default(), version: watch::channel(0).0, listed: watch::channel(0).0 });
        let mut serving = JoinSet::new();
        serving.spawn(accept(listener, shared.clone()));
        Ok(Bridge { url, shared, _serving: serving })
    }

    /// The CLI's `--mcp-config`: this server, as `rein`, with its token.
    pub(super) fn config(&self) -> Value {
        json!({ "mcpServers": { "rein": { "type": "http", "url": self.url, "headers": { "Authorization": format!("Bearer {}", self.shared.token) } } } })
    }

    /// Offers `tools` from now on, telling the CLI if they changed. Returns
    /// whether they did.
    pub(super) fn set_tools(&self, tools: Vec<Value>) -> bool {
        let mut offered = lock(&self.shared.tools);
        if *offered == tools {
            return false;
        }
        *offered = tools;
        drop(offered);
        self.shared.version.send_modify(|version| *version += 1);
        true
    }

    /// Waits, a little, for the CLI to read the tools as they are now.
    pub(super) async fn relisted(&self) {
        let now = *self.shared.version.borrow();
        let mut listed = self.shared.listed.subscribe();
        let _ = tokio::time::timeout(RELISTED, listed.wait_for(|at| *at >= now)).await;
    }

    /// What the call `id` gave: MCP content, for the CLI to hand the model.
    pub(super) fn answer(&self, id: &str, content: Vec<Value>) {
        let result = json!({ "content": content, "isError": false });
        let mut calls = lock(&self.shared.calls);
        match calls.remove(id) {
            Some(Call::Waiting(waiting)) => {
                let _ = waiting.send(result);
            }
            _ => {
                calls.insert(id.to_owned(), Call::Answered(result));
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    // Each connection is a task of this one's: they stop with it.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => {
                    connections.spawn(serve(socket, shared.clone()));
                }
                Err(_) => return,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

/// One connection's requests, in turn, until it closes.
async fn serve(socket: TcpStream, shared: Arc<Shared>) {
    let (read, mut write) = socket.into_split();
    let mut read = BufReader::new(read);
    while let Some(asked) = read_request(&mut read).await {
        let sent = if !shared.allows(asked.authorization.as_deref()) {
            reply(&mut write, 401, &[]).await
        } else {
            match asked.method.as_str() {
                "POST" => match shared.answer(&asked.body).await {
                    Some(body) => reply(&mut write, 200, &body).await,
                    None => reply(&mut write, 202, &[]).await,
                },
                "GET" => return stream(&mut write, &shared).await,
                "DELETE" => reply(&mut write, 200, &[]).await,
                _ => reply(&mut write, 405, &[]).await,
            }
        };
        if sent.is_err() {
            return;
        }
    }
}

impl Shared {
    /// Whether `authorization` carries the token, compared in full.
    fn allows(&self, authorization: Option<&str>) -> bool {
        let Some(given) = authorization.and_then(|value| value.strip_prefix("Bearer ")) else { return false };
        given.len() == self.token.len() && given.bytes().zip(self.token.bytes()).fold(0, |differ, (a, b)| differ | (a ^ b)) == 0
    }

    /// The answer to the JSON-RPC message `body`; `None` for a notification,
    /// which gets none.
    async fn answer(&self, body: &[u8]) -> Option<Vec<u8>> {
        let Ok(message) = serde_json::from_slice::<Value>(body) else {
            return Some(failure(&Value::Null, -32700, "that isn't JSON"));
        };
        let id = message.get("id")?.clone();
        let params = &message["params"];
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "rein", "version": env!("CARGO_PKG_VERSION") },
            }),
            "ping" => json!({}),
            "tools/list" => {
                let tools = lock(&self.tools).clone();
                let now = *self.version.borrow();
                self.listed.send_replace(now);
                json!({ "tools": tools })
            }
            "tools/call" => self.call(params).await,
            _ => return Some(failure(&id, -32601, "rein has no such method")),
        };
        serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).ok()
    }

    /// Holds the call until rein has run it. Claude Code names which of the
    /// model's calls it is (`claudecode/toolUseId`), the id rein gave it.
    async fn call(&self, params: &Value) -> Value {
        let Some(id) = params["_meta"]["claudecode/toolUseId"].as_str() else {
            return failed("rein couldn't tell which of the model's calls this is");
        };
        let waiting = {
            let mut calls = lock(&self.calls);
            if let Some(Call::Answered(result)) = calls.remove(id) {
                return result;
            }
            let (answer, waiting) = oneshot::channel();
            calls.insert(id.to_owned(), Call::Waiting(answer));
            waiting
        };
        waiting.await.unwrap_or_else(|_| failed("rein stopped before this call ran"))
    }
}

/// A call's result that says it failed, and why.
fn failed(why: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": why }], "isError": true })
}

fn failure(id: &Value, code: i64, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })).unwrap_or_default()
}

/// What the CLI says by itself goes to it this way: that the tools changed.
async fn stream(write: &mut OwnedWriteHalf, shared: &Shared) {
    if write.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n").await.is_err() {
        return;
    }
    let mut version = shared.version.subscribe();
    loop {
        let said = tokio::select! {
            changed = version.changed() => {
                if changed.is_err() {
                    return;
                }
                format!("event: message\ndata: {}\n\n", json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }))
            }
            // A comment now and then, so a stream nobody's on is noticed.
            () = tokio::time::sleep(Duration::from_secs(15)) => ": ping\n\n".to_owned(),
        };
        if write.write_all(said.as_bytes()).await.is_err() {
            return;
        }
    }
}

struct Asked {
    method: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

/// The next request on a connection; `None` once it closes or sends what
/// isn't one.
async fn read_request(read: &mut BufReader<OwnedReadHalf>) -> Option<Asked> {
    let mut head = (&mut *read).take(LONGEST_HEAD);
    let mut line = String::new();
    if head.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let method = line.split_whitespace().next()?.to_owned();
    let (mut length, mut authorization) = (0usize, None);
    loop {
        line.clear();
        if head.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().ok()?;
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_owned());
        }
    }
    if length > LONGEST_BODY {
        return None;
    }
    let mut body = vec![0; length];
    read.read_exact(&mut body).await.ok()?;
    Some(Asked { method, authorization, body })
}

async fn reply(write: &mut OwnedWriteHalf, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        _ => "Method Not Allowed",
    };
    let kind = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    write.write_all(format!("HTTP/1.1 {status} {reason}\r\n{kind}Content-Length: {}\r\n\r\n", body.len()).as_bytes()).await?;
    write.write_all(body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_tool() -> Value {
        json!({ "name": "read", "description": "Read a file.", "inputSchema": { "type": "object" } })
    }

    async fn post(bridge: &Bridge, token: &str, message: Value) -> reqwest::Response {
        reqwest::Client::new().post(&bridge.url).bearer_auth(token).json(&message).send().await.unwrap()
    }

    fn token(bridge: &Bridge) -> String {
        bridge.shared.token.clone()
    }

    /// `message` sent in a task of its own, as the CLI waits on a call.
    fn sent_aside(bridge: &Bridge, message: Value) -> tokio::task::JoinHandle<Value> {
        let (url, token) = (bridge.url.clone(), token(bridge));
        tokio::spawn(async move { reqwest::Client::new().post(url).bearer_auth(token).json(&message).send().await.unwrap().json().await.unwrap() })
    }

    #[tokio::test]
    async fn serves_the_tools_and_holds_a_call_until_rein_answers_it() {
        let bridge = Bridge::start(vec![read_tool()]).await.unwrap();
        let init = post(&bridge, &token(&bridge), json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).await;
        let init: Value = init.json().await.unwrap();
        assert_eq!((init["result"]["protocolVersion"].as_str(), init["result"]["capabilities"]["tools"]["listChanged"].as_bool()), (Some("2025-11-25"), Some(true)));
        let noted = post(&bridge, &token(&bridge), json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        assert_eq!(noted.status(), 202);
        let listed: Value = post(&bridge, &token(&bridge), json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await.json().await.unwrap();
        assert_eq!(listed["result"]["tools"], json!([read_tool()]));

        let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read","arguments":{"path":"a.rs"},"_meta":{"claudecode/toolUseId":"toolu_a"}}});
        let held = sent_aside(&bridge, call);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!held.is_finished(), "held until rein has run it");
        bridge.answer("toolu_a", vec![json!({"type":"text","text":"fn main() {}"})]);
        let answered = held.await.unwrap();
        assert_eq!(answered["result"]["content"][0]["text"], "fn main() {}");

        // Answered before the CLI asks (the second call of a step): kept.
        bridge.answer("toolu_b", vec![json!({"type":"text","text":"second"})]);
        let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read","arguments":{},"_meta":{"claudecode/toolUseId":"toolu_b"}}});
        let answered: Value = post(&bridge, &token(&bridge), call).await.json().await.unwrap();
        assert_eq!(answered["result"]["content"][0]["text"], "second");
    }

    #[tokio::test]
    async fn without_its_token_nothing_is_served() {
        let bridge = Bridge::start(vec![read_tool()]).await.unwrap();
        let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        for wrong in ["", "nope", &token(&bridge)[1..]] {
            assert_eq!(post(&bridge, wrong, list.clone()).await.status(), 401, "{wrong:?}");
        }
        let unsent = reqwest::Client::new().post(&bridge.url).json(&list).send().await.unwrap();
        assert_eq!(unsent.status(), 401);
        let config = bridge.config();
        assert_eq!(config["mcpServers"]["rein"]["headers"]["Authorization"], format!("Bearer {}", token(&bridge)));
    }

    #[tokio::test]
    async fn a_change_to_the_tools_is_said_on_the_stream_and_waited_for() {
        let bridge = Bridge::start(vec![read_tool()]).await.unwrap();
        let mut stream = reqwest::Client::new().get(&bridge.url).bearer_auth(token(&bridge)).send().await.unwrap();
        assert_eq!(stream.headers()["content-type"], "text/event-stream");
        assert!(!bridge.set_tools(vec![read_tool()]), "the same tools are no change");
        assert!(bridge.set_tools(vec![read_tool(), json!({"name":"weather"})]));
        let said = tokio::time::timeout(Duration::from_secs(5), stream.chunk()).await.unwrap().unwrap().unwrap();
        assert!(String::from_utf8_lossy(&said).contains("notifications/tools/list_changed"), "{said:?}");
        let waited = tokio::spawn({
            let shared = bridge.shared.clone();
            async move {
                let started = std::time::Instant::now();
                let now = *shared.version.borrow();
                shared.listed.subscribe().wait_for(|at| *at >= now).await.unwrap();
                started.elapsed()
            }
        });
        let listed: Value = post(&bridge, &token(&bridge), json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await.json().await.unwrap();
        assert_eq!(listed["result"]["tools"][1]["name"], "weather");
        assert!(waited.await.unwrap() < RELISTED, "read before the wait ran out");
        tokio::time::timeout(Duration::from_secs(1), bridge.relisted()).await.expect("already read: no wait");
    }

    #[tokio::test]
    async fn a_held_call_is_answered_as_stopped_when_rein_lets_go() {
        let bridge = Bridge::start(vec![read_tool()]).await.unwrap();
        let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read","arguments":{},"_meta":{"claudecode/toolUseId":"toolu_a"}}});
        let held = sent_aside(&bridge, call);
        tokio::time::sleep(Duration::from_millis(100)).await;
        lock(&bridge.shared.calls).clear();
        let answered = held.await.unwrap();
        assert_eq!(answered["result"]["isError"], true);
        let unnamed = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read","arguments":{}}});
        let answered: Value = post(&bridge, &token(&bridge), unnamed).await.json().await.unwrap();
        assert_eq!(answered["result"]["isError"], true, "a call it can't place is turned down, not guessed");
        let unknown: Value = post(&bridge, &token(&bridge), json!({"jsonrpc":"2.0","id":"x","method":"server/discover"})).await.json().await.unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }
}
