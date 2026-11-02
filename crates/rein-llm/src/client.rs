//! One client for every provider: what differs is the wire format ([`Api`]),
//! where requests go, and how they're signed.
//!
//! Every request carries the whole conversation, so a long one sends the
//! same hundreds of kilobytes again and again. Bodies are gzipped on the way
//! (`Content-Encoding: gzip`), which makes rein's JSON about three times
//! smaller; the model reads the same text either way, as the server unpacks
//! it first. A server that turns a compressed request away, like many a
//! local one, gets it again uncompressed, and nothing compressed after that.
//!
//! Servers that speak a format don't all take every field of it. When one
//! turns a request away naming a field rein can do without (a server that
//! knows no `reasoning_effort`, say), the request goes again without it,
//! and that field stays out of this client's requests from then on. No
//! table of which server takes what to keep up to date: the server says.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::chat::Quirks;
use crate::decode::Decode;
use crate::{Api, Error, Event, ModelInfo, Request, Result, anthropic, chat, gemini, openai};

/// How requests prove who's asking. Its `Debug` never shows the secret,
/// so neither can a log line, a `dbg!` or a panic message.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// Nothing, as for a server on this computer.
    None,
    /// `Authorization: Bearer <token>`: OpenAI's keys and sign-ins, gateways.
    Bearer(String),
    /// A header of the provider's own, like Anthropic's `x-api-key`.
    Header(&'static str, String),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::None => f.write_str("Auth::None"),
            Auth::Bearer(_) => f.write_str("Auth::Bearer(..)"),
            Auth::Header(name, _) => write!(f, "Auth::Header({name:?}, ..)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api: Api,
    base_url: String,
    auth: Auth,
    /// What this provider needs that it can't say in an error.
    quirks: Quirks,
    /// How long an answer may go quiet before rein gives up on it.
    idle: Duration,
    /// How requests went over the wire, shared by the client's clones.
    wire: Arc<Wire>,
}

/// How long a streaming answer may send nothing at all. Providers send
/// something (text, thinking, or a keep-alive) far more often than this
/// while they work; Codex gives up after the same time.
const IDLE: Duration = Duration::from_secs(300);

/// Bodies smaller than this go as they are: there'd be little to save.
const COMPRESS_FROM: usize = 1024;

#[derive(Debug, Default)]
struct Wire {
    /// Compression was turned off.
    off: AtomicBool,
    /// The server turned a compressed request away, so it's off.
    refused: AtomicBool,
    /// Request bodies' bytes as JSON, and as they were sent.
    json: AtomicU64,
    sent: AtomicU64,
    /// Fields the server turned away, left out from then on.
    omitted: Mutex<Vec<&'static str>>,
}

/// What requests have cost on the wire so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Traffic {
    /// The request bodies' bytes, as JSON.
    pub json: u64,
    /// The bytes that went: fewer, once compressed.
    pub sent: u64,
    pub compression: Compression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Gzip,
    /// Turned off by the user.
    Off,
    /// The server didn't take a compressed request.
    Refused,
}

/// The fields a format can do without: the word (in lower case) a server's
/// error would name it by, its key in the body, and what rein calls leaving
/// it out.
pub(crate) type Optional = &'static [(&'static str, &'static str, &'static str)];

impl Client {
    /// A client for the server at `base_url` (like `https://api.anthropic.com/v1`),
    /// which speaks `api`.
    pub fn new(http: reqwest::Client, api: Api, base_url: impl Into<String>, auth: Auth) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        Client { http, api, base_url, auth, quirks: Quirks::default(), idle: IDLE, wire: Arc::default() }
    }

    /// Gives up on an answer that sends nothing for `idle`.
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    pub fn with_quirks(mut self, quirks: Quirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// OpenAI itself: `bearer` is an API key or a Sign in with ChatGPT
    /// access token, which go in the same header; only who pays differs.
    pub fn openai(http: reqwest::Client, bearer: impl Into<String>) -> Self {
        Client::new(http, Api::Responses, openai::API_BASE, Auth::Bearer(bearer.into()))
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends request bodies as they are, uncompressed.
    pub fn without_compression(self) -> Self {
        self.wire.off.store(true, Ordering::Relaxed);
        self
    }

    pub fn traffic(&self) -> Traffic {
        let compression = match (self.wire.refused.load(Ordering::Relaxed), self.wire.off.load(Ordering::Relaxed)) {
            (true, _) => Compression::Refused,
            (false, true) => Compression::Off,
            (false, false) => Compression::Gzip,
        };
        Traffic { json: self.wire.json.load(Ordering::Relaxed), sent: self.wire.sent.load(Ordering::Relaxed), compression }
    }

    /// The fields this client leaves out, because the server turned them away.
    pub fn omitted(&self) -> Vec<&'static str> {
        self.wire.omitted.lock().map(|o| o.clone()).unwrap_or_default()
    }

    /// The models this credential can use, in the server's order.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        match self.api {
            Api::Responses => openai::models(self).await,
            Api::Anthropic => anthropic::models(self).await,
            Api::Chat => chat::models(self).await,
            Api::Gemini => gemini::models(self).await,
        }
    }

    /// Whether the server takes this client's key: `path` is one it only
    /// answers for a good one.
    pub async fn check_key(&self, path: &str) -> Result<()> {
        self.get(path).await.map(|_| ())
    }

    /// `GET`s `path` (after the base URL) as this client, as JSON.
    pub(crate) async fn get(&self, path: &str) -> Result<Value> {
        let mut builder = self.authorize(self.http.get(format!("{}/{path}", self.base_url)));
        for (name, value) in self.headers(None) {
            builder = builder.header(name, value);
        }
        Ok(check(builder.send().await?).await?.json().await?)
    }

    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::None => builder,
            Auth::Bearer(token) => builder.bearer_auth(token),
            Auth::Header(name, value) => builder.header(*name, value),
        }
    }

    /// The format's own headers, for `body` when it's a request.
    fn headers(&self, request: Option<(&Request<'_>, &Value)>) -> Vec<(&'static str, String)> {
        match self.api {
            Api::Responses => openai::headers(request),
            Api::Anthropic => anthropic::headers(request),
            Api::Chat | Api::Gemini => Vec::new(),
        }
    }

    fn body(&self, request: &Request<'_>, omitted: &[&str]) -> Value {
        match self.api {
            Api::Responses => openai::request_body(request),
            Api::Anthropic => anthropic::request_body(request, omitted, &self.base_url),
            Api::Chat => chat::request_body(request, omitted, self.quirks),
            Api::Gemini => gemini::request_body(request, omitted),
        }
    }

    fn optional(&self) -> Optional {
        match self.api {
            Api::Responses => &[],
            Api::Anthropic => anthropic::OPTIONAL,
            Api::Chat => chat::OPTIONAL,
            Api::Gemini => gemini::OPTIONAL,
        }
    }

    fn decoder(&self, request: &Request<'_>) -> Box<dyn Decode> {
        match self.api {
            Api::Responses => Box::new(openai::EventDecoder::default()),
            Api::Anthropic => Box::new(anthropic::decoder(request.model)),
            Api::Chat => Box::new(chat::decoder()),
            Api::Gemini => Box::new(gemini::decoder()),
        }
    }

    fn endpoint(&self, request: &Request<'_>) -> String {
        match self.api {
            Api::Responses => format!("{}/responses", self.base_url),
            Api::Anthropic => format!("{}/messages", self.base_url),
            Api::Chat => format!("{}/chat/completions", self.base_url),
            Api::Gemini => gemini::endpoint(&self.base_url, request.model),
        }
    }

    /// Sends the request and returns the answer as a stream of events.
    pub async fn stream(&self, request: &Request<'_>) -> Result<ResponseStream> {
        let mut omitted = self.omitted();
        loop {
            let body = self.body(request, &omitted);
            match self.send(request, &body, &omitted).await {
                Ok(response) => return Ok(ResponseStream { response, decoder: self.decoder(request), record: None, idle: self.idle }),
                // The server named a field it doesn't take: once more
                // without it, and never again with it.
                Err(e) if let Some(field) = rejected_field(&e, &body, self.optional(), &omitted) => {
                    omitted.push(field);
                    if let Ok(mut learned) = self.wire.omitted.lock() {
                        learned.push(field);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Posts `body`, gzipped if it's worth it and the server takes it.
    async fn send(&self, request: &Request<'_>, json: &Value, omitted: &[&str]) -> Result<reqwest::Response> {
        let headers = self.headers(Some((request, json)));
        let body = serde_json::to_vec(json)?;
        let gzipped = (body.len() >= COMPRESS_FROM && !self.wire.off.load(Ordering::Relaxed)).then(|| gzip(&body)).flatten();
        let compressed = gzipped.is_some();
        let answer = check(self.post(request, &headers, &body, gzipped).await?).await;
        // A server that can't unpack the body can't read it: the same
        // request again as it is. If that goes through, compression was the
        // trouble, and it's off from now on; if not, it wasn't. (One that
        // names a field in it read it fine.)
        let unreadable = |e: &Error| matches!(e.status(), Some(400 | 415 | 422)) && rejected_field(e, json, self.optional(), omitted).is_none();
        match answer {
            Err(e) if compressed && unreadable(&e) => {
                let plain = check(self.post(request, &headers, &body, None).await?).await?;
                self.wire.refused.store(true, Ordering::Relaxed);
                self.wire.off.store(true, Ordering::Relaxed);
                Ok(plain)
            }
            answer => answer,
        }
    }

    /// Posts `body`, or `gzipped` in its place when there is one.
    async fn post(&self, request: &Request<'_>, headers: &[(&'static str, String)], body: &[u8], gzipped: Option<Vec<u8>>) -> Result<reqwest::Response> {
        let mut builder =
            self.authorize(self.http.post(self.endpoint(request))).header(reqwest::header::ACCEPT, "text/event-stream").header(reqwest::header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let sent = match gzipped {
            Some(gzipped) => {
                builder = builder.header(reqwest::header::CONTENT_ENCODING, "gzip");
                gzipped
            }
            None => body.to_vec(),
        };
        self.wire.json.fetch_add(body.len() as u64, Ordering::Relaxed);
        self.wire.sent.fetch_add(sent.len() as u64, Ordering::Relaxed);
        Ok(builder.body(sent).send().await?)
    }
}

/// The field `error` turned away, if it's one rein can leave out and
/// `body` has it. The server has to say its name: a request turned away for
/// any other reason is an error, not something to learn from.
fn rejected_field(error: &Error, body: &Value, optional: Optional, omitted: &[&str]) -> Option<&'static str> {
    let Error::Api { status: Some(400 | 422), message, .. } = error else { return None };
    let message = message.to_lowercase();
    let body = body.to_string();
    optional.iter().find(|(word, key, field)| message.contains(word) && !omitted.contains(field) && body.contains(&format!("\"{key}\""))).map(|(_, _, field)| *field)
}

/// `bytes` gzipped. Level 6, zlib's own default: within a few percent of the
/// smallest, at a fraction of the time.
fn gzip(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 3), flate2::Compression::new(6));
    encoder.write_all(bytes).ok()?;
    encoder.finish().ok()
}

/// Turns a non-2xx response into [`Error::Api`], keeping the request ID,
/// which providers' support asks for.
pub(crate) async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let headers = response.headers();
    let request_id = ["x-request-id", "request-id"].iter().find_map(|name| headers.get(*name)).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let text = response.text().await.unwrap_or_default();
    let (code, message) = parse_error_body(&text);
    Err(Error::api(Some(status.as_u16()), code, message, request_id))
}

/// The code and message in an error body, in any of the shapes providers
/// use.
pub(crate) fn parse_error_body(text: &str) -> (Option<String>, String) {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    // A list of errors, as Gemini sometimes sends: the first says enough.
    let v = if let Some(first) = v.as_array().and_then(|errors| errors.first()) { first.clone() } else { v };
    if v["error"].is_object() {
        let error = &v["error"];
        let message = error["message"].as_str().map(str::to_owned).unwrap_or_else(|| error.to_string());
        // OpenAI says `code`, Anthropic `type`, Gemini `status`.
        let code = ["code", "type", "status"].iter().find_map(|key| error[*key].as_str()).map(str::to_owned);
        return (code, message);
    }
    // Requests turned away before reaching the API proper answer with
    // {"detail": "..."}, or just a message.
    if let Some(detail) = v["detail"].as_str().or(v["message"].as_str()).or(v["error"].as_str()) {
        return (None, detail.to_owned());
    }
    (None, text.chars().take(500).collect())
}

/// A streaming answer. Call [`ResponseStream::next`] until it returns `None`.
/// Dropping it closes the connection, which cancels the request.
pub struct ResponseStream {
    response: reqwest::Response,
    decoder: Box<dyn Decode>,
    record: Option<File>,
    idle: Duration,
}

impl ResponseStream {
    /// Also writes the raw stream to `file`, so tests can replay it later.
    pub fn record_to(&mut self, file: File) {
        self.record = Some(file);
    }

    pub async fn next(&mut self) -> Option<Result<Event>> {
        loop {
            if let Some(item) = self.decoder.pop() {
                return Some(item);
            }
            if self.decoder.is_finished() {
                return None;
            }
            let Ok(chunk) = tokio::time::timeout(self.idle, self.response.chunk()).await else {
                self.decoder.fail();
                return Some(Err(Error::Incomplete(format!("the provider sent nothing for {} seconds", self.idle.as_secs()))));
            };
            match chunk {
                Ok(Some(bytes)) => {
                    if let Some(file) = &mut self.record
                        && let Err(e) = file.write_all(&bytes)
                    {
                        return Some(Err(e.into()));
                    }
                    self.decoder.push(&bytes);
                }
                Ok(None) => return self.decoder.finish(),
                Err(e) => {
                    self.decoder.fail();
                    return Some(Err(e.into()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn an_answer_that_goes_quiet_is_given_up_on() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0u8; 65536]).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
            let event = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n";
            socket.write_all(format!("{head}{:x}\r\n{event}\r\n", event.len()).as_bytes()).await.unwrap();
            // And then nothing, with the connection left open.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let client = Client::new(reqwest::Client::new(), Api::Responses, base, Auth::None).with_idle_timeout(Duration::from_millis(300));
        let request =
            Request { model: "m", instructions: None, input: &[], tools: &[], reasoning_effort: None, reasoning_summary: false, session_id: None, max_output_tokens: None };
        let mut stream = client.stream(&request).await.unwrap();
        assert!(matches!(stream.next().await, Some(Ok(Event::TextDelta(_)))));
        let started = std::time::Instant::now();
        assert!(matches!(stream.next().await, Some(Err(Error::Incomplete(_)))));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(stream.next().await.is_none(), "nothing after giving up");
        server.abort();
    }

    #[test]
    fn keys_never_show_in_debug_output() {
        let client = Client::new(reqwest::Client::new(), Api::Anthropic, "https://x", Auth::Header("x-api-key", "sk-ant-secret".into()));
        let shown = format!("{client:?} {:?}", Auth::Bearer("sk-or-secret".into()));
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("Auth::Header(\"x-api-key\", ..)"), "{shown}");
    }

    #[test]
    fn parses_every_error_body_shape() {
        let openai = r#"{"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}"#;
        assert_eq!(parse_error_body(openai), (Some("subscription_sharing_usage_limit_exceeded".into()), "limit".into()));
        let anthropic = r#"{"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}, "request_id": "req_1"}"#;
        assert_eq!(parse_error_body(anthropic), (Some("overloaded_error".into()), "Overloaded".into()));
        let gemini = r#"[{"error": {"code": 400, "message": "bad key", "status": "INVALID_ARGUMENT"}}]"#;
        assert_eq!(parse_error_body(gemini), (Some("INVALID_ARGUMENT".into()), "bad key".into()));
        assert_eq!(parse_error_body(r#"{"detail": "not enabled"}"#), (None, "not enabled".into()));
        assert_eq!(parse_error_body("upstream timed out"), (None, "upstream timed out".into()));
    }

    #[test]
    fn learns_only_from_a_field_the_server_names() {
        const OPTIONAL: Optional = &[("reasoning_effort", "reasoning_effort", "effort"), ("stream_options", "stream_options", "stream_options")];
        let body = json!({ "model": "m", "reasoning_effort": "low", "stream_options": { "include_usage": true } });
        let error = |status, message: &str| Error::api(Some(status), None, message.into(), None);
        let named = error(400, "`reasoning_effort` is not supported with this model");
        assert_eq!(rejected_field(&named, &body, OPTIONAL, &[]), Some("effort"));
        assert_eq!(rejected_field(&named, &body, OPTIONAL, &["effort"]), None, "left out already: a real error");
        assert_eq!(rejected_field(&error(400, "invalid model"), &body, OPTIONAL, &[]), None);
        assert_eq!(rejected_field(&error(401, "reasoning_effort needs a paid key"), &body, OPTIONAL, &[]), None);
        let without = json!({ "model": "m" });
        assert_eq!(rejected_field(&named, &without, OPTIONAL, &[]), None, "the body didn't have it");
    }
}
