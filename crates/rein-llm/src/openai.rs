//! OpenAI's Responses API: `POST /v1/responses`, streamed.
//!
//! An API key and a Sign in with ChatGPT access token both go in the same
//! `Authorization: Bearer` header; only who pays differs.

pub mod signin;

use serde_json::{Value, json};

use crate::client::{Client, EFFORT, Optional};
use crate::decode::{Decoder, Translate};
use crate::sse::SseEvent;
use crate::{Api, Effort, Efforts, Error, Event, Item, ModelInfo, Opaque, Request, Result, ToolCall, Usage};

/// What the Responses API may turn away: an effort level the model doesn't
/// take, which OpenAI names as the error's `param`.
pub(crate) const OPTIONAL: Optional = &[("reasoning.effort", "reasoning", EFFORT)];

pub const API_BASE: &str = "https://api.openai.com/v1";

/// Sign in with ChatGPT only accepts function tools grouped in a namespace,
/// so all of rein's tools live in this one.
const NAMESPACE: &str = "rein";

/// The headers a request needs besides the body.
pub(crate) fn headers(request: Option<(&Request<'_>, &Value)>) -> Vec<(&'static str, String)> {
    // The prompt cache follows this header, not `prompt_cache_key`: the
    // server replaces that with a random key on every request.
    request.and_then(|(request, _)| request.session_id).map(|id| ("session-id", id.to_owned())).into_iter().collect()
}

/// The models this credential can use, in the server's order.
pub(crate) async fn models(client: &Client) -> Result<Vec<ModelInfo>> {
    Ok(parse_models(&client.get("models").await?))
}

pub(crate) fn request_body(request: &Request<'_>) -> Value {
    // Only what the Responses API can read: another format's reasoning
    // means nothing here.
    let input: Vec<Value> = request.input.iter().filter(|item| !matches!(item, Item::Opaque(o) if o.api != Api::Responses)).map(item_json).collect();
    // Sign in with ChatGPT requires `store: false` and `stream: true`, and
    // rejects fields like `temperature` and `max_output_tokens`. Sending only
    // what it accepts keeps one request shape for both ways of paying.
    let mut body = json!({ "model": request.model, "input": input, "store": false, "stream": true });
    if let Some(instructions) = &request.instructions {
        body["instructions"] = json!(instructions);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning"] = json!({ "effort": effort.wire() });
    }
    if request.reasoning_summary {
        body["reasoning"]["summary"] = json!("auto");
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> =
            request.tools.iter().map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": t.strict })).collect();
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
        Item::Opaque(opaque) => opaque.value.clone(),
    }
}

fn parse_models(body: &Value) -> Vec<ModelInfo> {
    // Sign in with ChatGPT lists {"models": [{slug, display_name, visibility,
    // supported_reasoning_levels, ..}]}; API keys get the standard
    // {"data": [{id}]}, which says nothing about effort.
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
                    max_output: None,
                    efforts: efforts(m),
                })
            })
            .collect();
    }
    let ids = body["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str());
    ids.map(ModelInfo::named).collect()
}

/// A ChatGPT model's levels, each with what it's for, and its default. One
/// rein doesn't know (`ultra` hands work to helpers, as only Codex does)
/// isn't offered.
fn efforts(m: &Value) -> Efforts {
    let Some(levels) = m["supported_reasoning_levels"].as_array() else { return Efforts::Unknown };
    let listed = levels.iter().filter_map(|level| Some((level["effort"].as_str()?.parse::<Effort>().ok()?, level["description"].as_str().map(str::to_owned))));
    Efforts::listed(listed, m["default_reasoning_level"].as_str().and_then(|name| name.parse().ok()))
}

/// Reads the Responses API's stream; see [`crate::decode`].
pub type EventDecoder = Decoder<Responses>;

/// The Responses API's events, as rein's.
#[derive(Debug, Default)]
pub struct Responses;

impl Translate for Responses {
    fn event(&mut self, sse: &SseEvent, out: &mut Vec<Result<Event>>) {
        out.extend(decode(sse));
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
        "response.reasoning_summary_text.delta" => Some(Ok(Event::Thought(v["delta"].as_str()?.to_owned()))),
        // A new part of the summary: keep it apart from the last one.
        "response.reasoning_summary_part.added" => Some(Ok(Event::Thought("\n\n".into()))),
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
        "reasoning" => Some(Item::Opaque(Opaque::new(Api::Responses, item.clone()))),
        _ => None,
    }
}

fn api_error(error: &Value, fallback: &str) -> Error {
    Error::api(None, str_field(error, "code"), str_field(error, "message").unwrap_or_else(|| fallback.to_owned()), None)
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
    use crate::Tool;

    #[test]
    fn request_body_has_only_fields_both_billing_paths_accept() {
        let input = [Item::User("hi".into()), Item::Assistant("hello".into()), Item::User("bye".into())];
        let request = Request {
            model: "m",
            instructions: Some("be brief"),
            input: &input,
            tools: &[],
            reasoning_effort: Some(crate::Effort::Low),
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
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
        let tools = [Tool { strict: true, name: "read".into(), description: "Read a file.".into(), parameters: json!({"type": "object"}) }];
        let input = [
            Item::User("open a.rs".into()),
            Item::Opaque(Opaque::new(Api::Responses, json!({"type": "reasoning", "encrypted_content": "xyz"}))),
            // Claude's thinking means nothing to OpenAI: it stays out.
            Item::Opaque(Opaque::new(Api::Anthropic, json!({"type": "thinking", "thinking": "", "signature": "s"}))),
            Item::ToolCall(call.clone()),
            Item::ToolResult { call_id: "call_1".into(), output: "fn main() {}".into() },
        ];
        let request =
            Request { model: "m", instructions: None, input: &input, tools: &tools, reasoning_effort: None, reasoning_summary: false, session_id: None, max_output_tokens: None };
        let body = request_body(&request);
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["tools"][0]["name"], "read");
        assert_eq!(body["input"][1], json!({"type": "reasoning", "encrypted_content": "xyz"}));
        assert_eq!(body["input"][2], json!({"type": "function_call", "call_id": "call_1", "namespace": "rein", "name": "read", "arguments": "{\"path\":\"a.rs\"}"}));
        assert_eq!(body["input"][3], json!({"type": "function_call_output", "call_id": "call_1", "output": "fn main() {}"}));
        assert_eq!(body["input"].as_array().unwrap().len(), 4);

        // The model's call comes back as the same item.
        let done = json!({"type": "function_call", "call_id": "call_1", "namespace": "rein", "name": "read", "arguments": "{\"path\":\"a.rs\"}"});
        assert_eq!(decode_item(&done), Some(Item::ToolCall(call)));
    }

    #[test]
    fn session_ids_are_fresh_v4_uuids() {
        let (a, b) = (crate::new_session_id().unwrap(), crate::new_session_id().unwrap());
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
        assert_eq!(parse_models(&chatgpt), [ModelInfo { id: "a".into(), name: "Model A".into(), context_window: Some(500_000), max_output: None, efforts: Efforts::Unknown }]);
        let api_key = json!({"data": [{"id": "b"}]});
        assert_eq!(parse_models(&api_key), [ModelInfo { id: "b".into(), name: "b".into(), context_window: None, max_output: None, efforts: Efforts::Unknown }]);
    }

    #[test]
    fn a_chatgpt_models_levels_come_from_its_list() {
        // As the list said for gpt-6.1-sol on 2026-11-03, trimmed.
        let chatgpt = json!({"models": [{"slug": "gpt-6.1-sol", "visibility": "list", "default_reasoning_level": "low", "supported_reasoning_levels": [
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
            {"effort": "xhigh", "description": "Extra high reasoning depth for complex problems"},
            {"effort": "max", "description": "Maximum reasoning depth for the hardest problems"},
            {"effort": "ultra", "description": "Maximum reasoning with automatic task delegation"},
        ]}]});
        let efforts = &parse_models(&chatgpt)[0].efforts;
        let levels: Vec<Effort> = efforts.offered().iter().map(|l| l.effort).collect();
        assert_eq!(levels, [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh, Effort::Max], "not ultra, which hands work to helpers");
        assert_eq!(efforts.offered()[0].about.as_deref(), Some("Fast responses with lighter reasoning"));
        assert_eq!(efforts.default(), Some(Effort::Low));
        assert_eq!(efforts.sent(Some(Effort::Minimal)), Some(Effort::Low), "below them all: its lowest");
    }

    #[test]
    fn off_goes_as_none() {
        let input = [Item::User("hi".into())];
        let request = Request {
            model: "m",
            instructions: None,
            input: &input,
            tools: &[],
            reasoning_effort: Some(Effort::Off),
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
        };
        assert_eq!(request_body(&request)["reasoning"]["effort"], "none");
    }
}
