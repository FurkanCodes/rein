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
pub mod gemini;
mod oauth;
pub mod openai;
pub mod openrouter;
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
    /// Gemini's own, `POST /models/{model}:streamGenerateContent`.
    Gemini,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Api::Responses => "responses",
            Api::Anthropic => "anthropic",
            Api::Chat => "chat",
            Api::Gemini => "gemini",
        }
    }

    pub fn parse(name: &str) -> Option<Api> {
        [Api::Responses, Api::Anthropic, Api::Chat, Api::Gemini].into_iter().find(|api| api.name() == name)
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
/// way of saying it. It borrows what it carries: the conversation can be
/// hundreds of kilobytes, and nothing that reads a request needs its own
/// copy of it.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub model: &'a str,
    /// The system prompt.
    pub instructions: Option<&'a str>,
    /// The whole conversation so far. Nothing is kept on the server, so
    /// every request carries it all.
    pub input: &'a [Item],
    /// The tools the model may call.
    pub tools: &'a [Tool],
    /// How hard the model thinks. `None` uses the model's default.
    pub reasoning_effort: Option<Effort>,
    /// Ask for short summaries of the model's reasoning as it thinks
    /// ([`Event::Thought`]). Only for models that reason.
    pub reasoning_summary: bool,
    /// The same ID on every request of one conversation. Sign in with ChatGPT
    /// routes requests to its prompt cache by the `session-id` header, so
    /// without it nothing is ever cached; see [`new_session_id`].
    pub session_id: Option<&'a str>,
    /// The most the answer may take, where the format needs a number
    /// (Anthropic's does). `None` lets rein pick.
    pub max_output_tokens: Option<u64>,
}

/// How hard a model thinks, in rein's words. Each format says it its own
/// way, and every one of them has to decide what each level means: a
/// `match` on this is exhaustive, so a new level can't be forgotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    /// Beyond `High`, where the model has it.
    XHigh,
    /// The most there is, where the model has it.
    Max,
}

impl Effort {
    pub const ALL: [Effort; 6] = [Self::Minimal, Self::Low, Self::Medium, Self::High, Self::XHigh, Self::Max];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The levels, for messages: `minimal, low, medium, high, xhigh or max`.
    pub fn choices() -> String {
        let names: Vec<&str> = Self::ALL.iter().map(|e| e.as_str()).collect();
        format!("{} or {}", names[..names.len() - 1].join(", "), names[names.len() - 1])
    }
}

impl std::str::FromStr for Effort {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|e| e.as_str() == name.trim()).ok_or_else(|| format!("no effort called {name}; use {}", Self::choices()))
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A fresh ID for one conversation, for [`Request::session_id`].
pub fn new_session_id() -> Result<String> {
    oauth::new_uuid()
}

/// An ID for a tool call whose provider gave it none: unique in the
/// conversation, as rein needs.
pub(crate) fn new_call_id() -> String {
    format!("call_{}", oauth::new_uuid().unwrap_or_default().replace('-', ""))
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

/// What kind of failure an [`Error::Api`] is, whoever sent it and in
/// whatever words: what rein acts on. The provider's own code stays as it
/// was, next to it, for messages and support.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The conversation no longer fits the model's window: compact, and
    /// ask again.
    ContextFull,
    /// The provider turned Claude's thinking away because the conversation
    /// before it changed: drop that thinking, and ask again.
    StaleThinking,
    /// The model, or a filter in front of it, declined to answer.
    Refusal,
    /// The plan or account has no usage left: waiting a few seconds won't
    /// help.
    UsageLimit,
    Other,
}

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The provider rejected the request, or failed while answering.
    Api {
        status: Option<u16>,
        kind: ErrorKind,
        /// The provider's own code, as it sent it.
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
    /// An error a provider sent: its kind comes from its code, or where
    /// that says nothing rein knows, from its words.
    pub fn api(status: Option<u16>, code: Option<String>, message: String, request_id: Option<String>) -> Error {
        let lower = message.to_lowercase();
        let rejected = status.is_none_or(|s| matches!(s, 400 | 413 | 422));
        let full =
            ["prompt is too long", "context length", "context window", "maximum context", "exceeds the maximum number of tokens", "input token count", "too many input tokens"];
        let named = code.as_deref().unwrap_or_default();
        let kind = if named == "context_length_exceeded" || rejected && full.iter().any(|words| lower.contains(words)) {
            ErrorKind::ContextFull
        } else if rejected && lower.contains("signature") && lower.contains("thinking") {
            ErrorKind::StaleThinking
        } else if named.contains("usage_limit") || named.contains("quota") {
            ErrorKind::UsageLimit
        } else {
            ErrorKind::Other
        };
        Error::Api { status, kind, code, message, request_id }
    }

    /// The model declined to answer, saying `why`.
    pub fn refusal(why: impl Into<String>) -> Error {
        Error::Api { status: None, kind: ErrorKind::Refusal, code: None, message: why.into(), request_id: None }
    }

    /// The conversation doesn't fit: `why`, in the provider's words.
    pub fn context_full(why: impl Into<String>) -> Error {
        Error::Api { status: None, kind: ErrorKind::ContextFull, code: None, message: why.into(), request_id: None }
    }

    /// What kind of failure this is, where a provider said; `None` for the
    /// network, files and the like.
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            Error::Api { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// The provider's own machine-readable code, if it sent one.
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
        match self {
            Error::Api { kind: ErrorKind::UsageLimit, .. } => false,
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
            Error::Api { status, code, message, request_id, .. } => {
                write!(f, "{message}")?;
                let details: Vec<String> = [status.map(|s| format!("HTTP {s}")), code.clone(), request_id.as_ref().map(|r| format!("request {r}"))].into_iter().flatten().collect();
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

impl std::error::Error for Error {
    /// What went wrong underneath, so the whole chain can be shown: the
    /// network error under "error sending request" says DNS or TLS.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Http(e) => Some(e),
            Error::Io(e) => Some(e),
            Error::Json(e) => Some(e),
            _ => None,
        }
    }
}

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
        Error::api(Some(status), code.map(str::to_owned), String::new(), None)
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
        assert_eq!(api(400, "prompt is too long: 210000 tokens > 200000 maximum").kind(), Some(ErrorKind::ContextFull));
        assert_eq!(api(400, "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).").kind(), Some(ErrorKind::ContextFull));
        let stale = "messages.5.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.";
        assert_eq!(api(400, stale).kind(), Some(ErrorKind::StaleThinking));
        assert_eq!(api(400, stale).code(), Some("invalid_request_error"), "the provider's own code stays");
        assert_eq!(api(400, "max_tokens: field required").kind(), Some(ErrorKind::Other));
        assert_eq!(api(500, "context window service down").kind(), Some(ErrorKind::Other), "only a rejected request");
        let openai = Error::api(Some(400), Some("context_length_exceeded".into()), "too long".into(), None);
        assert_eq!(openai.kind(), Some(ErrorKind::ContextFull));
        assert_eq!(Error::refusal("no").kind(), Some(ErrorKind::Refusal));
    }

    #[test]
    fn the_cause_underneath_can_be_had() {
        use std::error::Error as _;
        let io = Error::from(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert_eq!(io.source().map(ToString::to_string).as_deref(), Some("gone"));
        assert!(Error::Interrupted.source().is_none());
    }

    #[test]
    fn only_claude_thinking_is_bound_to_what_came_before() {
        let thinking = Opaque::new(Api::Anthropic, serde_json::json!({"type": "thinking", "thinking": "", "signature": "s"}));
        assert!(thinking.bound());
        assert!(Opaque::new(Api::Anthropic, serde_json::json!({"type": "redacted_thinking", "data": "d"})).bound());
        assert!(!Opaque::new(Api::Responses, serde_json::json!({"type": "reasoning", "encrypted_content": "x"})).bound());
    }
}
