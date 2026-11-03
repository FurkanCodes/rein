//! Anthropic's Messages API: `POST /v1/messages`, streamed.
//!
//! rein's conversation is a list of items; Claude's is a list of messages
//! that take turns, each a list of blocks. So the items are grouped: what
//! the model said, thought and called becomes one assistant message, and
//! what tools gave back, with anything the user said after, one user
//! message. Claude's thinking comes back signed, as blocks rein sends back
//! as they were; which model can read them is the server's business, and it
//! quietly drops (unbilled) what the model in use can't.
//!
//! Claude caches nothing unless asked: rein marks the end of the system
//! prompt (the tools and instructions, the same in every request) and the
//! end of the conversation, so each request reads the one before it from
//! the cache.
//!
//! An API key goes in `x-api-key`; a gateway's token (`ANTHROPIC_AUTH_TOKEN`)
//! as `Authorization: Bearer`. There's no signing in with a Claude plan:
//! Anthropic doesn't allow other apps to use one.

use serde_json::{Value, json};

use crate::client::{Client, EFFORT, Optional};
use crate::decode::{Decoder, Translate};
use crate::sse::SseEvent;
use crate::{Api, Effort, Efforts, Error, Event, Item, ModelInfo, Opaque, Request, Result, ToolCall, Usage};

pub const API_BASE: &str = "https://api.anthropic.com/v1";

/// The API version rein speaks. It turns sampling settings away, which
/// rein never sends.
const VERSION: &str = "2026-10-21";

/// When none is given: plenty for one step, within every current model's
/// limit.
const MAX_TOKENS: u64 = 32_000;

/// The most rein asks for in one answer, whatever the model allows.
const MAX_TOKENS_CAP: u64 = 64_000;

/// The models whose refusals Anthropic can hand to another model in the
/// same request (`fallbacks`), when asked on its own API.
const FALLBACKS: [&str; 4] = ["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// What a server that speaks this format may not take: a gateway in front
/// of an older model, or another provider's copy of the API.
pub(crate) const OPTIONAL: Optional = &[
    ("fallbacks", "fallbacks", "fallbacks"),
    ("display", "display", "thinking.display"),
    ("output_config", "output_config", EFFORT),
    ("effort", "effort", EFFORT),
    ("cache_control", "cache_control", "cache_control"),
];

pub(crate) fn headers(request: Option<(&Request<'_>, &Value)>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("anthropic-version", VERSION.to_owned())];
    if request.is_some_and(|(_, body)| body.get("fallbacks").is_some()) {
        headers.push(("anthropic-beta", FALLBACK_BETA.to_owned()));
    }
    headers
}

/// The models this key can use, newest first, with their windows.
pub(crate) async fn models(client: &Client) -> Result<Vec<ModelInfo>> {
    let mut models = Vec::new();
    let mut path = "models?limit=1000".to_owned();
    loop {
        let page = client.get(&path).await?;
        models.extend(parse_models(&page));
        match page["last_id"].as_str() {
            Some(last) if page["has_more"] == true => path = format!("models?limit=1000&after_id={last}"),
            _ => return Ok(models),
        }
    }
}

fn parse_models(page: &Value) -> Vec<ModelInfo> {
    let models = page["data"].as_array().into_iter().flatten();
    models
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            Some(ModelInfo {
                id: id.to_owned(),
                name: m["display_name"].as_str().unwrap_or(id).to_owned(),
                context_window: m["max_input_tokens"].as_u64(),
                max_output: m["max_tokens"].as_u64(),
                efforts: efforts(&m["capabilities"]),
            })
        })
        .collect()
}

/// A model's levels, from what its `capabilities` say it supports. The
/// older models think only with a budget, which rein sets from its own
/// levels, and not at all when told none. Claude's list doesn't say which
/// level a model uses by default.
fn efforts(capabilities: &Value) -> Efforts {
    if !capabilities.is_object() {
        return Efforts::Unknown;
    }
    let supported = |value: &Value| value["supported"] == true;
    let effort = &capabilities["effort"];
    if supported(effort) {
        let levels = [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh, Effort::Max].into_iter().filter(|level| supported(&effort[level.as_str()]));
        return Efforts::listed(levels.map(|level| (level, None)), None);
    }
    if supported(&capabilities["thinking"]["types"]["enabled"]) {
        return Efforts::listed([Effort::Off, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High].map(|level| (level, None)), Some(Effort::Off));
    }
    Efforts::NotTaken
}

/// Models from before adaptive thinking, which think only with a budget.
fn legacy(model: &str) -> bool {
    let model = model.rsplit('/').next().unwrap_or(model);
    model.starts_with("claude-3")
        || model == "claude-opus-4"
        || model == "claude-sonnet-4"
        || ["-4-5", "-4-1", "-4-0", "opus-4-2025", "sonnet-4-2025", "opus-4@", "sonnet-4@"].iter().any(|old| model.contains(old))
}

/// A thinking budget for the older models, from rein's effort levels;
/// none, for no thinking.
fn budget(effort: Effort) -> Option<u64> {
    match effort {
        Effort::Off => None,
        Effort::Minimal => Some(1024),
        Effort::Low => Some(4096),
        Effort::Medium => Some(10_000),
        Effort::High => Some(24_000),
        Effort::XHigh | Effort::Max => Some(32_000),
    }
}

/// `output_config.effort`. Claude's levels start at `low`, and the newest
/// models can't stop thinking, so less than that is `low`: only a model
/// whose list says nothing is sent one.
fn claude_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Off | Effort::Minimal | Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// A tool call's ID as Claude takes them: letters, digits, `_` and `-`,
/// 64 at most. The same ID always maps to the same one, so calls and
/// results still pair up.
fn tool_id(id: &str) -> String {
    let id: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect();
    if id.is_empty() { "call".to_owned() } else { id }
}

pub(crate) fn request_body(request: &Request<'_>, omitted: &[&str], base_url: &str) -> Value {
    let mut messages = messages(request.input);
    let cache = !omitted.contains(&"cache_control");
    if cache {
        mark_last(&mut messages);
    }
    let legacy = legacy(request.model);
    let mut max_tokens = request.max_output_tokens.map_or(MAX_TOKENS, |m| m.min(MAX_TOKENS_CAP));
    let mut body = json!({ "model": request.model, "messages": messages, "stream": true });
    if let Some(instructions) = &request.instructions {
        let mut system = json!({ "type": "text", "text": instructions });
        if cache {
            system["cache_control"] = json!({ "type": "ephemeral" });
        }
        body["system"] = json!([system]);
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request.tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters })).collect();
        body["tools"] = json!(tools);
    }
    let effort = request.reasoning_effort;
    if legacy {
        // They don't think unless given a budget, which the answer's
        // limit has to leave room for.
        if let Some(budget) = effort.and_then(budget) {
            max_tokens = max_tokens.max(budget + 8192);
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
    } else {
        // The model decides when and how much to think; effort says how
        // hard. Without `display`, the newest models think in silence.
        let mut thinking = json!({ "type": "adaptive" });
        if request.reasoning_summary && !omitted.contains(&"thinking.display") {
            thinking["display"] = json!("summarized");
        }
        body["thinking"] = thinking;
        if let Some(effort) = effort {
            body["output_config"] = json!({ "effort": claude_effort(effort) });
        }
    }
    // A refusal by mistake (security work looks risky to a classifier)
    // goes to another model instead of ending the turn.
    if base_url == API_BASE && FALLBACKS.contains(&request.model) && !omitted.contains(&"fallbacks") {
        body["fallbacks"] = json!("default");
    }
    body["max_tokens"] = json!(max_tokens);
    body
}

/// rein's items as Claude's messages: consecutive items from the same side
/// go in one message, in order.
fn messages(input: &[Item]) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    for item in input {
        let (role, block) = match item {
            Item::User(text) if !text.is_empty() => ("user", json!({ "type": "text", "text": text })),
            Item::ToolResult { call_id, output } => {
                let output = if output.is_empty() { "(no output)" } else { output };
                ("user", json!({ "type": "tool_result", "tool_use_id": tool_id(call_id), "content": output }))
            }
            Item::Assistant(text) if !text.is_empty() => ("assistant", json!({ "type": "text", "text": text })),
            Item::ToolCall(call) => {
                let input: Value = serde_json::from_str(&call.arguments).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
                ("assistant", json!({ "type": "tool_use", "id": tool_id(&call.call_id), "name": call.name, "input": input }))
            }
            // Only Claude's own: another format's reasoning means nothing here.
            Item::Opaque(opaque) if opaque.api == Api::Anthropic => ("assistant", opaque.value.clone()),
            _ => continue,
        };
        match messages.last_mut() {
            Some(last) if last["role"] == role => last["content"].as_array_mut().expect("content is a list").push(block),
            _ => messages.push(json!({ "role": role, "content": [block] })),
        }
    }
    messages
}

/// Marks the last block that can carry a cache breakpoint, so the next
/// request reads all of this one from the cache. (Thinking can't carry one.)
fn mark_last(messages: &mut [Value]) {
    let Some(content) = messages.last_mut().and_then(|m| m["content"].as_array_mut()) else { return };
    if let Some(block) = content.iter_mut().rev().find(|b| matches!(b["type"].as_str(), Some("text" | "tool_result" | "tool_use"))) {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
}

/// Reads the Messages API's stream; see [`crate::decode`].
pub type EventDecoder = Decoder<Messages>;

/// A decoder for an answer from `model`.
pub fn decoder(model: &str) -> EventDecoder {
    Decoder::new(Messages { requested: model.to_owned(), ..Messages::default() })
}

/// The Messages API's events, as rein's. Text and thinking are shown as
/// they stream; the blocks become items when the message ends, because a
/// model that declines partway can hand over to another, and what it
/// started then doesn't count.
#[derive(Debug, Default)]
pub struct Messages {
    /// The model asked for, to tell when another one answered.
    requested: String,
    blocks: Vec<Block>,
    usage: Usage,
    stop_reason: Option<String>,
    stop_details: Value,
    /// Some thinking was shown already: the next part starts apart.
    thought: bool,
}

#[derive(Debug)]
enum Block {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    Tool {
        id: String,
        name: String,
        input: String,
    },
    /// Sent back as it came: redacted thinking, and blocks rein doesn't know.
    Raw(Value),
    /// From a model that declined: not part of the answer.
    Dropped,
}

impl Translate for Messages {
    fn event(&mut self, sse: &SseEvent, out: &mut Vec<Result<Event>>) {
        let Ok(v) = serde_json::from_str::<Value>(&sse.data) else {
            if !sse.data.trim().is_empty() {
                out.push(Err(Error::Incomplete(format!("unreadable event: {}", sse.data.chars().take(200).collect::<String>()))));
            }
            return;
        };
        match v["type"].as_str().or(sse.event.as_deref()).unwrap_or_default() {
            "message_start" => {
                self.count(&v["message"]["usage"]);
                if let Some(model) = v["message"]["model"].as_str()
                    && !self.requested.is_empty()
                    && !model.starts_with(&self.requested)
                    && FALLBACKS.contains(&self.requested.as_str())
                {
                    // Anthropic keeps sending a conversation that fell back
                    // to the other model for a while.
                    out.push(Ok(Event::Note(format!("{model} answered: {} declined earlier in this conversation", self.requested))));
                }
            }
            "content_block_start" => self.start(&v["content_block"], out),
            "content_block_delta" => self.delta(v["index"].as_u64(), &v["delta"], out),
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                if !v["delta"]["stop_details"].is_null() {
                    self.stop_details = v["delta"]["stop_details"].clone();
                }
                self.count(&v["usage"]);
            }
            "message_stop" => self.stop(out),
            "error" => {
                let error = &v["error"];
                let message = error["message"].as_str().unwrap_or("stream error").to_owned();
                out.push(Err(Error::api(None, error["type"].as_str().map(str::to_owned), message, None)));
            }
            // `ping`, `content_block_stop`, and kinds added later.
            _ => {}
        }
    }
}

impl Messages {
    fn start(&mut self, block: &Value, out: &mut Vec<Result<Event>>) {
        let block = match block["type"].as_str().unwrap_or_default() {
            "text" => {
                let text = block["text"].as_str().unwrap_or_default().to_owned();
                if !text.is_empty() {
                    out.push(Ok(Event::TextDelta(text.clone())));
                }
                Block::Text(text)
            }
            "thinking" => {
                if std::mem::take(&mut self.thought) {
                    out.push(Ok(Event::Thought("\n\n".into())));
                }
                let str = |key: &str| block[key].as_str().unwrap_or_default().to_owned();
                Block::Thinking { thinking: str("thinking"), signature: str("signature") }
            }
            "tool_use" => {
                Block::Tool { id: block["id"].as_str().unwrap_or_default().to_owned(), name: block["name"].as_str().unwrap_or_default().to_owned(), input: String::new() }
            }
            // Another model takes over from here: what the one that
            // declined thought or started calling isn't part of the answer.
            "fallback" => {
                for earlier in &mut self.blocks {
                    if !matches!(earlier, Block::Text(_)) {
                        *earlier = Block::Dropped;
                    }
                }
                let model = |side: &str| block[side]["model"].as_str().unwrap_or("the model").to_owned();
                out.push(Ok(Event::Note(format!("{} declined, so {} answered", model("from"), model("to")))));
                Block::Dropped
            }
            _ => Block::Raw(block.clone()),
        };
        self.blocks.push(block);
    }

    fn delta(&mut self, index: Option<u64>, delta: &Value, out: &mut Vec<Result<Event>>) {
        // Blocks arrive in order, so the index is the position; without
        // one, it's the latest.
        let at = index.map_or(self.blocks.len().saturating_sub(1), |i| i as usize);
        let Some(block) = self.blocks.get_mut(at) else { return };
        let piece = |key: &str| delta[key].as_str().unwrap_or_default();
        match (block, delta["type"].as_str().unwrap_or_default()) {
            (Block::Text(text), "text_delta") => {
                text.push_str(piece("text"));
                out.push(Ok(Event::TextDelta(piece("text").to_owned())));
            }
            (Block::Thinking { thinking, .. }, "thinking_delta") => {
                thinking.push_str(piece("thinking"));
                if !piece("thinking").is_empty() {
                    self.thought = true;
                    out.push(Ok(Event::Thought(piece("thinking").to_owned())));
                }
            }
            (Block::Thinking { signature, .. }, "signature_delta") => signature.push_str(piece("signature")),
            (Block::Tool { input, .. }, "input_json_delta") => input.push_str(piece("partial_json")),
            _ => {}
        }
    }

    /// Adds what a usage object says. Counts come cumulative, and not
    /// every event has every one.
    fn count(&mut self, usage: &Value) {
        let n = |key: &str| usage[key].as_u64();
        let fresh = n("input_tokens");
        let (written, read) = (n("cache_creation_input_tokens"), n("cache_read_input_tokens"));
        if fresh.is_some() || written.is_some() || read.is_some() {
            // Claude counts only what came after the last cache mark as
            // input; rein counts everything the request carried.
            self.usage.input_tokens = fresh.unwrap_or(0) + written.unwrap_or(0) + read.unwrap_or(0);
            self.usage.cached_input_tokens = read.unwrap_or(0);
        }
        if let Some(output) = n("output_tokens") {
            self.usage.output_tokens = output;
        }
        if let Some(thinking) = usage["output_tokens_details"]["thinking_tokens"].as_u64() {
            self.usage.reasoning_tokens = thinking;
        }
    }

    fn stop(&mut self, out: &mut Vec<Result<Event>>) {
        match self.stop_reason.as_deref() {
            Some("refusal") => {
                let why = self.stop_details["explanation"].as_str().unwrap_or("Claude declined to go on with this.");
                out.push(Err(Error::refusal(why)));
                return;
            }
            Some("max_tokens") => {
                out.push(Err(Error::Incomplete("the answer reached its length limit (max_tokens)".into())));
                return;
            }
            Some("model_context_window_exceeded") => {
                out.push(Err(Error::context_full("the conversation filled the model's context window")));
                return;
            }
            _ => {}
        }
        for block in std::mem::take(&mut self.blocks) {
            let item = match block {
                Block::Text(text) if !text.is_empty() => Item::Assistant(text),
                Block::Thinking { thinking, signature } => Item::Opaque(Opaque::new(Api::Anthropic, json!({ "type": "thinking", "thinking": thinking, "signature": signature }))),
                Block::Tool { id, name, input } => {
                    let arguments = if input.trim().is_empty() { "{}".to_owned() } else { input };
                    Item::ToolCall(ToolCall { call_id: id, name, arguments })
                }
                Block::Raw(value) => Item::Opaque(Opaque::new(Api::Anthropic, value)),
                Block::Text(_) | Block::Dropped => continue,
            };
            out.push(Ok(Event::Item(item)));
        }
        out.push(Ok(Event::Completed { usage: self.usage }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    /// The tool every test request offers.
    static TOOLS: std::sync::LazyLock<Vec<Tool>> =
        std::sync::LazyLock::new(|| vec![Tool { strict: true, name: "read".into(), description: "Read a file.".into(), parameters: json!({"type": "object"}) }]);

    fn request<'a>(model: &'a str, input: &'a [Item]) -> Request<'a> {
        Request {
            model,
            instructions: Some("be brief"),
            input,
            tools: &TOOLS,
            reasoning_effort: Some(Effort::High),
            reasoning_summary: true,
            session_id: Some("s"),
            max_output_tokens: None,
        }
    }

    fn thinking(signature: &str) -> Item {
        Item::Opaque(Opaque::new(Api::Anthropic, json!({ "type": "thinking", "thinking": "hm", "signature": signature })))
    }

    fn call(id: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "read".into(), arguments: r#"{"path":"a.rs"}"#.into() })
    }

    fn result(id: &str) -> Item {
        Item::ToolResult { call_id: id.into(), output: "fn main() {}".into() }
    }

    #[test]
    fn items_become_messages_that_take_turns() {
        let input = vec![
            Item::User("context".into()),
            Item::User("read a.rs and b.rs".into()),
            thinking("sig1"),
            Item::Assistant("Reading both.".into()),
            call("toolu_1"),
            call("call|weird:id"),
            result("toolu_1"),
            result("call|weird:id"),
            Item::User("and c.rs".into()),
            // OpenAI's reasoning, from before a switch: not for Claude.
            Item::Opaque(Opaque::new(Api::Responses, json!({ "type": "reasoning", "encrypted_content": "x" }))),
            Item::Assistant("Done.".into()),
        ];
        let body = request_body(&request("claude-opus-5-5", &input), &[], API_BASE);
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2, "two user items, one message");
        assert_eq!(messages[1]["content"][0], json!({ "type": "thinking", "thinking": "hm", "signature": "sig1" }), "thinking goes back first, unchanged");
        assert_eq!(messages[1]["content"][2], json!({ "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a.rs" } }));
        assert_eq!(messages[1]["content"][3]["id"], "call_weird_id");
        // Every result in one message, before what the user added.
        let results: Vec<&str> = messages[2]["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(results, ["tool_result", "tool_result", "text"]);
        assert_eq!(messages[2]["content"][1]["tool_use_id"], "call_weird_id");
        assert_eq!(messages[3]["content"], json!([{ "type": "text", "text": "Done.", "cache_control": { "type": "ephemeral" } }]));
    }

    #[test]
    fn caches_the_system_prompt_and_the_conversation_so_far() {
        let body = request_body(&request("claude-opus-5-5", &[Item::User("hi".into())]), &[], API_BASE);
        assert_eq!(body["system"], json!([{ "type": "text", "text": "be brief", "cache_control": { "type": "ephemeral" } }]));
        assert_eq!(body["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(body["tools"][0], json!({ "name": "read", "description": "Read a file.", "input_schema": { "type": "object" } }));
        // A gateway that won't take the marks gets none.
        let plain = request_body(&request("claude-opus-5-5", &[Item::User("hi".into())]), &["cache_control"], API_BASE);
        assert!(!plain.to_string().contains("cache_control"));
    }

    #[test]
    fn thinks_adaptively_with_effort_or_with_a_budget_on_older_models() {
        let body = request_body(&request("claude-opus-5-5", &[Item::User("hi".into())]), &[], API_BASE);
        assert_eq!(body["thinking"], json!({ "type": "adaptive", "display": "summarized" }));
        assert_eq!(body["output_config"], json!({ "effort": "high" }));
        assert_eq!((body["max_tokens"].as_u64(), body["stream"].as_bool()), (Some(MAX_TOKENS), Some(true)));
        assert_eq!(body.get("temperature"), None);

        let old = request_body(&request("claude-haiku-4-5", &[Item::User("hi".into())]), &[], API_BASE);
        assert_eq!(old["thinking"], json!({ "type": "enabled", "budget_tokens": 24_000 }));
        assert_eq!(old.get("output_config"), None);
        assert!(old["max_tokens"].as_u64().unwrap() > 24_000);
        let hi = [Item::User("hi".into())];
        let mut quiet = request("claude-sonnet-4-5", &hi);
        quiet.reasoning_effort = None;
        assert_eq!(request_body(&quiet, &[], API_BASE).get("thinking"), None);

        let mut big = request("claude-opus-5-5", &hi);
        big.max_output_tokens = Some(128_000);
        assert_eq!(request_body(&big, &[], API_BASE)["max_tokens"], MAX_TOKENS_CAP);
        let left_out = request_body(&request("claude-opus-5-5", &hi), &["thinking.display"], API_BASE);
        assert_eq!(left_out["thinking"], json!({ "type": "adaptive" }));
        // A level the model turned down comes as none (see `Client::turned_down`).
        let mut unsaid = request("claude-opus-5-5", &hi);
        unsaid.reasoning_effort = None;
        assert_eq!(request_body(&unsaid, &[], API_BASE).get("output_config"), None);
    }

    #[test]
    fn asks_for_fallbacks_only_on_anthropics_own_api_for_models_that_have_them() {
        let body = |model: &str, base: &str| request_body(&request(model, &[Item::User("hi".into())]), &[], base);
        assert_eq!(body("claude-opus-5-5", API_BASE)["fallbacks"], "default");
        assert_eq!(body("claude-haiku-5-5", API_BASE).get("fallbacks"), None);
        assert_eq!(body("claude-opus-5-5", "https://gateway.example/v1").get("fallbacks"), None);
        let with = body("claude-opus-5-5", API_BASE);
        let headers = headers(Some((&request("claude-opus-5-5", &[]), &with)));
        assert!(headers.contains(&("anthropic-beta", FALLBACK_BETA.to_owned())));
        assert!(headers.contains(&("anthropic-version", VERSION.to_owned())));
        let without = body("claude-haiku-5-5", API_BASE);
        assert_eq!(super::headers(Some((&request("claude-haiku-5-5", &[]), &without))).len(), 1);
    }

    #[test]
    fn knows_the_models_from_before_adaptive_thinking() {
        for old in ["claude-haiku-4-5", "claude-sonnet-4-5", "claude-opus-4-1", "claude-3-7-sonnet-latest", "claude-opus-4-5"] {
            assert!(legacy(old), "{old}");
        }
        for new in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-4-6", "claude-sonnet-4-6", "claude-fable-5-1"] {
            assert!(!legacy(new), "{new}");
        }
    }

    /// The events of a stream written as (type, data) pairs.
    fn stream(events: &[Value]) -> Vec<Result<Event>> {
        let text: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
        let mut decoder = decoder("claude-opus-5-5");
        // A byte at a time: the network may split anywhere.
        for byte in text.as_bytes() {
            decoder.push(std::slice::from_ref(byte));
        }
        let mut out = Vec::new();
        while let Some(event) = decoder.pop() {
            out.push(event);
        }
        out.extend(decoder.finish());
        out
    }

    fn start(input: u64, written: u64, read: u64) -> Value {
        json!({ "type": "message_start", "message": { "model": "claude-opus-5-5", "usage": {
            "input_tokens": input, "cache_creation_input_tokens": written, "cache_read_input_tokens": read, "output_tokens": 1 } } })
    }

    fn stop(reason: &str, output: u64) -> [Value; 2] {
        [json!({ "type": "message_delta", "delta": { "stop_reason": reason }, "usage": { "output_tokens": output } }), json!({ "type": "message_stop" })]
    }

    #[test]
    fn reads_text_thinking_and_calls() {
        let mut events = vec![
            start(10, 2000, 30000),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "**Reading**" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "EqQB" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "ping" }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "Let me look." } }),
            json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_9", "name": "read", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "{\"path\": \"a" } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": ".rs\"}" } }),
            json!({ "type": "content_block_start", "index": 3, "content_block": { "type": "redacted_thinking", "data": "abc" } }),
        ];
        events.extend(stop("tool_use", 42));
        let events: Vec<Event> = stream(&events).into_iter().map(|e| e.expect("no errors")).collect();
        assert_eq!(
            events,
            [
                Event::Thought("**Reading**".into()),
                Event::TextDelta("Let me look.".into()),
                Event::Item(Item::Opaque(Opaque::new(Api::Anthropic, json!({ "type": "thinking", "thinking": "**Reading**", "signature": "EqQB" })))),
                Event::Item(Item::Assistant("Let me look.".into())),
                Event::Item(Item::ToolCall(ToolCall { call_id: "toolu_9".into(), name: "read".into(), arguments: "{\"path\": \"a.rs\"}".into() })),
                Event::Item(Item::Opaque(Opaque::new(Api::Anthropic, json!({ "type": "redacted_thinking", "data": "abc" })))),
                Event::Completed { usage: Usage { input_tokens: 32_010, cached_input_tokens: 30_000, output_tokens: 42, reasoning_tokens: 0 } },
            ]
        );
    }

    #[test]
    fn a_model_that_declines_partway_hands_over_and_its_start_is_dropped() {
        let mut events = vec![
            start(10, 0, 0),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "a" } }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "Sure, " } }),
            json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "bash", "input": {} } }),
            json!({ "type": "content_block_start", "index": 3, "content_block": { "type": "fallback", "from": { "model": "claude-opus-5-5" }, "to": { "model": "claude-opus-5" } } }),
            json!({ "type": "content_block_start", "index": 4, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 4, "delta": { "type": "text_delta", "text": "here it is." } }),
        ];
        events.extend(stop("end_turn", 5));
        let events: Vec<Event> = stream(&events).into_iter().map(|e| e.unwrap()).collect();
        assert!(events.contains(&Event::Note("claude-opus-5-5 declined, so claude-opus-5 answered".into())));
        let items: Vec<&Item> = events.iter().filter_map(|e| if let Event::Item(item) = e { Some(item) } else { None }).collect();
        assert_eq!(items, [&Item::Assistant("Sure, ".into()), &Item::Assistant("here it is.".into())], "no thinking, no call from the model that declined");
    }

    #[test]
    fn refusals_limits_and_errors_end_the_answer() {
        let refused = [
            start(1, 0, 0),
            json!({ "type": "message_delta", "delta": { "stop_reason": "refusal", "stop_details": { "type": "refusal", "category": "cyber", "explanation": "Declined." } }, "usage": { "output_tokens": 0 } }),
            json!({ "type": "message_stop" }),
        ];
        let Err(e) = stream(&refused).pop().unwrap() else { panic!("a refusal is an error") };
        assert_eq!((e.kind(), e.to_string()), (Some(crate::ErrorKind::Refusal), "Declined.".into()));

        let [delta, end] = stop("max_tokens", 32_000);
        assert!(matches!(stream(&[start(1, 0, 0), delta, end]).pop(), Some(Err(Error::Incomplete(_)))));

        let overloaded = [start(1, 0, 0), json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } })];
        let Err(e) = stream(&overloaded).pop().unwrap() else { panic!() };
        assert_eq!(e.code(), Some("overloaded_error"));

        assert!(matches!(stream(&[start(1, 0, 0)]).pop(), Some(Err(Error::Interrupted))), "cut off before message_stop");
    }

    #[test]
    fn says_when_another_model_keeps_answering() {
        let mut sticky = vec![json!({ "type": "message_start", "message": { "model": "claude-opus-5", "usage": { "input_tokens": 1 } } })];
        sticky.extend(stop("end_turn", 1));
        let events = stream(&sticky);
        assert!(matches!(&events[0], Ok(Event::Note(note)) if note.starts_with("claude-opus-5 answered")));
    }

    #[test]
    fn reads_the_model_list() {
        let page = json!({ "data": [{ "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1_000_000, "max_tokens": 128_000 }], "has_more": false });
        let models = parse_models(&page);
        assert_eq!(
            models,
            [ModelInfo { id: "claude-opus-5-5".into(), name: "Claude Opus 5.5".into(), context_window: Some(1_000_000), max_output: Some(128_000), efforts: Efforts::Unknown }]
        );
    }

    #[test]
    fn a_models_levels_are_what_its_capabilities_support() {
        let yes = json!({ "supported": true });
        let no = json!({ "supported": false });
        // As the Models API shows them: every leaf says whether.
        let adaptive = json!({ "effort": { "supported": true, "low": yes, "medium": yes, "high": yes, "xhigh": no, "max": yes }, "thinking": { "supported": true, "types": { "adaptive": yes, "enabled": no } } });
        let levels: Vec<Effort> = efforts(&adaptive).offered().iter().map(|l| l.effort).collect();
        assert_eq!(levels, [Effort::Low, Effort::Medium, Effort::High, Effort::Max]);
        assert_eq!(efforts(&adaptive).default(), None, "Claude's list doesn't say");
        // Older models think with a budget, and not unless told to.
        let budget = json!({ "effort": { "supported": false }, "thinking": { "supported": true, "types": { "adaptive": no, "enabled": yes } } });
        assert_eq!(efforts(&budget).default(), Some(Effort::Off));
        assert_eq!(efforts(&budget).sent(Some(Effort::Max)), Some(Effort::High), "its biggest budget");
        let neither = json!({ "effort": { "supported": false }, "thinking": { "supported": false, "types": { "adaptive": no, "enabled": no } } });
        assert_eq!(efforts(&neither), Efforts::NotTaken);
        assert_eq!(efforts(&Value::Null), Efforts::Unknown, "a gateway's list without them");
    }

    #[test]
    fn off_on_a_budget_model_is_no_thinking() {
        let input = [Item::User("hi".into())];
        let mut quiet = request("claude-haiku-4-5", &input);
        quiet.reasoning_effort = Some(Effort::Off);
        let body = request_body(&quiet, &[], API_BASE);
        assert!(body.get("thinking").is_none(), "{body}");
        quiet.reasoning_effort = Some(Effort::Low);
        assert_eq!(request_body(&quiet, &[], API_BASE)["thinking"]["budget_tokens"], 4096);
    }
}
