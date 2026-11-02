//! Chat Completions: `POST /chat/completions`, streamed. OpenAI's older
//! API, which most of the rest speak: OpenRouter, Groq, DeepSeek, xAI,
//! Mistral, Cerebras, Together, Fireworks, Moonshot, z.ai, and servers on
//! your own computer like Ollama, LM Studio, vLLM and llama.cpp.
//!
//! They differ in the details. What's optional in a request (asking for
//! token counts while streaming, reasoning effort, sending a model's
//! reasoning back) is left out once a server says it doesn't take it (see
//! [`crate::client`]). What can't be learned that way, like Mistral's
//! nine-character tool call IDs, the providers rein knows say in [`Quirks`].
//! Answers are read in every dialect: reasoning comes as
//! `reasoning_content` (DeepSeek, Moonshot, vLLM) or `reasoning`
//! (OpenRouter, Groq, Ollama).

use serde_json::{Map, Value, json};

use crate::client::{Client, Optional};
use crate::decode::{Decoder, Translate};
use crate::sse::SseEvent;
use crate::{Api, Effort, Error, Event, Item, ModelInfo, Opaque, Request, Result, ToolCall, Usage};

/// What a provider needs that it can't say in an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quirks {
    /// Effort goes as `"reasoning": {"effort": ...}`, as OpenRouter has
    /// it, rather than `"reasoning_effort"`.
    pub effort_object: bool,
    /// Tool call IDs are nine letters and digits, as Mistral has them.
    pub short_ids: bool,
    /// A tool's result names the tool, as Mistral wants.
    pub tool_names: bool,
}

pub(crate) const OPTIONAL: Optional = &[
    ("stream_options", "stream_options", "stream_options"),
    ("reasoning_content", "reasoning_content", "replay"),
    ("reasoning_details", "reasoning_details", "replay"),
    ("reasoning_effort", "reasoning_effort", "effort"),
    ("reasoning", "reasoning", "effort"),
];

/// The models a server offers, with their windows where it says.
pub(crate) async fn models(client: &Client) -> Result<Vec<ModelInfo>> {
    Ok(parse_models(&client.get("models").await?))
}

fn parse_models(body: &Value) -> Vec<ModelInfo> {
    let models = body["data"].as_array().or(body["models"].as_array()).into_iter().flatten();
    models
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            // Each server calls it something else.
            let window = ["context_length", "context_window", "max_context_length", "max_model_len"].iter().find_map(|key| m[*key].as_u64());
            Some(ModelInfo {
                id: id.to_owned(),
                name: m["name"].as_str().unwrap_or(id).to_owned(),
                context_window: window.or(m["top_provider"]["context_length"].as_u64()),
                max_output: m["top_provider"]["max_completion_tokens"].as_u64(),
            })
        })
        .collect()
}

/// A tool call's ID the way the server takes it.
fn tool_id(id: &str, quirks: Quirks) -> String {
    if !quirks.short_ids {
        return id.to_owned();
    }
    // The same ID always gives the same nine characters, so calls and
    // results still pair up.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    (0..9)
        .map(|_| {
            let digit = DIGITS[(hash % 62) as usize] as char;
            hash /= 62;
            digit
        })
        .collect()
}

pub(crate) fn request_body(request: &Request<'_>, omitted: &[&str], quirks: Quirks) -> Value {
    let mut body = json!({ "model": request.model, "messages": messages(request, omitted, quirks), "stream": true });
    if !omitted.contains(&"stream_options") {
        // Without it, most servers don't say what the answer cost.
        body["stream_options"] = json!({ "include_usage": true });
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> =
            request.tools.iter().map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.parameters } })).collect();
        body["tools"] = json!(tools);
    }
    if let Some(effort) = request.reasoning_effort
        && !omitted.contains(&"effort")
    {
        // The levels the format knows; beyond them, its highest.
        let effort = match effort {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High | Effort::XHigh | Effort::Max => "high",
        };
        if quirks.effort_object {
            body["reasoning"] = json!({ "effort": effort });
        } else {
            body["reasoning_effort"] = json!(effort);
        }
    }
    body
}

/// rein's items as chat messages. What the model said, the reasoning it
/// came with and the tools it called are one assistant message; each
/// tool's result is a message of its own, right after.
fn messages(request: &Request<'_>, omitted: &[&str], quirks: Quirks) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(instructions) = &request.instructions {
        messages.push(json!({ "role": "system", "content": instructions }));
    }
    let names: std::collections::HashMap<&str, &str> = request
        .input
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall(call) => Some((call.call_id.as_str(), call.name.as_str())),
            _ => None,
        })
        .collect();
    let mut turn: Option<Map<String, Value>> = None;
    let end = |turn: &mut Option<Map<String, Value>>, messages: &mut Vec<Value>| {
        if let Some(mut message) = turn.take() {
            let has_calls = message.contains_key("tool_calls");
            if message.get("content").is_none_or(Value::is_null) && !has_calls {
                return; // reasoning alone says nothing
            }
            message.entry("content").or_insert(Value::Null);
            messages.push(Value::Object(message));
        }
    };
    for item in request.input {
        match item {
            Item::User(text) => {
                end(&mut turn, &mut messages);
                messages.push(json!({ "role": "user", "content": text }));
            }
            Item::ToolResult { call_id, output } => {
                end(&mut turn, &mut messages);
                let mut result = json!({ "role": "tool", "tool_call_id": tool_id(call_id, quirks), "content": output });
                if quirks.tool_names
                    && let Some(name) = names.get(call_id.as_str())
                {
                    result["name"] = json!(name);
                }
                messages.push(result);
            }
            Item::Assistant(text) => {
                let message = turn.get_or_insert_with(assistant);
                let joined = match message.get("content").and_then(Value::as_str) {
                    Some(before) => format!("{before}\n\n{text}"),
                    None => text.clone(),
                };
                message.insert("content".into(), json!(joined));
            }
            Item::ToolCall(call) => {
                let message = turn.get_or_insert_with(assistant);
                let calls = message.entry("tool_calls").or_insert_with(|| json!([]));
                let call = json!({ "id": tool_id(&call.call_id, quirks), "type": "function", "function": { "name": call.name, "arguments": call.arguments } });
                calls.as_array_mut().expect("a list").push(call);
            }
            // Reasoning the server asked to have back: on the message it
            // came with. Only this format's own.
            Item::Opaque(opaque) if opaque.api == Api::Chat && !omitted.contains(&"replay") => {
                let message = turn.get_or_insert_with(assistant);
                // Only the fields rein keeps: the item may come from a file
                // on disk, and anything else there (a `tool_calls` that
                // isn't a list, say) mustn't reach the message.
                for field in ["reasoning_content", "reasoning_details"] {
                    if let Some(value) = opaque.value.get(field) {
                        message.insert(field.into(), value.clone());
                    }
                }
            }
            Item::Opaque(_) => {}
        }
    }
    end(&mut turn, &mut messages);
    messages
}

fn assistant() -> Map<String, Value> {
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message
}

/// Reads a Chat Completions stream; see [`crate::decode`].
pub type EventDecoder = Decoder<Completions>;

pub fn decoder() -> EventDecoder {
    Decoder::new(Completions::default())
}

/// A stream of chunks, as rein's events. Text and reasoning show as they
/// come; the calls arrive in pieces and become items at the end.
#[derive(Debug, Default)]
pub struct Completions {
    text: String,
    /// Reasoning to send back, under the name the server used for it.
    reasoning: Option<(&'static str, String)>,
    /// OpenRouter's reasoning, for models that need it back as it was.
    details: Vec<Value>,
    calls: Vec<Partial>,
    usage: Usage,
    finish: Option<String>,
}

#[derive(Debug, Default)]
struct Partial {
    index: Option<u64>,
    id: String,
    name: String,
    arguments: String,
}

impl Translate for Completions {
    fn event(&mut self, sse: &SseEvent, out: &mut Vec<Result<Event>>) {
        let data = sse.data.trim();
        if data == "[DONE]" {
            return self.end(out);
        }
        if data.is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => return out.push(Err(e.into())),
        };
        if v["error"].is_object() || v["error"].is_string() {
            let error = &v["error"];
            let message = error["message"].as_str().or(error.as_str()).unwrap_or("stream error").to_owned();
            let code = ["code", "type"].iter().find_map(|key| error[*key].as_str()).map(str::to_owned);
            return out.push(Err(Error::api(None, code, message, None)));
        }
        if let Some(choice) = v["choices"].as_array().and_then(|choices| choices.first()) {
            self.delta(&choice["delta"], out);
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish = Some(reason.to_owned());
            }
        }
        if v["usage"].is_object() {
            self.count(&v["usage"]);
        }
    }

    /// Some servers just close the stream after the last chunk.
    fn closed(&mut self, out: &mut Vec<Result<Event>>) {
        if self.finish.is_some() {
            self.end(out);
        } else {
            out.push(Err(Error::Interrupted));
        }
    }
}

impl Completions {
    fn delta(&mut self, delta: &Value, out: &mut Vec<Result<Event>>) {
        // A server may send both names: the first one says it.
        let reasoning = ["reasoning_content", "reasoning"].into_iter().find_map(|field| delta[field].as_str().filter(|p| !p.is_empty()).map(|p| (field, p)));
        if let Some((field, piece)) = reasoning {
            out.push(Ok(Event::Thought(piece.to_owned())));
            // Only `reasoning_content` is asked for back, by the servers
            // that send it.
            let (_, kept) = self.reasoning.get_or_insert((field, String::new()));
            kept.push_str(piece);
        }
        if let Some(details) = delta["reasoning_details"].as_array() {
            self.details.extend(details.iter().cloned());
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(text);
            out.push(Ok(Event::TextDelta(text.to_owned())));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64();
            let id = call["id"].as_str().filter(|id| !id.is_empty());
            // Pieces of one call share an index; a server that sends whole
            // calls may number none of them.
            let found = self.calls.iter().position(|c| match (index, id) {
                (Some(i), _) => c.index == Some(i),
                (None, Some(id)) => c.id == id,
                (None, None) => false,
            });
            let at = match found {
                Some(at) => at,
                None if index.is_none() && id.is_none() && !self.calls.is_empty() => self.calls.len() - 1,
                None => {
                    self.calls.push(Partial { index, ..Partial::default() });
                    self.calls.len() - 1
                }
            };
            let partial = &mut self.calls[at];
            if let Some(id) = id {
                partial.id = id.to_owned();
            }
            if let Some(name) = call["function"]["name"].as_str() {
                partial.name.push_str(name);
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                partial.arguments.push_str(arguments);
            } else if call["function"]["arguments"].is_object() {
                partial.arguments.push_str(&call["function"]["arguments"].to_string());
            }
        }
    }

    fn count(&mut self, usage: &Value) {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        self.usage = Usage {
            input_tokens: n(&usage["prompt_tokens"]),
            // OpenAI's name, then DeepSeek's.
            cached_input_tokens: usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(n(&usage["prompt_cache_hit_tokens"])),
            output_tokens: n(&usage["completion_tokens"]),
            reasoning_tokens: n(&usage["completion_tokens_details"]["reasoning_tokens"]),
        };
    }

    fn end(&mut self, out: &mut Vec<Result<Event>>) {
        match self.finish.as_deref() {
            Some("length") => return out.push(Err(Error::Incomplete("the answer reached its length limit".into()))),
            Some("content_filter") => return out.push(Err(Error::refusal("the provider's filter stopped the answer"))),
            _ => {}
        }
        let mut replay = Map::new();
        if let Some(("reasoning_content", text)) = self.reasoning.take() {
            replay.insert("reasoning_content".into(), json!(text));
        }
        if !self.details.is_empty() {
            replay.insert("reasoning_details".into(), json!(std::mem::take(&mut self.details)));
        }
        if !replay.is_empty() {
            out.push(Ok(Event::Item(Item::Opaque(Opaque::new(Api::Chat, Value::Object(replay))))));
        }
        if !self.text.is_empty() {
            out.push(Ok(Event::Item(Item::Assistant(std::mem::take(&mut self.text)))));
        }
        for call in std::mem::take(&mut self.calls) {
            let call_id = if call.id.is_empty() { crate::new_call_id() } else { call.id };
            let arguments = if call.arguments.trim().is_empty() { "{}".to_owned() } else { call.arguments };
            out.push(Ok(Event::Item(Item::ToolCall(ToolCall { call_id, name: call.name, arguments }))));
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
        std::sync::LazyLock::new(|| vec![Tool { name: "read".into(), description: "Read a file.".into(), parameters: json!({"type": "object"}) }]);

    fn request(input: &[Item]) -> Request<'_> {
        Request {
            model: "qwen3-coder",
            instructions: Some("be brief"),
            input,
            tools: &TOOLS,
            reasoning_effort: Some(Effort::Max),
            reasoning_summary: true,
            session_id: None,
            max_output_tokens: None,
        }
    }

    fn call(id: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "read".into(), arguments: r#"{"path":"a.rs"}"#.into() })
    }

    fn result(id: &str) -> Item {
        Item::ToolResult { call_id: id.into(), output: "fn main() {}".into() }
    }

    #[test]
    fn items_become_chat_messages() {
        let reasoning = Item::Opaque(Opaque::new(Api::Chat, json!({ "reasoning_content": "Let me look." })));
        let claude = Item::Opaque(Opaque::new(Api::Anthropic, json!({ "type": "thinking", "thinking": "", "signature": "s" })));
        let input = vec![
            Item::User("read a.rs".into()),
            claude,
            reasoning,
            Item::Assistant("Reading.".into()),
            call("c1"),
            call("c2"),
            result("c1"),
            result("c2"),
            Item::Assistant("Done.".into()),
        ];
        let body = request_body(&request(&input), &[], Quirks::default());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0], json!({ "role": "system", "content": "be brief" }));
        assert_eq!(messages[1], json!({ "role": "user", "content": "read a.rs" }));
        assert_eq!(
            messages[2],
            json!({ "role": "assistant", "content": "Reading.", "reasoning_content": "Let me look.", "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"a.rs\"}" } },
                { "id": "c2", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"a.rs\"}" } },
            ] })
        );
        assert_eq!(messages[3], json!({ "role": "tool", "tool_call_id": "c1", "content": "fn main() {}" }));
        assert_eq!(messages[5], json!({ "role": "assistant", "content": "Done." }));
        assert_eq!(messages.len(), 6, "Claude's signed thinking means nothing here");
        assert_eq!(body["stream_options"], json!({ "include_usage": true }));
        assert_eq!(body["reasoning_effort"], "high", "max is beyond the format: its highest");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn a_tampered_reasoning_item_cant_break_the_message() {
        let tampered = Item::Opaque(Opaque::new(Api::Chat, json!({ "tool_calls": {}, "role": "system", "reasoning_content": "hm" })));
        let body = request_body(&request(&[Item::User("hi".into()), tampered, call("c1"), result("c1")]), &[], Quirks::default());
        let assistant = &body["messages"][2];
        assert_eq!((assistant["role"].as_str(), assistant["reasoning_content"].as_str()), (Some("assistant"), Some("hm")));
        assert_eq!(assistant["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn leaves_out_what_the_server_turned_away() {
        let reasoning = Item::Opaque(Opaque::new(Api::Chat, json!({ "reasoning_content": "hm" })));
        let body = request_body(&request(&[Item::User("hi".into()), reasoning, Item::Assistant("a".into())]), &["stream_options", "effort", "replay"], Quirks::default());
        let text = body.to_string();
        assert!(!text.contains("stream_options") && !text.contains("reasoning"), "{text}");
        // An assistant message of reasoning alone isn't sent.
        let lone = Item::Opaque(Opaque::new(Api::Chat, json!({ "reasoning_content": "hm" })));
        let body = request_body(&request(&[Item::User("hi".into()), lone, Item::User("again".into())]), &[], Quirks::default());
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn says_it_the_way_each_provider_needs() {
        let openrouter = Quirks { effort_object: true, ..Quirks::default() };
        let body = request_body(&request(&[Item::User("hi".into())]), &[], openrouter);
        assert_eq!((body["reasoning"].clone(), body.get("reasoning_effort")), (json!({ "effort": "high" }), None));

        let mistral = Quirks { short_ids: true, tool_names: true, ..Quirks::default() };
        let body = request_body(&request(&[Item::User("hi".into()), call("toolu_01A09q90qw90lq917835lq9"), result("toolu_01A09q90qw90lq917835lq9")]), &[], mistral);
        let id = body["messages"][2]["tool_calls"][0]["id"].as_str().unwrap().to_owned();
        assert!(id.len() == 9 && id.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
        assert_eq!(body["messages"][3]["tool_call_id"], id.as_str(), "the result pairs with its call");
        assert_eq!(body["messages"][3]["name"], "read");
        assert_ne!(tool_id("call_1", mistral), tool_id("call_2", mistral));
    }

    fn stream(chunks: &[Value], done: bool) -> Vec<Result<Event>> {
        let mut text: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        if done {
            text.push_str("data: [DONE]\n\n");
        }
        let mut decoder = decoder();
        for piece in text.as_bytes().chunks(7) {
            decoder.push(piece);
        }
        let mut out = Vec::new();
        while let Some(event) = decoder.pop() {
            out.push(event);
        }
        out.extend(decoder.finish());
        // Closing can end the answer, with all that comes with that.
        while let Some(event) = decoder.pop() {
            out.push(event);
        }
        out
    }

    fn chunk(delta: Value) -> Value {
        json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": null }] })
    }

    #[test]
    fn reads_reasoning_text_calls_in_pieces_and_usage() {
        let chunks = [
            chunk(json!({ "role": "assistant", "reasoning_content": "**Looking**" })),
            chunk(json!({ "content": "Let me " })),
            chunk(json!({ "content": "look." })),
            chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_a", "type": "function", "function": { "name": "read", "arguments": "" } }] })),
            chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"path\":" } }] })),
            chunk(json!({ "tool_calls": [{ "index": 1, "id": "call_b", "function": { "name": "glob", "arguments": "{}" } }] })),
            chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "\"a.rs\"}" } }] })),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            json!({ "choices": [], "usage": { "prompt_tokens": 900, "completion_tokens": 30, "prompt_tokens_details": { "cached_tokens": 512 } } }),
        ];
        let events: Vec<Event> = stream(&chunks, true).into_iter().map(|e| e.unwrap()).collect();
        assert_eq!(
            events,
            [
                Event::Thought("**Looking**".into()),
                Event::TextDelta("Let me ".into()),
                Event::TextDelta("look.".into()),
                Event::Item(Item::Opaque(Opaque::new(Api::Chat, json!({ "reasoning_content": "**Looking**" })))),
                Event::Item(Item::Assistant("Let me look.".into())),
                Event::Item(Item::ToolCall(ToolCall { call_id: "call_a".into(), name: "read".into(), arguments: "{\"path\":\"a.rs\"}".into() })),
                Event::Item(Item::ToolCall(ToolCall { call_id: "call_b".into(), name: "glob".into(), arguments: "{}".into() })),
                Event::Completed { usage: Usage { input_tokens: 900, cached_input_tokens: 512, output_tokens: 30, reasoning_tokens: 0 } },
            ]
        );
    }

    #[test]
    fn reads_every_dialect() {
        // Ollama: reasoning as `reasoning`, a whole call with no index or
        // ID, and no [DONE] before closing.
        let chunks = [
            chunk(json!({ "reasoning": "hm" })),
            chunk(json!({ "tool_calls": [{ "function": { "name": "read", "arguments": { "path": "a.rs" } } }] })),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }], "usage": { "prompt_tokens": 10, "completion_tokens": 2, "prompt_cache_hit_tokens": 8 } }),
        ];
        let events: Vec<Event> = stream(&chunks, false).into_iter().map(|e| e.unwrap()).collect();
        let call = events.iter().find_map(|e| if let Event::Item(Item::ToolCall(c)) = e { Some(c) } else { None }).unwrap();
        assert_eq!((call.name.as_str(), call.arguments.as_str()), ("read", r#"{"path":"a.rs"}"#));
        assert!(call.call_id.starts_with("call_"), "{}", call.call_id);
        assert!(!events.iter().any(|e| matches!(e, Event::Item(Item::Opaque(_)))), "`reasoning` isn't asked for back");
        assert!(matches!(events.last(), Some(Event::Completed { usage }) if usage.cached_input_tokens == 8), "DeepSeek's cache count: {events:?}");

        let cut = [chunk(json!({ "content": "half" }))];
        assert!(matches!(stream(&cut, false).last(), Some(Err(Error::Interrupted))));
        let long = [json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "length" }] })];
        assert!(matches!(stream(&long, true).last(), Some(Err(Error::Incomplete(_)))));
        let failed = [json!({ "error": { "message": "model overloaded", "code": "overloaded" } })];
        assert!(matches!(stream(&failed, false).last(), Some(Err(e)) if e.code() == Some("overloaded")));
    }

    #[test]
    fn reads_openrouters_real_model_list() {
        // Trimmed from OpenRouter's public `GET /api/v1/models`, 2026-11-02.
        let real: Value = serde_json::from_str(include_str!("../tests/fixtures/real-openrouter-models.json")).unwrap();
        let models = parse_models(&real);
        assert_eq!(models.len(), 3);
        assert_eq!(
            models[0],
            ModelInfo { id: "recursive/flint-v1.0:free".into(), name: "Recursive: Flint (free)".into(), context_window: Some(262_144), max_output: Some(235_929) }
        );
        assert_eq!((models[2].id.as_str(), models[2].context_window, models[2].max_output), ("anthropic/claude-haiku-5.5", Some(1_000_000), Some(128_000)));
    }

    #[test]
    fn reads_every_model_list() {
        let openrouter = json!({ "data": [{ "id": "qwen/qwen3-coder", "name": "Qwen3 Coder", "context_length": 262144, "top_provider": { "max_completion_tokens": 65536 } }] });
        assert_eq!(parse_models(&openrouter)[0], ModelInfo { id: "qwen/qwen3-coder".into(), name: "Qwen3 Coder".into(), context_window: Some(262_144), max_output: Some(65_536) });
        let groq = json!({ "data": [{ "id": "llama-4", "context_window": 131072 }] });
        assert_eq!(parse_models(&groq)[0].context_window, Some(131_072));
        let ollama = json!({ "object": "list", "data": [{ "id": "qwen3:8b", "object": "model" }] });
        assert_eq!(parse_models(&ollama)[0].context_window, None);
    }
}
