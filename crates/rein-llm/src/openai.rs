//! OpenAI's Responses API: `POST /v1/responses`, streamed.
//!
//! An API key and a Sign in with ChatGPT access token both go in the same
//! `Authorization: Bearer` header; only who pays differs.

pub mod signin;

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;

use serde_json::{Value, json};

use crate::sse::{SseEvent, SseParser};
use crate::{Error, Event, Item, Result, Tool, ToolCall, Usage};

pub const API_BASE: &str = "https://api.openai.com/v1";

/// Sign in with ChatGPT only accepts function tools grouped in a namespace,
/// so all of rein's tools live in this one.
const NAMESPACE: &str = "rein";

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    bearer: String,
}

/// One request to the model.
#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    /// The system prompt.
    pub instructions: Option<String>,
    /// The whole conversation so far. Sign in with ChatGPT keeps no
    /// server-side history, so every request carries it all.
    pub input: Vec<Item>,
    /// The tools the model may call.
    pub tools: Vec<Tool>,
    /// For example "low", "medium" or "high". `None` uses the model default.
    pub reasoning_effort: Option<String>,
    /// The same ID on every request of one conversation. Sign in with ChatGPT
    /// routes requests to its prompt cache by the `session-id` header, so
    /// without it nothing is ever cached; see [`new_session_id`].
    pub session_id: Option<String>,
}

/// A fresh ID for one conversation, for [`Request::session_id`].
pub fn new_session_id() -> Result<String> {
    signin::new_uuid()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// How many tokens one request may carry, when the list says.
    pub context_window: Option<u64>,
}

impl Client {
    /// `bearer` is an OpenAI API key or a Sign in with ChatGPT access token.
    pub fn new(http: reqwest::Client, bearer: impl Into<String>) -> Self {
        Client { http, base_url: API_BASE.to_owned(), bearer: bearer.into() }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// The models this credential can use, in the server's order.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self.http.get(format!("{}/models", self.base_url)).bearer_auth(&self.bearer).send().await?;
        let body: Value = check(response).await?.json().await?;
        Ok(parse_models(&body))
    }

    /// Sends the request and returns the answer as a stream of events.
    pub async fn stream(&self, request: &Request) -> Result<ResponseStream> {
        let mut builder = self
            .http
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.bearer)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&request_body(request));
        if let Some(id) = &request.session_id {
            // The prompt cache follows this header, not `prompt_cache_key`:
            // the server replaces that with a random key on every request.
            builder = builder.header("session-id", id);
        }
        let response = builder.send().await?;
        Ok(ResponseStream { response: check(response).await?, decoder: EventDecoder::default(), record: None })
    }
}

fn request_body(request: &Request) -> Value {
    let input: Vec<Value> = request.input.iter().map(item_json).collect();
    // Sign in with ChatGPT requires `store: false` and `stream: true`, and
    // rejects fields like `temperature` and `max_output_tokens`. Sending only
    // what it accepts keeps one request shape for both ways of paying.
    let mut body = json!({ "model": request.model, "input": input, "store": false, "stream": true });
    if let Some(instructions) = &request.instructions {
        body["instructions"] = json!(instructions);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": true }))
            .collect();
        body["tools"] = json!([{ "type": "namespace", "name": NAMESPACE, "description": "rein's tools for working in the user's project.", "tools": tools }]);
    }
    body
}

/// One conversation item in the Responses API's input format.
fn item_json(item: &Item) -> Value {
    match item {
        Item::User(text) => json!({ "role": "user", "content": text }),
        Item::Assistant(text) => json!({ "role": "assistant", "content": text }),
        Item::ToolCall(call) => json!({
            "type": "function_call",
            "call_id": call.call_id,
            "namespace": NAMESPACE,
            "name": call.name,
            "arguments": call.arguments,
        }),
        Item::ToolResult { call_id, output } => json!({ "type": "function_call_output", "call_id": call_id, "output": output }),
        Item::Opaque(value) => value.clone(),
    }
}

/// Turns a non-2xx response into [`Error::Api`], keeping the request ID,
/// which OpenAI support asks for.
async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let request_id = response.headers().get("x-request-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let text = response.text().await.unwrap_or_default();
    let (code, message) = parse_error_body(&text);
    Err(Error::Api { status: Some(status.as_u16()), code, message, request_id })
}

fn parse_error_body(text: &str) -> (Option<String>, String) {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if v["error"].is_object() {
        let message = str_field(&v["error"], "message").unwrap_or_else(|| v["error"].to_string());
        return (str_field(&v["error"], "code"), message);
    }
    // Requests turned away before reaching the Responses API answer with
    // {"detail": "..."} instead.
    if let Some(detail) = v["detail"].as_str() {
        return (None, detail.to_owned());
    }
    (None, text.chars().take(500).collect())
}

fn parse_models(body: &Value) -> Vec<ModelInfo> {
    // Sign in with ChatGPT lists {"models": [{slug, display_name, visibility}]};
    // API keys get the standard {"data": [{id}]}.
    if let Some(models) = body["models"].as_array() {
        return models
            .iter()
            .filter(|m| m["visibility"].as_str().is_none_or(|v| v == "list"))
            .filter_map(|m| {
                let id = m["slug"].as_str()?;
                Some(ModelInfo {
                    id: id.to_owned(),
                    name: m["display_name"].as_str().unwrap_or(id).to_owned(),
                    context_window: m["context_window"].as_u64(),
                })
            })
            .collect();
    }
    let ids = body["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str());
    ids.map(|id| ModelInfo { id: id.to_owned(), name: id.to_owned(), context_window: None }).collect()
}

/// A streaming answer. Call [`ResponseStream::next`] until it returns `None`.
/// Dropping it closes the connection, which cancels the request.
pub struct ResponseStream {
    response: reqwest::Response,
    decoder: EventDecoder,
    record: Option<File>,
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
            match self.response.chunk().await {
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
                    self.decoder.finished = true;
                    return Some(Err(e.into()));
                }
            }
        }
    }
}

/// Turns raw stream bytes into [`Event`]s. It's separate from the HTTP code
/// so tests can feed it recorded streams without a network.
#[derive(Debug, Default)]
pub struct EventDecoder {
    parser: SseParser,
    queue: VecDeque<Result<Event>>,
    /// Set once the response has completed or failed.
    finished: bool,
}

impl EventDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        for sse in self.parser.push(bytes) {
            if self.finished {
                break;
            }
            if let Some(item) = decode(&sse) {
                // Completing and every kind of failure end the response.
                self.finished = matches!(item, Ok(Event::Completed { .. }) | Err(_));
                self.queue.push_back(item);
            }
        }
    }

    pub fn pop(&mut self) -> Option<Result<Event>> {
        self.queue.pop_front()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Call when the stream ends. A stream that ends before the response
    /// completed was cut off, and that's reported as [`Error::Interrupted`].
    pub fn finish(&mut self) -> Option<Result<Event>> {
        if self.finished {
            return self.pop();
        }
        self.finished = true;
        Some(Err(Error::Interrupted))
    }
}

fn decode(sse: &SseEvent) -> Option<Result<Event>> {
    if sse.data.is_empty() || sse.data == "[DONE]" {
        return None;
    }
    let v: Value = match serde_json::from_str(&sse.data) {
        Ok(v) => v,
        Err(e) => return Some(Err(e.into())),
    };
    match v["type"].as_str().or(sse.event.as_deref())? {
        "response.output_text.delta" => Some(Ok(Event::TextDelta(v["delta"].as_str()?.to_owned()))),
        "response.output_item.done" => decode_item(&v["item"]).map(|item| Ok(Event::Item(item))),
        "response.completed" => Some(Ok(Event::Completed { usage: parse_usage(&v["response"]["usage"]) })),
        "response.failed" => Some(Err(api_error(&v["response"]["error"], "the response failed"))),
        "response.incomplete" => {
            let reason = v["response"]["incomplete_details"]["reason"].as_str().unwrap_or("unknown reason");
            Some(Err(Error::Incomplete(reason.to_owned())))
        }
        "error" => Some(Err(api_error(if v["error"].is_object() { &v["error"] } else { &v }, "stream error"))),
        // Many other events (reasoning summaries, argument deltas, ...)
        // arrive too; the finished items above carry what rein needs.
        _ => None,
    }
}

/// A finished output item, as rein's conversation item.
fn decode_item(item: &Value) -> Option<Item> {
    match item["type"].as_str()? {
        "message" => {
            let parts = item["content"].as_array()?.iter();
            let text: String = parts.filter(|p| p["type"] == "output_text").filter_map(|p| p["text"].as_str()).collect();
            Some(Item::Assistant(text))
        }
        "function_call" => Some(Item::ToolCall(ToolCall {
            call_id: item["call_id"].as_str()?.to_owned(),
            name: item["name"].as_str()?.to_owned(),
            arguments: item["arguments"].as_str().unwrap_or("{}").to_owned(),
        })),
        // Encrypted reasoning: the next request must carry it back as is.
        "reasoning" => Some(Item::Opaque(item.clone())),
        _ => None,
    }
}

fn api_error(error: &Value, fallback: &str) -> Error {
    Error::Api {
        status: None,
        code: str_field(error, "code"),
        message: str_field(error, "message").unwrap_or_else(|| fallback.to_owned()),
        request_id: None,
    }
}

fn parse_usage(usage: &Value) -> Usage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    Usage {
        input_tokens: n(&usage["input_tokens"]),
        cached_input_tokens: n(&usage["input_tokens_details"]["cached_tokens"]),
        output_tokens: n(&usage["output_tokens"]),
        reasoning_tokens: n(&usage["output_tokens_details"]["reasoning_tokens"]),
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_has_only_fields_both_billing_paths_accept() {
        let request = Request {
            model: "m".into(),
            instructions: Some("be brief".into()),
            input: vec![Item::User("hi".into()), Item::Assistant("hello".into()), Item::User("bye".into())],
            tools: vec![],
            reasoning_effort: Some("low".into()),
            session_id: None,
        };
        assert_eq!(
            request_body(&request),
            json!({
                "model": "m",
                "instructions": "be brief",
                "input": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": "hello"},
                    {"role": "user", "content": "bye"},
                ],
                "reasoning": {"effort": "low"},
                "store": false,
                "stream": true,
            })
        );
    }

    #[test]
    fn tools_go_in_a_namespace_and_calls_round_trip() {
        let call = ToolCall { call_id: "call_1".into(), name: "read".into(), arguments: r#"{"path":"a.rs"}"#.into() };
        let request = Request {
            model: "m".into(),
            instructions: None,
            input: vec![
                Item::User("open a.rs".into()),
                Item::Opaque(json!({"type": "reasoning", "encrypted_content": "xyz"})),
                Item::ToolCall(call.clone()),
                Item::ToolResult { call_id: "call_1".into(), output: "fn main() {}".into() },
            ],
            tools: vec![Tool { name: "read".into(), description: "Read a file.".into(), parameters: json!({"type": "object"}) }],
            reasoning_effort: None,
            session_id: None,
        };
        let body = request_body(&request);
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["tools"][0]["name"], "read");
        assert_eq!(body["input"][1], json!({"type": "reasoning", "encrypted_content": "xyz"}));
        assert_eq!(
            body["input"][2],
            json!({"type": "function_call", "call_id": "call_1", "namespace": "rein", "name": "read", "arguments": "{\"path\":\"a.rs\"}"})
        );
        assert_eq!(body["input"][3], json!({"type": "function_call_output", "call_id": "call_1", "output": "fn main() {}"}));

        // The model's call comes back as the same item.
        let done = json!({"type": "function_call", "call_id": "call_1", "namespace": "rein", "name": "read", "arguments": "{\"path\":\"a.rs\"}"});
        assert_eq!(decode_item(&done), Some(Item::ToolCall(call)));
    }

    #[test]
    fn session_ids_are_fresh_v4_uuids() {
        let (a, b) = (new_session_id().unwrap(), new_session_id().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4", "version 4: {a}");
        assert!("89ab".contains(&a[19..20]), "RFC 4122 variant: {a}");
    }

    #[test]
    fn parses_both_model_list_shapes() {
        let chatgpt = json!({"models": [
            {"slug": "a", "display_name": "Model A", "visibility": "list", "context_window": 500000},
            {"slug": "hidden", "display_name": "Hidden", "visibility": "hide"},
        ]});
        assert_eq!(parse_models(&chatgpt), [ModelInfo { id: "a".into(), name: "Model A".into(), context_window: Some(500_000) }]);
        let api_key = json!({"data": [{"id": "b"}]});
        assert_eq!(parse_models(&api_key), [ModelInfo { id: "b".into(), name: "b".into(), context_window: None }]);
    }

    #[test]
    fn parses_both_error_body_shapes() {
        let structured = r#"{"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}"#;
        assert_eq!(
            parse_error_body(structured),
            (Some("subscription_sharing_usage_limit_exceeded".into()), "limit".into())
        );
        assert_eq!(parse_error_body(r#"{"detail": "not enabled"}"#), (None, "not enabled".into()));
    }
}
