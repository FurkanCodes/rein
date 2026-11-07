//! What goes over `rein serve`'s standard input and output: JSON-RPC, one
//! message a line, as the Agent Client Protocol has it
//! (<https://agentclientprotocol.com>). Read into types here, once; written
//! here too, so the rest of `serve` doesn't spell a field.

use std::path::{Path, PathBuf};

use rein_agent::permissions::Mode;
use serde::Deserialize;

use crate::input::{Menu, Question};
use crate::ui::asked::Asked;
use crate::ui::steps::Whole;
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
/// A client that can show what a command asks says so as it starts, and
/// gets these: the list to choose from, the line to type, the question to
/// answer. ACP has a slash command run as a prompt and has no word for any
/// of them, so a client without them gets a command's answer but no menu.
pub(super) const ASKS: &str = "io.github.furkancodes/asks";
const CHOOSE: &str = "_io.github.furkancodes/choose";
const READ: &str = "_io.github.furkancodes/read";
const YES: &str = "_io.github.furkancodes/yes";
/// What you sent from this folder before, for ↑ and for finding it again.
const SENT: &str = "_io.github.furkancodes/sent";
/// What `@` can point at in this project.
const PATHS: &str = "_io.github.furkancodes/paths";
/// Sends the command that's running to the background, as Ctrl-B does.
const BACKGROUND: &str = "_io.github.furkancodes/background";
/// The commands in the background, and stopping one.
const TASKS: &str = "_io.github.furkancodes/tasks";
const STOP_TASK: &str = "_io.github.furkancodes/stop_task";
/// Looks up the other providers' models, which a list of them needs.
const MODELS: &str = "_io.github.furkancodes/models";
/// What's taking room in the conversation.
const ROOM: &str = "_io.github.furkancodes/room";
/// The turns there are to undo, what undoing one does, and doing it.
const UNDO_TURNS: &str = "_io.github.furkancodes/undo_turns";
const UNDO_PLAN: &str = "_io.github.furkancodes/undo_plan";
const UNDO: &str = "_io.github.furkancodes/undo";
/// Marks a thought as rein's own note, a whole one: a check, a hook, a
/// command in the background.
const NOTE: &str = "io.github.furkancodes/note";
/// Marks a note as the line that ends a turn, and the turn to undo from it.
const TURN: &str = "io.github.furkancodes/turn";
/// Marks a command as a skill.
const SKILL: &str = "io.github.furkancodes/skill";
/// What an image is called in the message it goes with: `[Image #1]`.
const LABEL: &str = "io.github.furkancodes/label";
/// Marks text as what a command said, which is rein's own word and nothing
/// the model wrote. A client that doesn't know it shows it as any answer.
const SAID: &str = "io.github.furkancodes/said";
/// A `user_message_chunk` that's a whole message. ACP's chunks of one role
/// run together, and two commands in a row read `/model/effort`.
const WHOLE: &str = "io.github.furkancodes/whole";
/// Another rein's words went into the conversation. They're never the
/// user's, so they aren't a `user_message_chunk`.
const HEARD: &str = "_io.github.furkancodes/heard";
/// A turn nobody prompted starts, on what another rein sent: ACP's turns
/// are all `session/prompt`s, with an answer to say they ended.
const TURN_STARTED: &str = "_io.github.furkancodes/turn_started";
const TURN_ENDED: &str = "_io.github.furkancodes/turn_ended";
/// What ACP has no word for and a client shows: the branch, and how much of
/// your limits is used.
const STATE: &str = "_io.github.furkancodes/state";

pub(super) const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
pub(super) const INTERNAL: i64 = -32603;
const PARSE_ERROR: i64 = -32700;
/// A request that can't be taken while a turn runs: a number of its own, so
/// a client needn't read the words to tell it from a turn that failed.
const BUSY: i64 = -32001;

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

    pub(super) fn params(message: impl Into<String>) -> Refused {
        Refused { code: INVALID_PARAMS, message: message.into() }
    }

    pub(super) fn busy(message: impl Into<String>) -> Refused {
        Refused { code: BUSY, message: message.into() }
    }
}

/// What a client can ask of rein.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    Initialize {
        panes: bool,
        asks: bool,
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
        images: Vec<Pasted>,
    },
    /// These are answered while a turn runs: they need none of what it holds.
    Sent {
        session: String,
    },
    Paths {
        session: String,
    },
    Background {
        session: String,
    },
    Tasks {
        session: String,
    },
    StopTask {
        session: String,
        id: String,
    },
    Room {
        session: String,
    },
    /// These wait for the turn: they change what it's working on.
    Models {
        session: String,
    },
    UndoTurns {
        session: String,
    },
    UndoPlan {
        session: String,
        turn: usize,
    },
    Undo {
        session: String,
        turn: usize,
    },
    SetMode {
        session: String,
        mode: Mode,
    },
    Steer {
        session: String,
        text: String,
    },
    /// `session/set_config_option`.
    SetOption {
        session: String,
        option: String,
        value: String,
    },
}

impl Request {
    /// It's answered as a turn runs, and between them.
    pub(super) const fn beside_a_turn(&self) -> bool {
        matches!(self, Request::Sent { .. } | Request::Paths { .. } | Request::Background { .. } | Request::Tasks { .. } | Request::StopTask { .. } | Request::Room { .. })
    }
}

/// An image that came with a prompt.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Pasted {
    pub(super) bytes: Vec<u8>,
    /// What the message calls it, if it does.
    pub(super) label: Option<String>,
}

/// One line that came in.
#[derive(Debug, PartialEq)]
pub(super) enum Incoming {
    /// It wants an answer under `id`.
    Request { id: Value, request: Result<Request, Refused> },
    /// `session/cancel`: stop the turn.
    Cancel,
    /// The answer to something rein asked, under the number it asked with:
    /// `None` for an error. `null` is an answer: it's what a method with
    /// nothing to say comes back with.
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
    #[serde(default)]
    result: Value,
    error: Option<Value>,
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
struct InSession {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfTurn {
    session_id: String,
    turn: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OfTask {
    session_id: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMode {
    session_id: String,
    mode_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetOption {
    session_id: String,
    config_id: String,
    value: String,
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
    Image {
        data: String,
        #[serde(default, rename = "_meta")]
        meta: Value,
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
            (None, Some(id)) => id.as_u64().map_or(Incoming::Ignored, |id| Incoming::Reply { id, result: line.error.is_none().then_some(line.result) }),
            (None, None) => Incoming::Unreadable(Refused::params("a message needs a method or an id")),
        }
    }
}

fn request(method: &str, params: Value) -> Result<Request, Refused> {
    fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, Refused> {
        serde_json::from_value(params).map_err(|e| Refused::params(e.to_string()))
    }
    match method {
        "initialize" => {
            let meta = &params["clientCapabilities"]["_meta"];
            Ok(Request::Initialize { panes: meta[PANES] == true, asks: meta[ASKS] == true })
        }
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
            let (text, images) = read_prompt(prompt.prompt)?;
            Ok(Request::Prompt { session: prompt.session_id, text, images })
        }
        SENT => parse::<InSession>(params).map(|asked| Request::Sent { session: asked.session_id }),
        PATHS => parse::<InSession>(params).map(|asked| Request::Paths { session: asked.session_id }),
        BACKGROUND => parse::<InSession>(params).map(|asked| Request::Background { session: asked.session_id }),
        TASKS => parse::<InSession>(params).map(|asked| Request::Tasks { session: asked.session_id }),
        STOP_TASK => parse::<OfTask>(params).map(|asked| Request::StopTask { session: asked.session_id, id: asked.id }),
        ROOM => parse::<InSession>(params).map(|asked| Request::Room { session: asked.session_id }),
        MODELS => parse::<InSession>(params).map(|asked| Request::Models { session: asked.session_id }),
        UNDO_TURNS => parse::<InSession>(params).map(|asked| Request::UndoTurns { session: asked.session_id }),
        UNDO_PLAN => parse::<OfTurn>(params).map(|asked| Request::UndoPlan { session: asked.session_id, turn: asked.turn }),
        UNDO => parse::<OfTurn>(params).map(|asked| Request::Undo { session: asked.session_id, turn: asked.turn }),
        "session/set_mode" => {
            let set: SetMode = parse(params)?;
            let mode = set.mode_id.parse().map_err(Refused::params)?;
            Ok(Request::SetMode { session: set.session_id, mode })
        }
        "session/set_config_option" => parse::<SetOption>(params).map(|set| Request::SetOption { session: set.session_id, option: set.config_id, value: set.value }),
        STEER => parse::<Steer>(params).map(|steer| Request::Steer { session: steer.session_id, text: steer.message }),
        _ => Err(Refused { code: METHOD_NOT_FOUND, message: format!("rein has no method {method}") }),
    }
}

/// A prompt as one message, and the images that go with it: a file it links
/// to is named with `@`, which reads it in, as you'd type it.
fn read_prompt(blocks: Vec<Block>) -> Result<(String, Vec<Pasted>), Refused> {
    use base64::Engine as _;
    let mut pieces = Vec::with_capacity(blocks.len());
    let mut images = Vec::new();
    for block in blocks {
        pieces.push(match block {
            Block::Text { text } => text,
            Block::ResourceLink { uri } => uri.strip_prefix("file://").map_or_else(|| uri.clone(), |path| format!("@{path}")),
            Block::Image { data, meta } => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| Refused::params(format!("that image isn't base64: {e}")))?;
                images.push(Pasted { bytes, label: meta[LABEL].as_str().map(str::to_owned) });
                continue;
            }
            Block::Other => return Err(Refused::params("rein takes text, images and links to files in a prompt")),
        });
    }
    Ok((pieces.join("\n"), images))
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

/// Marks the answer that says no and hands the conversation back to you.
pub(super) const INSTEAD: &str = "io.github.furkancodes/instead";
/// What rein asks, in its own words, which ACP has no place for.
const QUESTION: &str = "io.github.furkancodes/question";
/// Why it's asked.
const WHY: &str = "io.github.furkancodes/why";
/// The line an edit starts on, which a `diff` has no place for.
const LINE: &str = "io.github.furkancodes/line";

/// Asks whether a tool call may run, under the number `id`.
pub(super) fn ask_permission(id: u64, session: &str, call: &str, options: &Value, asked: &Asked) -> String {
    let meta = json!({ QUESTION: asked.question, WHY: asked.reason });
    let params = json!({ "sessionId": session, "toolCall": { "toolCallId": call }, "options": options, "_meta": meta });
    json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": params }).to_string()
}

/// A change to a file, as ACP's `diff`.
pub(super) fn diff(change: &Whole) -> Value {
    json!({ "type": "diff", "path": change.path, "oldText": change.old, "newText": change.new, "_meta": { LINE: change.line } })
}

/// All that can be set, as it stands now.
pub(super) fn options(session: &str, options: &Value) -> String {
    update(session, &json!({ "sessionUpdate": "config_option_update", "configOptions": options }))
}

/// Says what a client shows that ACP has no word for.
pub(super) fn state(session: &str, branch: Option<&str>, meters: &[crate::meters::Cell], background: usize) -> String {
    let meters: Vec<Value> = meters
        .iter()
        .map(|cell| json!({ "label": cell.label, "used": cell.used, "said": cell.said, "resets": cell.resets, "warn": cell.tone != rein_llm::Standing::Fine }))
        .collect();
    notification(STATE, &json!({ "sessionId": session, "branch": branch, "meters": meters, "background": background }))
}

/// A note of rein's own, whole.
pub(super) fn noted(session: &str, text: &str) -> String {
    update(session, &json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text }, "_meta": { NOTE: true } }))
}

/// The line that ends a turn, and the turn it can be undone back to.
pub(super) fn turn_line(session: &str, text: &str, turn: Option<usize>) -> String {
    update(session, &json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text }, "_meta": { NOTE: true, TURN: turn } }))
}

/// A skill, as a command: only these load from later in a message.
pub(super) fn skill(name: &str, about: &str) -> Value {
    json!({ "name": name, "description": about, "_meta": { SKILL: true } })
}

/// A message of yours, whole.
pub(super) fn yours(session: &str, text: &str) -> String {
    update(session, &json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": text }, "_meta": { WHOLE: true } }))
}

/// Text a command said.
pub(super) fn said(session: &str, text: &str) -> String {
    update(session, &json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text }, "_meta": { SAID: true } }))
}

/// Says what can be run in `session`: `commands` are ACP's `availableCommands`.
pub(super) fn commands(session: &str, commands: &[Value]) -> String {
    update(session, &json!({ "sessionUpdate": "available_commands_update", "availableCommands": commands }))
}

fn asking(id: u64, method: &str, params: &Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

/// Asks which of `menu`, under the number `id`.
pub(super) fn choose(id: u64, session: &str, menu: &Menu) -> String {
    let items: Vec<Value> = menu.items.iter().map(|(name, about)| json!({ "name": name, "about": about })).collect();
    asking(id, CHOOSE, &json!({ "sessionId": session, "title": menu.title, "items": items, "current": menu.current, "at": menu.at }))
}

/// Asks for a line, under the number `id`. One that's `secret` is never shown.
pub(super) fn read(id: u64, session: &str, prompt: &str, secret: bool) -> String {
    asking(id, READ, &json!({ "sessionId": session, "prompt": prompt, "secret": secret }))
}

/// Asks `question`, under the number `id`.
pub(super) fn yes(id: u64, session: &str, question: &Question) -> String {
    let answers: Vec<Value> = question.answers.iter().map(|(key, says)| json!({ "key": key, "says": says })).collect();
    asking(id, YES, &json!({ "sessionId": session, "title": question.title, "detail": question.detail, "command": question.command, "answers": answers }))
}

/// What a client starts for a sub-agent, with no shell in between: it
/// runs `command` with `args`, in `cwd`, with `env` added to what it has.
pub(super) struct Pane<'a> {
    pub(super) name: &'a str,
    pub(super) model: &'a str,
    pub(super) command: &'a str,
    pub(super) args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) env: &'a [(String, String)],
}

/// Asks a client to open `pane`, under the number `id`.
pub(super) fn open_pane(id: u64, session: &str, pane: &Pane<'_>) -> String {
    // As ACP says an MCP server's environment.
    let env: Vec<Value> = pane.env.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect();
    let params = json!({ "sessionId": session, "name": pane.name, "model": pane.model, "command": pane.command, "args": pane.args, "cwd": pane.cwd, "env": env });
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
    fn a_client_says_it_can_show_what_a_command_asks() {
        let said = json!({ "id": 1, "method": "initialize", "params": { "clientCapabilities": { "_meta": { ASKS: true } } } });
        assert_eq!(asked(said), Ok(Request::Initialize { panes: false, asks: true }));
    }

    #[test]
    fn a_choice_comes_in_as_acp_says() {
        let set = json!({ "id": 1, "method": "session/set_config_option", "params": { "sessionId": "a", "configId": "model", "value": "groq/llama" } });
        assert_eq!(asked(set), Ok(Request::SetOption { session: "a".to_owned(), option: "model".to_owned(), value: "groq/llama".to_owned() }));
        let unset = json!({ "id": 1, "method": "session/set_config_option", "params": { "sessionId": "a", "configId": "model" } });
        assert_eq!(asked(unset).unwrap_err().code, INVALID_PARAMS);
    }

    #[test]
    fn a_prompts_text_and_links_become_one_message() {
        let prompt = json!({ "id": 1, "method": "session/prompt", "params": { "sessionId": "s", "prompt": [
            { "type": "text", "text": "look at this" },
            { "type": "resource_link", "uri": "file:///p/a.rs", "name": "a.rs" },
            { "type": "resource_link", "uri": "https://a.example/b", "name": "b" },
        ] } });
        assert_eq!(asked(prompt), Ok(Request::Prompt { session: "s".into(), text: "look at this\n@/p/a.rs\nhttps://a.example/b".into(), images: Vec::new() }));
    }

    #[test]
    fn an_image_comes_with_its_message_under_the_name_it_goes_by() {
        let prompt = json!({ "id": 1, "method": "session/prompt", "params": { "sessionId": "s", "prompt": [
            { "type": "text", "text": "what's [Image #1]?" },
            { "type": "image", "data": "aGk=", "mimeType": "image/png", "_meta": { LABEL: "[Image #1]" } },
            { "type": "image", "data": "aGk=", "mimeType": "image/png" },
        ] } });
        let one = |label: Option<&str>| Pasted { bytes: b"hi".to_vec(), label: label.map(str::to_owned) };
        assert_eq!(asked(prompt), Ok(Request::Prompt { session: "s".into(), text: "what's [Image #1]?".into(), images: vec![one(Some("[Image #1]")), one(None)] }));
    }

    #[test]
    fn what_rein_cant_take_says_so_and_isnt_guessed_at() {
        let image = json!({ "id": 1, "method": "session/prompt", "params": { "sessionId": "s", "prompt": [{ "type": "image", "data": "not base64!" }] } });
        assert_eq!(asked(image).unwrap_err().code, INVALID_PARAMS);
        let audio = json!({ "id": 1, "method": "session/prompt", "params": { "sessionId": "s", "prompt": [{ "type": "audio", "data": "aGk=" }] } });
        assert_eq!(asked(audio).unwrap_err().code, INVALID_PARAMS);
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
        assert_eq!(starts(json!({ "_meta": { PANES: true } })), Ok(Request::Initialize { panes: true, asks: false }));
        assert_eq!(starts(json!({ "_meta": { PANES: "yes" } })), Ok(Request::Initialize { panes: false, asks: false }), "not a clear yes");
        assert_eq!(starts(json!({ "fs": { "readTextFile": true } })), Ok(Request::Initialize { panes: false, asks: false }));
        assert_eq!(asked(json!({ "id": 1, "method": "initialize" })), Ok(Request::Initialize { panes: false, asks: false }));
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
        assert_eq!(Incoming::read(r#"{"id":7,"result":null}"#), Incoming::Reply { id: 7, result: Some(Value::Null) }, "nothing to say isn't an error");
        assert_eq!(Incoming::read(r#"{"id":"x","result":{}}"#), Incoming::Ignored);
    }
}
