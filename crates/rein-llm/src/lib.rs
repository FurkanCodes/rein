//! Streaming LLM provider layer for rein.
//!
//! Stage 1 speaks OpenAI's Responses API, authenticated either with an
//! OpenAI API key or with Sign in with ChatGPT (the user's ChatGPT plan).

pub mod openai;
pub mod sse;
pub mod store;

pub use reqwest;
pub use serde_json;

use std::fmt;

/// One thing that happened while the model was answering.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A piece of the visible answer text, for showing as it's written.
    TextDelta(String),
    /// A piece of a short summary of the model's reasoning, written while
    /// it thinks. Only shown; the reasoning itself comes as an item.
    Thought(String),
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
    /// encrypted reasoning. rein never looks inside.
    Opaque(serde_json::Value),
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
    pub input_tokens: u64,
    /// The part of `input_tokens` served from the provider's prompt cache.
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// The part of `output_tokens` spent on hidden reasoning.
    pub reasoning_tokens: u64,
}

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
}
