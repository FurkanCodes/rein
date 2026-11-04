//! MCP's Streamable HTTP transport (spec 2025-11-25), the client's side:
//! each message rein sends is a POST to the server's one URL, and the
//! answer comes back as one JSON object, or as a stream of events (SSE)
//! that ends with it, the server's own requests on the way. A server may
//! give a session ID as it says hello, which goes with each request after;
//! once it's gone (404), a new session is started.
//!
//! The HTTP is rein's, given as a [`Web`]: an HTTP client brings C with it,
//! and this crate has none, so its tests build for Linux from a Mac.
//! Sign-ins are rein's too, given as [`Tokens`], asked for before each
//! request: a token that runs out is renewed, never kept from the start.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::rpc::{self, Error};

/// A future a [`Web`] or [`Tokens`] gives back.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The most of one answer rein reads: an image fits, and a server that
/// never stops sending doesn't fill rein's memory.
const LONGEST: usize = 8 << 20;

/// How many times an answer whose stream ended before it came is picked
/// up again, where the server said it was.
const RESUMES: usize = 2;

/// The longest rein waits to pick a stream up again, whatever the server
/// says to wait.
const LONGEST_RETRY: Duration = Duration::from_secs(5);

/// How long a message that wants no answer may take to be taken.
const TOLD: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// An HTTP request for a [`Web`] to send.
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// Never what the headers say: one is a key.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("Request").field("method", &self.method).field("url", &crate::config::shown_url(&self.url)).field("headers", &names).finish_non_exhaustive()
    }
}

/// What a [`Web`] got back: its head, and its body to read.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Body>,
}

impl Response {
    /// The header called `name`, whatever its letters' case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(own, _)| own.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

/// An answer's body, as it comes.
pub trait Body: Send {
    /// The next part of it: `None` once it's all come.
    fn chunk(&mut self) -> Fut<'_, Result<Option<Vec<u8>>, String>>;
}

/// What sends rein's HTTP requests.
pub trait Web: Send + Sync {
    /// Sends `request`: the answer once its head has come.
    fn send(&self, request: Request) -> Fut<'_, Result<Response, String>>;
}

/// The sign-ins rein keeps for servers reached at a URL, by their URL.
pub trait Tokens: Send + Sync {
    /// The access token to send the server at `url` now, renewed first if
    /// it's about to run out: `None` if you haven't signed in to it.
    fn token<'a>(&'a self, url: &'a str) -> Fut<'a, Result<Option<String>, String>>;
    /// The server at `url` turned `rejected` down: a token to try instead,
    /// renewed without you, or one another rein window renewed meanwhile;
    /// `None` if you need to sign in again.
    fn renew<'a>(&'a self, url: &'a str, rejected: &'a str) -> Fut<'a, Result<Option<String>, String>>;
}

/// Whether rein may reach `url`: over https, or over plain http only on
/// this computer, where what's sent, a key, crosses no network.
pub fn reachable(url: &str) -> Result<(), String> {
    let Some((scheme, rest)) = url.split_once("://") else { return Err("its URL should start with https://".into()) };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" if !host.is_empty() => Ok(()),
        "http" if ["localhost", "127.0.0.1", "::1"].contains(&host.to_ascii_lowercase().as_str()) => Ok(()),
        "http" => Err("rein reaches a server over plain http only on this computer, where nothing crosses the network; its URL should start with https://".into()),
        _ => Err("its URL should start with https://".into()),
    }
}

/// A session with a server at a URL.
pub(crate) struct Session {
    web: Arc<dyn Web>,
    tokens: Option<Arc<dyn Tokens>>,
    url: String,
    headers: Vec<(String, String)>,
    /// You gave it no `Authorization` of your own: it gets rein's sign-in.
    signs_in: bool,
    /// The ID the server gave the session, if it did.
    id: Mutex<Option<String>>,
    /// The protocol version said hello in, sent with each request after.
    version: Mutex<Option<String>>,
    next: AtomicU64,
    /// Why the session ended, once it has.
    ended: Mutex<Option<String>>,
}

impl Session {
    /// A session with the server at `url`, sent `headers` with each
    /// request: both filled in already.
    pub(crate) fn new(web: Arc<dyn Web>, tokens: Option<Arc<dyn Tokens>>, url: String, headers: BTreeMap<String, String>) -> Session {
        let signs_in = !headers.keys().any(|name| name.eq_ignore_ascii_case("authorization"));
        let headers = headers.into_iter().collect();
        Session { web, tokens, url, headers, signs_in, id: Mutex::default(), version: Mutex::default(), next: AtomicU64::new(1), ended: Mutex::default() }
    }

    pub(crate) fn alive(&self) -> bool {
        locked(&self.ended).is_none()
    }

    /// Hello was said in `version`.
    pub(crate) fn speaks(&self, version: &str) {
        *locked(&self.version) = Some(version.to_owned());
    }

    /// Sends `method` with `params` and waits up to `timeout` for the
    /// answer. Given up on, the server is told: over HTTP, a request let go
    /// of isn't cancelled.
    pub(crate) async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Error> {
        let ended = locked(&self.ended).clone();
        if let Some(why) = ended {
            return Err(Error::Stopped(why));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        if let Ok(answer) = tokio::time::timeout(timeout, self.exchange(&body, Some(id))).await {
            return answer;
        }
        let cancel = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id, "reason": "rein stopped waiting for it" } });
        let _ = tokio::time::timeout(TOLD, self.exchange(&cancel.to_string(), None)).await;
        Err(Error::TimedOut { what: method.to_owned(), after: timeout })
    }

    /// Sends `method`, which wants no answer.
    pub(crate) async fn notify(&self, method: &str, params: Option<Value>) {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        let _ = tokio::time::timeout(TOLD, self.exchange(&message.to_string(), None)).await;
    }

    /// The token to send now, if rein signs in for it.
    async fn token(&self) -> Result<Option<String>, Error> {
        match (&self.tokens, self.signs_in) {
            (Some(tokens), true) => tokens.token(&self.url).await.map_err(Error::Unreachable),
            _ => Ok(None),
        }
    }

    /// Sends `body`, the message with `id` or one that wants no answer,
    /// and reads what comes back. A token the server turns down is renewed
    /// once, if it can be without you.
    async fn exchange(&self, body: &str, id: Option<u64>) -> Result<Value, Error> {
        let mut renewed = false;
        loop {
            let token = self.token().await?;
            let response = self.send(Method::Post, Some(body), token.as_deref(), None).await?;
            if let Some(given) = response.header("mcp-session-id").filter(|given| !given.is_empty() && given.bytes().all(|b| (0x21..=0x7e).contains(&b))) {
                locked(&self.id).get_or_insert_with(|| given.to_owned());
            }
            match response.status {
                401 | 403 => {
                    let challenge = response.header("www-authenticate").unwrap_or_default().to_owned();
                    // A 403 asks for more only when it says so; and with a
                    // key of yours, signing in isn't what would help.
                    if !self.signs_in || (response.status == 403 && !challenge.contains("insufficient_scope")) {
                        return Err(failed(response).await);
                    }
                    if response.status == 401
                        && !renewed
                        && let (Some(token), Some(tokens)) = (&token, &self.tokens)
                    {
                        renewed = true;
                        if tokens.renew(&self.url, token).await.map_err(Error::Unreachable)?.is_some() {
                            continue;
                        }
                    }
                    return Err(Error::Unauthorized(challenge));
                }
                404 if locked(&self.id).is_some() => {
                    *locked(&self.ended) = Some("its session ended".into());
                    return Err(Error::SessionEnded);
                }
                200..=299 => {
                    let Some(id) = id else { return Ok(Value::Null) };
                    return self.answer(response, id).await;
                }
                _ => return Err(failed(response).await),
            }
        }
    }

    /// Sends a request to the server's URL, with what each one carries.
    async fn send(&self, method: Method, body: Option<&str>, token: Option<&str>, from: Option<&str>) -> Result<Response, Error> {
        let mut headers: Vec<(String, String)> = vec![("Accept".into(), if body.is_some() { "application/json, text/event-stream".into() } else { "text/event-stream".into() })];
        if body.is_some() {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        let (version, id) = (locked(&self.version).clone(), locked(&self.id).clone());
        if let Some(version) = version {
            headers.push(("MCP-Protocol-Version".into(), version));
        }
        if let Some(id) = id {
            headers.push(("MCP-Session-Id".into(), id));
        }
        if let Some(from) = from {
            headers.push(("Last-Event-ID".into(), from.to_owned()));
        }
        if let Some(token) = token {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        headers.extend(self.headers.iter().cloned());
        let request = Request { method, url: self.url.clone(), headers, body: body.map(str::to_owned) };
        self.web.send(request).await.map_err(Error::Unreachable)
    }

    /// The answer to the request `id`, as one object or a stream of events.
    async fn answer(&self, response: Response, id: u64) -> Result<Value, Error> {
        let kind = response.header("content-type").unwrap_or_default().to_ascii_lowercase();
        if kind.starts_with("text/event-stream") {
            return self.stream(response.body, id).await;
        }
        if !kind.starts_with("application/json") {
            return Err(Error::Unreadable(format!("it answered as {}, not JSON", crate::clean::text(&kind))));
        }
        let text = read_all(response.body).await?;
        let message: Value = serde_json::from_str(&text).map_err(|e| Error::Unreadable(format!("its answer: {e}")))?;
        match self.take(&message, id).await {
            Some(answer) => answer,
            None => Err(Error::Unreadable("its answer wasn't to what rein asked".into())),
        }
    }

    /// Reads events until the answer to `id`. A stream that ends before it
    /// is picked up again where the server said it was, as many times as
    /// [`RESUMES`].
    async fn stream(&self, mut body: Box<dyn Body>, id: u64) -> Result<Value, Error> {
        let (mut events, mut last, mut wait, mut resumes) = (Events::default(), None::<String>, Duration::from_secs(1), 0);
        loop {
            let Some(bytes) = body.chunk().await.map_err(Error::Unreachable)? else {
                let Some(from) = last.clone().filter(|_| resumes < RESUMES) else { return Err(Error::Stopped("its answer ended before it came".into())) };
                resumes += 1;
                tokio::time::sleep(wait).await;
                body = self.resume(&from).await?;
                events = Events::default();
                continue;
            };
            for event in events.push(&bytes)? {
                if let Some(id) = event.id {
                    last = Some(id);
                }
                if let Some(ms) = event.retry {
                    wait = Duration::from_millis(ms).min(LONGEST_RETRY);
                }
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else { continue };
                if let Some(answer) = self.take(&message, id).await {
                    return answer;
                }
            }
        }
    }

    /// The stream that ended, picked up again after the event `from`.
    async fn resume(&self, from: &str) -> Result<Box<dyn Body>, Error> {
        let token = self.token().await?;
        let response = self.send(Method::Get, None, token.as_deref(), Some(from)).await?;
        let streams = response.header("content-type").is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
        if (200..300).contains(&response.status) && streams {
            Ok(response.body)
        } else {
            Err(Error::Stopped(format!("its answer ended before it came, and couldn't be picked up again ({})", response.status)))
        }
    }

    /// What `message` is: the answer to `id`, or else a request of the
    /// server's, answered here, or something else, let be.
    async fn take(&self, message: &Value, id: u64) -> Option<Result<Value, Error>> {
        if let Some(reply) = rpc::reply(message) {
            // Its answer is the server's to take; whether it does isn't rein's.
            if let Ok(token) = self.token().await {
                let _ = self.send(Method::Post, Some(&reply.to_string()), token.as_deref(), None).await;
            }
            return None;
        }
        rpc::answer(message).filter(|(answered, _)| *answered == id).map(|(_, answer)| answer)
    }
}

/// The error a response that isn't an answer is: its status, and the start
/// of what it said, cleaned.
pub(crate) async fn failed(response: Response) -> Error {
    let status = response.status;
    let said = read_all(response.body).await.unwrap_or_default();
    let said: String = crate::clean::text(said.trim()).chars().take(300).collect();
    Error::Http { status, said }
}

/// All of `body`, as long as it's no longer than [`LONGEST`].
pub(crate) async fn read_all(mut body: Box<dyn Body>) -> Result<String, Error> {
    let mut all = Vec::new();
    while let Some(bytes) = body.chunk().await.map_err(Error::Unreachable)? {
        all.extend_from_slice(&bytes);
        if all.len() > LONGEST {
            return Err(Error::Unreadable("an answer too long to read".into()));
        }
    }
    Ok(String::from_utf8_lossy(&all).into_owned())
}

/// Server-sent events as they come, a line at a time.
#[derive(Default)]
pub(crate) struct Events {
    line: Vec<u8>,
    /// The last line ended in `\r`: a `\n` after it ends nothing more.
    after_cr: bool,
    data: String,
    id: Option<String>,
    retry: Option<u64>,
    kind: Option<String>,
    /// The event under way has had a field.
    begun: bool,
}

/// One event: its data, and what it said of the stream.
pub(crate) struct Event {
    pub(crate) data: String,
    id: Option<String>,
    retry: Option<u64>,
    /// What it says it is (`event: endpoint`): a message when it says not.
    pub(crate) kind: Option<String>,
}

impl Events {
    /// The events `bytes` ends.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>, Error> {
        let mut ended = Vec::new();
        for &byte in bytes {
            if std::mem::take(&mut self.after_cr) && byte == b'\n' {
                continue;
            }
            if byte == b'\n' || byte == b'\r' {
                self.after_cr = byte == b'\r';
                let line = std::mem::take(&mut self.line);
                ended.extend(self.field(&line)?);
            } else {
                self.line.push(byte);
                if self.line.len() > LONGEST {
                    return Err(Error::Unreadable("an event too long to read".into()));
                }
            }
        }
        Ok(ended)
    }

    /// Takes one line: an event, if it ends one.
    fn field(&mut self, line: &[u8]) -> Result<Option<Event>, Error> {
        if line.is_empty() {
            if !std::mem::take(&mut self.begun) {
                return Ok(None);
            }
            return Ok(Some(Event { data: std::mem::take(&mut self.data), id: self.id.take(), retry: self.retry.take(), kind: self.kind.take() }));
        }
        let line = String::from_utf8_lossy(line);
        if line.starts_with(':') {
            return Ok(None);
        }
        let (name, value) = line.split_once(':').map_or((line.as_ref(), ""), |(name, value)| (name, value.strip_prefix(' ').unwrap_or(value)));
        match name {
            "data" => {
                if self.begun && !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                if self.data.len() > LONGEST {
                    return Err(Error::Unreadable("an event too long to read".into()));
                }
            }
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            "retry" => self.retry = value.parse().ok(),
            "event" => self.kind = Some(value.to_owned()),
            _ => {}
        }
        self.begun = true;
        Ok(None)
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) mod tests;
