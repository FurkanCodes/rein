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
use crate::{Api, Effort, Error, Event, ModelInfo, Request, Result, anthropic, chat, gemini, openai};

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
    pub(crate) api: Api,
    base_url: String,
    auth: Auth,
    /// What this provider needs that it can't say in an error.
    quirks: Quirks,
    /// Where it says how much of your limits is used, besides with its answers.
    meters: crate::meter::Asked,
    /// How long an answer may go quiet before rein gives up on it.
    pub(crate) idle: Duration,
    /// How requests went over the wire, shared by the client's clones.
    wire: Arc<Wire>,
    /// Your ChatGPT plan's sign-in, which renews itself.
    chatgpt: Option<Arc<openai::signin::Session>>,
    pub(crate) claude_code: Option<crate::claude_code::Backend>,
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
    /// Effort levels a model turned down, which it isn't sent again.
    turned_down: Mutex<Vec<(String, Effort)>>,
    /// Models that turned held-back tools down: they're sent every tool
    /// from the start.
    shown_all: Mutex<Vec<String>>,
    /// Models that turned an image down: they're told of images in words.
    blind: Mutex<Vec<String>>,
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

/// What every format calls the field that says how hard to think. A model
/// that turns it down is turned down one level at a time: another level
/// may well do.
pub(crate) const EFFORT: &str = "effort";

/// What every format calls holding tools back for the tool search. A model
/// that turns it down gets every tool from the start: per model, as some
/// of a provider's models have it and others don't.
pub(crate) const HOLD_BACK: &str = "hold back";

/// What every format calls showing the model images. A model that turns
/// one down is told of them in words from then on, per model.
pub(crate) const IMAGES: &str = "images";

impl Client {
    /// A client for the server at `base_url` (like `https://api.anthropic.com/v1`),
    /// which speaks `api`.
    pub fn new(http: reqwest::Client, api: Api, base_url: impl Into<String>, auth: Auth) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        Client { http, api, base_url, auth, quirks: Quirks::default(), meters: crate::meter::Asked::Nowhere, idle: IDLE, wire: Arc::default(), chatgpt: None, claude_code: None }
    }

    /// OpenAI on your ChatGPT plan, signed in to with `rein login` and saved
    /// at `credentials`. The sign-in renews itself as its token runs out; one
    /// that can't be renewed shows now, not at your first message.
    pub async fn chatgpt(http: reqwest::Client, credentials: std::path::PathBuf) -> Result<Self> {
        Self::chatgpt_at(http, openai::API_BASE, openai::signin::Session::new(credentials)).await
    }

    pub(crate) async fn chatgpt_at(http: reqwest::Client, base_url: &str, session: openai::signin::Session) -> Result<Self> {
        session.token(&http).await?;
        let mut client = Self::new(http, Api::Responses, base_url, Auth::None);
        client.chatgpt = Some(Arc::new(session));
        Ok(client)
    }

    /// Uses the official Claude Code CLI. A token, when supplied, is passed
    /// only to that subprocess; otherwise the CLI uses its stored login.
    pub fn claude_code(http: reqwest::Client, token: Option<String>) -> Self {
        let mut client = Self::new(http, Api::Anthropic, "claude-code", Auth::None);
        client.claude_code = Some(crate::claude_code::Backend::new(token));
        client
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

    /// The same client, asking `at` how much of your limits is used.
    pub fn with_meters(mut self, at: crate::meter::Asked) -> Self {
        self.meters = at;
        self
    }

    /// Whether this provider can be asked how much of your limits is
    /// used, besides being told with its answers.
    pub fn asks_meters(&self) -> bool {
        self.meters != crate::meter::Asked::Nowhere
    }

    /// How much of your limits is used, asked of the provider now, for
    /// `model`: nothing where it can't be asked.
    pub async fn meters(&self, model: &str) -> Result<Vec<crate::Meter>> {
        match self.meters {
            crate::meter::Asked::Nowhere => Ok(Vec::new()),
            crate::meter::Asked::OpenRouterKey => Ok(crate::openrouter::meters(&self.get("key").await?, model, std::time::SystemTime::now())),
        }
    }

    /// OpenAI itself, with `bearer` as the API key.
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

    /// The effort levels `model` turned down, which go as none from then
    /// on, so it thinks as it does by default.
    pub fn turned_down(&self, model: &str) -> Vec<Effort> {
        let turned_down = self.wire.turned_down.lock().map(|t| t.clone()).unwrap_or_default();
        turned_down.into_iter().filter(|(which, _)| which == model).map(|(_, effort)| effort).collect()
    }

    /// The models this credential can use, in the server's order.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if self.claude_code.is_some() {
            // The CLI has no model-discovery endpoint. These are its aliases;
            // callers may also supply full model IDs directly.
            return Ok(["sonnet", "opus", "haiku"]
                .into_iter()
                // rein says its tools to the CLI in words: what's held back
                // is said once it's found, whatever the model.
                .map(|id| ModelInfo { finds_tools: crate::FindsTools::Yes, ..ModelInfo::named(id) })
                .collect());
        }
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
        let mut builder = self.http.get(format!("{}/{path}", self.base_url));
        for (name, value) in self.headers(None) {
            builder = builder.header(name, value);
        }
        Ok(check(self.send_as(builder).await?).await?.json().await?)
    }

    /// Sends `builder` as this client. When the server turns a ChatGPT
    /// sign-in's token away, the sign-in is renewed and the request goes
    /// once more: a token can stop working before the time it gave.
    async fn send_as(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let Some(session) = &self.chatgpt else {
            return Ok(self.authorize(builder).send().await?);
        };
        let again = builder.try_clone();
        let token = session.token(&self.http).await?;
        let response = builder.bearer_auth(&token).send().await?;
        match again {
            Some(again) if response.status() == reqwest::StatusCode::UNAUTHORIZED => Ok(again.bearer_auth(session.renew(&self.http, &token).await?).send().await?),
            _ => Ok(response),
        }
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
            Api::Responses => openai::OPTIONAL,
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
        if let Some(backend) = &self.claude_code {
            return Ok(ResponseStream { source: StreamSource::Buffered(backend.respond(request, self.idle).await?) });
        }
        let mut omitted = self.omitted();
        let mut request = *request;
        if request.reasoning_effort.is_some_and(|effort| self.turned_down(request.model).contains(&effort)) {
            request.reasoning_effort = None;
        }
        if self.wire.shown_all.lock().is_ok_and(|models| models.iter().any(|model| model == request.model)) {
            request.hold_back = false;
        }
        if self.wire.blind.lock().is_ok_and(|models| models.iter().any(|model| model == request.model)) {
            request.images = false;
        }
        loop {
            let body = self.body(&request, &omitted);
            match self.send(&request, &body, &omitted).await {
                Ok(response) => {
                    return Ok(ResponseStream { source: StreamSource::Http(Box::new(HttpStream { response, decoder: self.decoder(&request), record: None, idle: self.idle })) });
                }
                // The server named a field it doesn't take: once more
                // without it, and never again with it. For effort, that's
                // this level for this model; for holding tools back, this
                // model.
                Err(e) if let Some(field) = rejected_field(&e, &body, self.optional(), &omitted) => match (field, request.reasoning_effort) {
                    (EFFORT, Some(effort)) => {
                        if let Ok(mut turned_down) = self.wire.turned_down.lock() {
                            turned_down.push((request.model.to_owned(), effort));
                        }
                        request.reasoning_effort = None;
                    }
                    (HOLD_BACK, _) if request.hold_back => {
                        if let Ok(mut shown_all) = self.wire.shown_all.lock() {
                            shown_all.push(request.model.to_owned());
                        }
                        request.hold_back = false;
                    }
                    // Nothing's held back already: it's about something else.
                    (HOLD_BACK, _) => return Err(e),
                    (IMAGES, _) if request.images => {
                        if let Ok(mut blind) = self.wire.blind.lock() {
                            blind.push(request.model.to_owned());
                        }
                        request.images = false;
                    }
                    (IMAGES, _) => return Err(e),
                    _ => {
                        omitted.push(field);
                        if let Ok(mut learned) = self.wire.omitted.lock() {
                            learned.push(field);
                        }
                    }
                },
                Err(e) => return Err(e),
            }
        }
    }

    /// Posts `body`, gzipped if it's worth it and the server takes it.
    pub(crate) async fn send(&self, request: &Request<'_>, json: &Value, omitted: &[&str]) -> Result<reqwest::Response> {
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
        let mut builder = self.http.post(self.endpoint(request)).header(reqwest::header::ACCEPT, "text/event-stream").header(reqwest::header::CONTENT_TYPE, "application/json");
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
        self.send_as(builder.body(sent)).await
    }
}

/// The field `error` turned away, if it's one rein can leave out and
/// `body` has it. The server has to say its name: a request turned away for
/// any other reason is an error, not something to learn from.
fn rejected_field(error: &Error, body: &Value, optional: Optional, omitted: &[&str]) -> Option<&'static str> {
    let Error::Api { status: Some(400 | 422), message, param, .. } = error else { return None };
    // OpenAI names the field apart from the message.
    let message = format!("{} {}", message.to_lowercase(), param.as_deref().unwrap_or_default().to_lowercase());
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
    let param = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"]["param"].as_str().map(str::to_owned));
    Err(Error::api(Some(status.as_u16()), code, message, request_id).about(param))
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

/// An answer. HTTP answers stream incrementally; CLI answers are buffered.
/// Dropping an in-flight request cancels its connection or subprocess.
pub struct ResponseStream {
    source: StreamSource,
}

enum StreamSource {
    Http(Box<HttpStream>),
    Buffered(std::collections::VecDeque<Result<Event>>),
}

impl ResponseStream {
    /// Also writes the raw HTTP stream to `file`; subprocess replies are not recorded.
    pub fn record_to(&mut self, file: File) {
        if let StreamSource::Http(stream) = &mut self.source {
            stream.record_to(file);
        }
    }

    pub async fn next(&mut self) -> Option<Result<Event>> {
        match &mut self.source {
            StreamSource::Http(stream) => stream.next().await,
            StreamSource::Buffered(events) => events.pop_front(),
        }
    }
}

struct HttpStream {
    response: reqwest::Response,
    decoder: Box<dyn Decode>,
    record: Option<File>,
    idle: Duration,
}

impl HttpStream {
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
    async fn a_level_a_model_turns_down_goes_as_none_from_then_on_for_that_model_only() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 65536];
                // Head and body come in one read or a few; the body is the JSON after the head.
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") || !bytes.ends_with(b"}") {
                    let n = socket.read(&mut buf).await.unwrap();
                    bytes.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let body: Value = serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                // As OpenAI says it, naming the field apart from the message.
                let reply = if body["model"] == "picky" && body["reasoning"]["effort"] == "minimal" {
                    let error = r#"{"error":{"message":"Unsupported value: 'minimal' is not supported with the 'picky' model.","type":"invalid_request_error","param":"reasoning.effort","code":"unsupported_value"}}"#;
                    format!("HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{error}", error.len())
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                };
                seen.lock().unwrap().push(body);
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let client = Client::new(reqwest::Client::new(), Api::Responses, base, Auth::None);
        let input = [crate::Item::User("hi".into())];
        let request = |model| Request {
            model,
            instructions: None,
            input: &input,
            tools: &[],
            reasoning_effort: Some(Effort::Minimal),
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
            hold_back: false,
            images: false,
        };
        assert!(client.stream(&request("picky")).await.is_ok());
        assert_eq!(client.turned_down("picky"), [Effort::Minimal]);
        assert!(client.stream(&request("picky")).await.is_ok());
        assert!(client.stream(&request("easy")).await.is_ok());
        let bodies = bodies.lock().unwrap();
        let efforts: Vec<&Value> = bodies.iter().map(|b| &b["reasoning"]["effort"]).collect();
        // Turned down, sent again without; then never again for it; another model still gets it.
        assert_eq!(efforts, [&json!("minimal"), &Value::Null, &Value::Null, &json!("minimal")]);
        assert!(client.turned_down("easy").is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn a_model_that_turns_added_tools_down_is_shown_every_tool_from_then_on() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 65536];
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") || !bytes.ends_with(b"}") {
                    let n = socket.read(&mut buf).await.unwrap();
                    bytes.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let body: Value = serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                let adds = body["input"].as_array().into_iter().flatten().any(|item| item["type"] == "additional_tools");
                let reply = if body["model"] == "old" && adds {
                    let error = r#"{"error":{"message":"Invalid value: 'additional_tools'. Supported values are: 'message', 'function_call' and 'function_call_output'.","type":"invalid_request_error","param":"input[3]","code":"invalid_value"}}"#;
                    format!("HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{error}", error.len())
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                };
                seen.lock().unwrap().push(body);
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let client = Client::new(reqwest::Client::new(), Api::Responses, base, Auth::None).without_compression();
        let (tools, input) = (crate::testing::held_back_tools(), crate::testing::found_and_called());
        let request = |model| Request { model, ..crate::testing::request(&input, &tools, true) };
        assert!(client.stream(&request("old")).await.is_ok());
        assert!(client.stream(&request("old")).await.is_ok());
        assert!(client.stream(&request("new")).await.is_ok());
        let bodies = bodies.lock().unwrap();
        let shown = |body: &Value| body["tools"][0]["tools"].as_array().unwrap().iter().any(|tool| tool["name"] == "mcp__linear__create_issue");
        // Turned down, sent again with all; then all from the start; another model still has them held back.
        assert_eq!(bodies.iter().map(shown).collect::<Vec<_>>(), [false, true, true, false]);
        server.abort();
    }

    #[tokio::test]
    async fn a_model_that_turns_an_image_down_is_told_of_images_in_words_from_then_on() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 65536];
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") || !bytes.ends_with(b"}") {
                    let n = socket.read(&mut buf).await.unwrap();
                    bytes.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let body: Value = serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                let reply = if body["model"] == "blind" && body.to_string().contains("input_image") {
                    let error = r#"{"error":{"message":"Invalid content type. image_url is only supported by certain models.","type":"invalid_request_error","param":"input[2].output","code":"invalid_value"}}"#;
                    format!("HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{error}", error.len())
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                };
                seen.lock().unwrap().push(body);
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let client = Client::new(reqwest::Client::new(), Api::Responses, base, Auth::None).without_compression();
        let input = crate::testing::pictured();
        let request = |model| Request { model, ..crate::testing::request(&input, &[], false) };
        assert!(client.stream(&request("blind")).await.is_ok());
        assert!(client.stream(&request("blind")).await.is_ok());
        assert!(client.stream(&request("sees")).await.is_ok());
        let bodies = bodies.lock().unwrap();
        let shown: Vec<bool> = bodies.iter().map(|b| b.to_string().contains("input_image")).collect();
        // Turned down, sent again in words; then in words from the start; another model is still shown it.
        assert_eq!(shown, [true, false, false, true]);
        assert!(bodies[1]["input"][2]["output"].as_str().unwrap().contains("this model can't see images"));
        server.abort();
    }

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
        let request = Request {
            model: "m",
            instructions: None,
            input: &[],
            tools: &[],
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
            hold_back: false,
            images: false,
        };
        let mut stream = client.stream(&request).await.unwrap();
        assert!(matches!(stream.next().await, Some(Ok(Event::TextDelta(_)))));
        let started = std::time::Instant::now();
        assert!(matches!(stream.next().await, Some(Err(Error::Incomplete(_)))));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(stream.next().await.is_none(), "nothing after giving up");
        server.abort();
    }

    #[tokio::test]
    async fn a_conversation_on_chatgpt_outlives_its_token() {
        use crate::openai::signin::{Session, tests::*};
        let path = signed_in("outlives", "first", in_an_hour());
        let (tokens, renewals) = serve(vec![(200, RENEWED)]).await;
        let (api, seen) = serve(vec![(200, "{}")]).await;
        let client = Client::chatgpt_at(reqwest::Client::new(), &api, Session::at(path.clone(), &format!("{tokens}/token"))).await.unwrap();
        // An hour on, the token the conversation started with has run out.
        let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        saved["expires_at"] = json!(0);
        std::fs::write(&path, saved.to_string()).unwrap();
        let request = Request {
            model: "m",
            instructions: None,
            input: &[],
            tools: &[],
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
            hold_back: false,
            images: false,
        };
        assert!(client.stream(&request).await.is_ok());
        assert_eq!(renewals.lock().unwrap().len(), 1);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "renewed before sending, not after being turned away");
        assert!(seen[0].to_lowercase().contains("authorization: bearer new-access\r\n"), "{}", seen[0]);
    }

    #[tokio::test]
    async fn a_chatgpt_token_the_server_turns_away_is_renewed_and_the_request_sent_again() {
        use crate::openai::signin::{Session, tests::*};
        let path = signed_in("turned-away-mid-turn", "first", in_an_hour());
        let (tokens, _) = serve(vec![(200, RENEWED)]).await;
        let expired = r#"{"error":{"message":"Provided authentication token is expired.","code":"token_expired"}}"#;
        let (api, seen) = serve(vec![(401, expired), (200, "{}")]).await;
        let client = Client::chatgpt_at(reqwest::Client::new(), &api, Session::at(path, &format!("{tokens}/token"))).await.unwrap();
        let request = Request {
            model: "m",
            instructions: None,
            input: &[],
            tools: &[],
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
            hold_back: false,
            images: false,
        };
        assert!(client.stream(&request).await.is_ok());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].to_lowercase().contains("authorization: bearer first\r\n"), "{}", seen[0]);
        assert!(seen[1].to_lowercase().contains("authorization: bearer new-access\r\n") && seen[1].contains(r#""model":"m""#), "{}", seen[1]);
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
