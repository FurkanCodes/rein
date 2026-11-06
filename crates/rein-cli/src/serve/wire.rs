//! What goes over `rein serve`'s standard input and output: JSON-RPC, one
//! message a line, as the Agent Client Protocol has it
//! (<https://agentclientprotocol.com>). Read into types here, once; written
//! here too, so the rest of `serve` doesn't spell a field.

use std::path::{Path, PathBuf};

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

/// A client that can start another `rein serve` says so as it starts, in
/// its `clientCapabilities`, and only then does rein join the other reins:
/// what they send starts turns nobody asked for, and a sub-agent needs
/// somewhere to run. ACP's draft for sub-agents lets a client stop one, but
/// not prompt or steer it, and here each one is a `rein serve` of its own.
pub(super) const PANES: &str = "io.github.furkancodes/panes";
/// Asks a client that said [`PANES`] to start a `rein serve` for a sub-agent.
const OPEN_PANE: &str = "_io.github.furkancodes/open_pane";
/// Another rein's words went into the conversation. They're never the
/// user's, so they aren't a `user_message_chunk`.
const HEARD: &str = "_io.github.furkancodes/heard";
/// A turn nobody prompted starts, on what another rein sent: ACP's turns
/// are all `session/prompt`s, with an answer to say they ended.
const TURN_STARTED: &str = "_io.github.furkancodes/turn_started";
const TURN_ENDED: &str = "_io.github.furkancodes/turn_ended";

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
    Initialize {
        panes: bool,
    },
    NewSession {
        cwd: PathBuf,
    },
    /// Saved conversations: `cwd`'s, or all of them, after the first `skip`.
    ListSessions {
        cwd: Option<PathBuf>,
        skip: usize,
    },
    LoadSession {
        session: String,
        cwd: PathBuf,
    },
    Prompt {
        session: String,
        text: String,
    },
    SetMode {
        session: String,
        mode: Mode,
    },
    Steer {
        session: String,
        text: String,
    },
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
struct ListSessions {
    cwd: Option<PathBuf>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadSession {
    session_id: String,
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
        "initialize" => Ok(Request::Initialize { panes: params["clientCapabilities"]["_meta"][PANES] == true }),
        "session/new" => parse::<NewSession>(params).map(|new| Request::NewSession { cwd: new.cwd }),
        "session/list" => {
            // Nothing's required of it, so a client may send no params.
            let list: ListSessions = parse(if params.is_null() { json!({}) } else { params })?;
            let skip = list.cursor.map_or(Ok(0), |cursor| cursor.parse().map_err(|_| Refused::params(format!("that isn't a cursor rein gave: {cursor}"))))?;
            Ok(Request::ListSessions { cwd: list.cwd, skip })
        }
        "session/load" => parse::<LoadSession>(params).map(|load| Request::LoadSession { session: load.session_id, cwd: load.cwd }),
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

/// What a client starts for a sub-agent, with no shell in between: it
/// runs `command` with `args`, in `cwd`.
pub(super) struct Pane<'a> {
    pub(super) name: &'a str,
    pub(super) model: &'a str,
    pub(super) command: &'a str,
    pub(super) args: &'a [String],
    pub(super) cwd: &'a Path,
}

/// Asks a client to open `pane`, under the number `id`.
pub(super) fn open_pane(id: u64, session: &str, pane: &Pane<'_>) -> String {
    let params = json!({ "sessionId": session, "name": pane.name, "model": pane.model, "command": pane.command, "args": pane.args, "cwd": pane.cwd });
    json!({ "jsonrpc": "2.0", "id": id, "method": OPEN_PANE, "params": params }).to_string()
}

fn notification(method: &str, params: &Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

/// `from`, another rein, said `text`, and it went into the conversation.
pub(super) fn heard(session: &str, from: &str, text: &str) -> String {
    notification(HEARD, &json!({ "sessionId": session, "from": from, "text": text }))
}

pub(super) fn turn_started(session: &str) -> String {
    notification(TURN_STARTED, &json!({ "sessionId": session }))
}

/// A turn nobody prompted ended, for `stop`, ACP's word for why.
pub(super) fn turn_ended(session: &str, stop: &str) -> String {
    notification(TURN_ENDED, &json!({ "sessionId": session, "stopReason": stop }))
}

/// A turn nobody prompted failed, so there's no answer to carry the error.
pub(super) fn turn_failed(session: &str, error: &str) -> String {
    notification(TURN_ENDED, &json!({ "sessionId": session, "error": error }))
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
        let cursor = json!({ "id": 1, "method": "session/list", "params": { "cursor": "anywhere" } });
        assert_eq!(asked(cursor).unwrap_err().code, INVALID_PARAMS);
        let unknown = json!({ "id": 1, "method": "session/fork", "params": {} });
        assert_eq!(asked(unknown).unwrap_err().code, METHOD_NOT_FOUND);
        assert!(matches!(Incoming::read("not json"), Incoming::Unreadable(Refused { code: PARSE_ERROR, .. })));
    }

    #[test]
    fn a_list_needs_nothing_and_a_page_goes_by_the_cursor_rein_gave() {
        assert_eq!(asked(json!({ "id": 1, "method": "session/list" })), Ok(Request::ListSessions { cwd: None, skip: 0 }));
        let next = json!({ "id": 1, "method": "session/list", "params": { "cwd": "/p", "cursor": "100" } });
        assert_eq!(asked(next), Ok(Request::ListSessions { cwd: Some("/p".into()), skip: 100 }));
    }

    #[test]
    fn only_a_client_that_says_so_opens_panes() {
        let starts = |capabilities: Value| asked(json!({ "id": 1, "method": "initialize", "params": { "protocolVersion": 1, "clientCapabilities": capabilities } }));
        assert_eq!(starts(json!({ "_meta": { PANES: true } })), Ok(Request::Initialize { panes: true }));
        assert_eq!(starts(json!({ "_meta": { PANES: "yes" } })), Ok(Request::Initialize { panes: false }), "not a clear yes");
        assert_eq!(starts(json!({ "fs": { "readTextFile": true } })), Ok(Request::Initialize { panes: false }));
        assert_eq!(asked(json!({ "id": 1, "method": "initialize" })), Ok(Request::Initialize { panes: false }));
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
