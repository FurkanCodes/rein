//! Streaming LLM provider layer for rein.
//!
//! Stage 1 speaks OpenAI's Responses API, authenticated either with an
//! OpenAI API key or with Sign in with ChatGPT (the user's ChatGPT plan).

pub mod openai;
pub mod sse;
pub mod store;

pub use reqwest;

use std::fmt;

/// One thing that happened while the model was answering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A piece of the visible answer text.
    TextDelta(String),
    /// The response finished normally. Always the last event.
    Completed { usage: Usage },
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
