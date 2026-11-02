//! Streaming LLM provider layer for rein.
//!
//! rein keeps a conversation in its own terms ([`Item`]s) and speaks each
//! provider's wire format only at the edge: a [`Client`] turns a
//! [`Request`] into the body its [`Api`] expects, and the answer back into
//! [`Event`]s. Four formats reach almost every provider there is:
//! OpenAI's Responses API, Anthropic's Messages API, Chat Completions (the
//! long tail, and local servers), and Gemini's own.

pub mod anthropic;
pub mod chat;
pub mod client;
pub mod decode;
pub mod openai;
pub mod provider;
pub mod sse;
pub mod store;

pub use client::{Auth, Client, Compression, ResponseStream, Traffic};
pub use reqwest;
pub use serde_json;

use std::fmt;

use serde::{Deserialize, Serialize};

/// One thing that happened while the model was answering.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A piece of the visible answer text, for showing as it's written.
    TextDelta(String),
    /// A piece of a short summary of the model's reasoning, written while
    /// it thinks. Only shown; the reasoning itself comes as an item.
    Thought(String),
    /// Something worth telling the user about the answer itself, like
    /// another model having answered in place of the one asked.
    Note(String),
    /// A finished piece of the answer. Append it to the conversation: the
    /// next request has to send it back.
    Item(Item),
    /// The response finished normally. Always the last event.
    Completed { usage: Usage },
}

/// One entry in a conversation. The model remembers nothing, so every
/// request carries all of them, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// What the user typed.
    User(String),
    /// What the model wrote.
    Assistant(String),
    /// The model asking rein to run a tool.
    ToolCall(ToolCall),
    /// What running that tool produced, sent back to the model.
    ToolResult { call_id: String, output: String },
    /// Provider data that has to be sent back unchanged, such as OpenAI's
    /// encrypted reasoning or Claude's signed thinking. rein never looks
    /// inside.
    Opaque(Opaque),
}

/// Data only the format that produced it can read, so only requests in
/// that format carry it: after a switch from OpenAI to Claude, say, the
/// conversation's words and tool calls go on, its encrypted reasoning
/// doesn't.
#[derive(Debug, Clone, PartialEq)]
pub struct Opaque {
    pub api: Api,
    pub value: serde_json::Value,
}

impl Opaque {
    pub fn new(api: Api, value: serde_json::Value) -> Opaque {
        Opaque { api, value }
    }

    /// Valid only while everything before it in the conversation stays as
    /// it was. Claude's thinking is signed together with what came before
    /// it, so once rein changes the conversation earlier on (compaction
    /// does), the thinking after that point has to go.
    pub fn bound(&self) -> bool {
        self.api == Api::Anthropic && matches!(self.value["type"].as_str(), Some("thinking" | "redacted_thinking"))
    }
}

/// A wire format: what a request looks like, and how the answer streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// OpenAI's Responses API, `POST /responses`.
    #[default]
    Responses,
    /// Anthropic's Messages API, `POST /messages`.
    Anthropic,
    /// Chat Completions, `POST /chat/completions`: most other providers,
    /// and servers on your own computer.
    Chat,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Api::Responses => "responses",
            Api::Anthropic => "anthropic",
            Api::Chat => "chat",
        }
    }

    pub fn parse(name: &str) -> Option<Api> {
        [Api::Responses, Api::Anthropic, Api::Chat].into_iter().find(|api| api.name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Pairs the call with its result.
    pub call_id: String,
    pub name: String,
    /// The arguments as a JSON object, still encoded as text.
    pub arguments: String,
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    /// Tells the model what the tool does and when to use it.
    pub description: String,
    /// A JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

/// Token counts reported when a response completes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Everything the request carried, cached or not.
    pub input_tokens: u64,
    /// The part of `input_tokens` served from the provider's prompt cache.
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// The part of `output_tokens` spent on hidden reasoning.
    pub reasoning_tokens: u64,
}

/// One request to the model, in rein's terms; each [`Api`] has its own
/// way of saying it.
#[derive(Debug, Clone)]
pub struct Request {
    pub model: String,
    /// The system prompt.
    pub instructions: Option<String>,
    /// The whole conversation so far. Nothing is kept on the server, so
    /// every request carries it all.
    pub input: Vec<Item>,
    /// The tools the model may call.
    pub tools: Vec<Tool>,
    /// For example "low", "medium" or "high". `None` uses the model default.
    pub reasoning_effort: Option<String>,
    /// Ask for short summaries of the model's reasoning as it thinks
    /// ([`Event::Thought`]). Only for models that reason.
    pub reasoning_summary: bool,
    /// The same ID on every request of one conversation. Sign in with ChatGPT
    /// routes requests to its prompt cache by the `session-id` header, so
    /// without it nothing is ever cached; see [`new_session_id`].
    pub session_id: Option<String>,
    /// The most the answer may take, where the format needs a number
    /// (Anthropic's does). `None` lets rein pick.
    pub max_output_tokens: Option<u64>,
}

/// A fresh ID for one conversation, for [`Request::session_id`].
pub fn new_session_id() -> Result<String> {
    openai::signin::new_uuid()
}

/// A model a provider offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// How many tokens one request may carry, when the list says.
    pub context_window: Option<u64>,
    /// How many tokens one answer may take, when the list says.
    pub max_output: Option<u64>,
}

/// What [`Error::code`] says when the conversation no longer fits the
/// model's window, whatever words the provider used.
pub const CONTEXT_FULL: &str = "context_length_exceeded";

/// What [`Error::code`] says when the provider turned Claude's thinking
/// away because the conversation before it changed. Dropping that thinking
/// and asking again works.
pub const STALE_THINKING: &str = "stale_thinking";

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The provider rejected the request, or failed while answering.
    Api {
        status: Option<u16>,
        code: Option<String>,
        message: String,
        request_id: Option<String>,
    },
    /// Signing in or refreshing credentials failed.
    Auth(String),
    /// The response stopped early, for example at an output limit.
    Incomplete(String),
    /// The connection closed before the response finished.
    Interrupted,
}

impl Error {
    /// An error a provider sent. Where its words say which kind it is, the
    /// code becomes rein's own ([`CONTEXT_FULL`], [`STALE_THINKING`]), so
    /// the agent can act on it whoever sent it.
    pub fn api(status: Option<u16>, code: Option<String>, message: String, request_id: Option<String>) -> Error {
        let lower = message.to_lowercase();
        let rejected = status.is_none_or(|s| matches!(s, 400 | 413 | 422));
        let full = [
            "prompt is too long",
            "context length",
            "context window",
            "maximum context",
            "exceeds the maximum number of tokens",
            "input token count",
            "too many input tokens",
        ];
        let code = if rejected && lower.contains("signature") && lower.contains("thinking") {
            Some(STALE_THINKING.to_owned())
        } else if rejected && full.iter().any(|words| lower.contains(words)) {
            Some(CONTEXT_FULL.to_owned())
        } else {
            code
        };
        Error::Api { status, code, message, request_id }
    }

    /// The provider's machine-readable error code, if it sent one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// Worth sending the same request again after a pause: the network
    /// failed, the provider is busy (429), or it had an error of its own
    /// (5xx). Not when you've run out of usage: waiting a few seconds won't
    /// change that.
    pub fn retryable(&self) -> bool {
        let code = self.code().unwrap_or_default();
        if code.contains("usage_limit") || code.contains("quota") {
            return false;
        }
        match self {
            Error::Http(e) => e.is_connect() || e.is_timeout() || e.is_request() || e.status().is_some_and(|s| s.is_server_error()),
            Error::Api { status: Some(status), .. } => *status == 429 || (500..600).contains(status),
            _ => false,
        }
    }

    /// The HTTP status the provider answered with, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Api { status, .. } => *status,
            Error::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => write!(f, "network error: {e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "unexpected response: {e}"),
            Error::Api { status, code, message, request_id } => {
                write!(f, "{message}")?;
                let details: Vec<String> = [
                    status.map(|s| format!("HTTP {s}")),
                    code.clone(),
                    request_id.as_ref().map(|r| format!("request {r}")),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !details.is_empty() {
                    write!(f, " ({})", details.join(", "))?;
                }
                Ok(())
            }
            Error::Auth(message) => write!(f, "{message}"),
            Error::Incomplete(reason) => write!(f, "the response stopped early: {reason}"),
            Error::Interrupted => write!(f, "the connection closed before the response finished"),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, code: Option<&str>) -> Error {
        Error::Api { status: Some(status), code: code.map(str::to_owned), message: String::new(), request_id: None }
    }

    #[test]
    fn retries_only_what_a_pause_can_fix() {
        assert!(api(429, Some("rate_limit_exceeded")).retryable());
        assert!(api(503, None).retryable());
        assert!(!api(429, Some("subscription_sharing_usage_limit_exceeded")).retryable());
        assert!(!api(400, None).retryable());
        assert!(!api(401, None).retryable());
        assert!(!Error::Interrupted.retryable(), "half an answer was shown already");
    }

    #[test]
    fn names_the_errors_the_agent_acts_on_whoever_sent_them() {
        let api = |status, message: &str| Error::api(Some(status), Some("invalid_request_error".into()), message.into(), None);
        assert_eq!(api(400, "prompt is too long: 210000 tokens > 200000 maximum").code(), Some(CONTEXT_FULL));
        assert_eq!(api(400, "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).").code(), Some(CONTEXT_FULL));
        let stale = "messages.5.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.";
        assert_eq!(api(400, stale).code(), Some(STALE_THINKING));
        assert_eq!(api(400, "max_tokens: field required").code(), Some("invalid_request_error"));
        assert_eq!(api(500, "context window service down").code(), Some("invalid_request_error"), "only a rejected request");
    }

    #[test]
    fn only_claude_thinking_is_bound_to_what_came_before() {
        let thinking = Opaque::new(Api::Anthropic, serde_json::json!({"type": "thinking", "thinking": "", "signature": "s"}));
        assert!(thinking.bound());
        assert!(Opaque::new(Api::Anthropic, serde_json::json!({"type": "redacted_thinking", "data": "d"})).bound());
        assert!(!Opaque::new(Api::Responses, serde_json::json!({"type": "reasoning", "encrypted_content": "x"})).bound());
    }
}
