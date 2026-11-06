//! What goes over `rein serve`'s standard input and output: JSON-RPC, one
//! message a line, as the Agent Client Protocol has it
//! (<https://agentclientprotocol.com>). Read into types here, once; written
//! here too, so the rest of `serve` doesn't spell a field.

use std::path::PathBuf;

use rein_agent::permissions::Mode;
use serde::Deserialize;
use serde_json::{Value, json};

/// The protocol's major version: the only one rein speaks.
pub(super) const VERSION: u64 = 1;

/// Steering, which ACP has no word for: its turn only ends or is cancelled.
/// Under a name of rein's own, as ACP asks of what it doesn't define.
const STEER: &str = "_io.github.furkancodes/steer";
/// How `initialize` says rein takes [`STEER`].
pub(super) const STEERS: &str = "io.github.furkancodes/steer";

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
pub(super) const INTERNAL: i64 = -32603;
const PARSE_ERROR: i64 = -32700;

/// Why a request isn't answered with what it asked for.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Refused {
    pub(super) code: i64,
    pub(super) message: String,
}

impl Refused {
    pub(super) fn new(message: impl Into<String>) -> Refused {
        Refused { code: INTERNAL, message: message.into() }
    }

    fn params(message: impl Into<String>) -> Refused {
        Refused { code: INVALID_PARAMS, message: message.into() }
    }
}

/// What a client can ask of rein.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    Initialize,
    NewSession { cwd: PathBuf },
    Prompt { session: String, text: String },
    SetMode { session: String, mode: Mode },
    Steer { session: String, text: String },
}

/// One line that came in.
#[derive(Debug, PartialEq)]
pub(super) enum Incoming {
    /// It wants an answer under `id`.
    Request { id: Value, request: Result<Request, Refused> },
    /// `session/cancel`: stop the turn.
    Cancel,
    /// The answer to something rein asked, under the number it asked with.
    Reply { id: u64, result: Option<Value> },
    /// A notification rein doesn't know, which ACP says to ignore.
    Ignored,
    /// Not a message at all.
    Unreadable(Refused),
}

#[derive(Deserialize)]
struct Line {
    id: Option<Value>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSession {
    cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Prompt {
    session_id: String,
    prompt: Vec<Block>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMode {
    session_id: String,
    mode_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Steer {
    session_id: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    ResourceLink {
        uri: String,
    },
    #[serde(other)]
    Other,
}

impl Incoming {
    pub(super) fn read(line: &str) -> Incoming {
        let line: Line = match serde_json::from_str(line) {
            Ok(line) => line,
            Err(e) => return Incoming::Unreadable(Refused { code: PARSE_ERROR, message: format!("that isn't JSON-RPC: {e}") }),
        };
        match (line.method, line.id) {
            (Some(method), Some(id)) => Incoming::Request { id, request: request(&method, line.params) },
            (Some(method), None) if method == "session/cancel" => Incoming::Cancel,
            (Some(_), None) => Incoming::Ignored,
            // rein numbers what it asks, so any other id is nothing it asked.
            (None, Some(id)) => id.as_u64().map_or(Incoming::Ignored, |id| Incoming::Reply { id, result: line.result }),
            (None, None) => Incoming::Unreadable(Refused::params("a message needs a method or an id")),
        }
    }
}

fn request(method: &str, params: Value) -> Result<Request, Refused> {
    fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, Refused> {
        serde_json::from_value(params).map_err(|e| Refused::params(e.to_string()))
    }
    match method {
        "initialize" => Ok(Request::Initialize),
        "session/new" => parse::<NewSession>(params).map(|new| Request::NewSession { cwd: new.cwd }),
        "session/prompt" => {
            let prompt: Prompt = parse(params)?;
            Ok(Request::Prompt { session: prompt.session_id, text: text_of(prompt.prompt)? })
        }
        "session/set_mode" => {
            let set: SetMode = parse(params)?;
            let mode = set.mode_id.parse().map_err(Refused::params)?;
            Ok(Request::SetMode { session: set.session_id, mode })
        }
        STEER => parse::<Steer>(params).map(|steer| Request::Steer { session: steer.session_id, text: steer.message }),
        _ => Err(Refused { code: METHOD_NOT_FOUND, message: format!("rein has no method {method}") }),
    }
}

/// A prompt as one message: a file it links to named with `@`, which reads
/// it in, as you'd type it.
fn text_of(blocks: Vec<Block>) -> Result<String, Refused> {
    let mut pieces = Vec::with_capacity(blocks.len());
    for block in blocks {
        pieces.push(match block {
            Block::Text { text } => text,
            Block::ResourceLink { uri } => uri.strip_prefix("file://").map_or_else(|| uri.clone(), |path| format!("@{path}")),
            Block::Other => return Err(Refused::params("rein takes text and links to files in a prompt so far")),
        });
    }
    Ok(pieces.join("\n"))
}

/// The answer to `id`.
pub(super) fn answer(id: &Value, result: &Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// `id` turned down.
pub(super) fn refuse(id: &Value, refused: &Refused) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": refused.code, "message": refused.message } }).to_string()
}

/// A `session/update` for `session`.
pub(super) fn update(session: &str, update: &Value) -> String {
    json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session, "update": update } }).to_string()
}

/// Asks whether a tool call may run, under the number `id`.
pub(super) fn ask_permission(id: u64, session: &str, call: &str, options: &Value) -> String {
    let params = json!({ "sessionId": session, "toolCall": { "toolCallId": call }, "options": options });
    json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": params }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(line: Value) -> Result<Request, Refused> {
        match Incoming::read(&line.to_string()) {
            Incoming::Request { request, .. } => request,
            other => panic!("not a request: {other:?}"),
        }
    }

    #[test]
    fn a_prompts_text_and_links_become_one_message() {
        let prompt = json!({ "id": 1, "method": "session/prompt", "params": { "sessionId": "s", "prompt": [
            { "type": "text", "text": "look at this" },
            { "type": "resource_link", "uri": "file:///p/a.rs", "name": "a.rs" },
            { "type": "resource_link", "uri": "https://a.example/b", "name": "b" },
        ] } });
        assert_eq!(asked(prompt), Ok(Request::Prompt { session: "s".into(), text: "look at this\n@/p/a.rs\nhttps://a.example/b".into() }));
    }

    #[test]
    fn what_rein_cant_take_says_so_and_isnt_guessed_at() {
        let image = json!({ "id": 1, "method": "session/prompt", "params": { "sessionId": "s", "prompt": [{ "type": "image", "data": "x" }] } });
        assert_eq!(asked(image).unwrap_err().code, INVALID_PARAMS);
        let mode = json!({ "id": 1, "method": "session/set_mode", "params": { "sessionId": "s", "modeId": "yolo" } });
        assert_eq!(asked(mode).unwrap_err().code, INVALID_PARAMS);
        let unknown = json!({ "id": 1, "method": "session/fork", "params": {} });
        assert_eq!(asked(unknown).unwrap_err().code, METHOD_NOT_FOUND);
        assert!(matches!(Incoming::read("not json"), Incoming::Unreadable(Refused { code: PARSE_ERROR, .. })));
    }

    #[test]
    fn a_notification_is_never_answered() {
        assert_eq!(Incoming::read(r#"{"method":"session/cancel","params":{"sessionId":"s"}}"#), Incoming::Cancel);
        assert_eq!(Incoming::read(r#"{"method":"_other/thing"}"#), Incoming::Ignored);
    }

    #[test]
    fn a_reply_goes_by_the_number_rein_asked_with() {
        assert_eq!(Incoming::read(r#"{"id":7,"result":{"a":1}}"#), Incoming::Reply { id: 7, result: Some(json!({ "a": 1 })) });
        assert_eq!(Incoming::read(r#"{"id":7,"error":{"code":1,"message":"no"}}"#), Incoming::Reply { id: 7, result: None });
        assert_eq!(Incoming::read(r#"{"id":"x","result":{}}"#), Incoming::Ignored);
    }
}
