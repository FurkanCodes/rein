//! OpenAI's Responses API: `POST /v1/responses`, streamed.
//!
//! An API key and a Sign in with ChatGPT access token both go in the same
//! `Authorization: Bearer` header; only who pays differs.

pub mod signin;

use serde_json::{Value, json};

use crate::client::{Client, EFFORT, HOLD_BACK, IMAGES, Optional};
use crate::decode::{Decoder, Translate};
use crate::sse::SseEvent;
use crate::{Api, Effort, Efforts, Error, Event, FindsTools, Harness, Item, Loading, ModelInfo, Opaque, Request, Result, Tool, ToolCall, Usage, found_text};

/// What the Responses API may turn away: an effort level the model doesn't
/// take, which OpenAI names as the error's `param`, and tools added partway
/// through the conversation, for what the tool search found.
pub(crate) const OPTIONAL: Optional = &[("reasoning.effort", "reasoning", EFFORT), ("additional_tools", "additional_tools", HOLD_BACK), ("image", "input_image", IMAGES)];

pub const API_BASE: &str = "https://api.openai.com/v1";

/// Sign in with ChatGPT only accepts function tools grouped in a namespace,
/// so all of rein's tools live in this one,
const NAMESPACE: &str = "rein";

/// but those the tool search found, which come in this one, where it found
/// them.
const FOUND: &str = "mcp";
const FOUND_ABOUT: &str = "Tools of the user's MCP servers that tool_search found.";

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
    let holds_back = request.holds_back();
    let images = crate::images_of(request.input);
    let custom = freeform_calls(request, request.input);
    let input: Vec<Value> = request
        .input
        .iter()
        .filter(|item| !matches!(item, Item::Opaque(o) if o.api != Api::Responses))
        .flat_map(|item| item_json(item, request, holds_back, &images, &custom))
        .collect();
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
    // Held back, a tool comes where the search found it, after the
    // conversation before, so the cache holds. The search is a function of
    // rein's: OpenAI's own (`tool_search`) isn't taken on a ChatGPT plan.
    let offered: Vec<&Tool> = if holds_back { request.tools.iter().filter(|t| t.loading != Loading::Found).collect() } else { request.all_tools().collect() };
    let shown: Vec<Value> = offered.iter().filter(|tool| tool.grammar.is_none()).map(|tool| function(tool)).collect();
    let mut tools = Vec::new();
    if !shown.is_empty() {
        tools.push(json!({ "type": "namespace", "name": NAMESPACE, "description": "rein's tools for working in the user's project.", "tools": shown }));
    }
    // A tool written in a grammar of its own goes as Codex sends it: a
    // custom tool, by itself, the model held to its grammar.
    tools.extend(offered.iter().filter_map(|tool| {
        let grammar = tool.grammar.as_ref()?;
        Some(json!({ "type": "custom", "name": tool.name, "description": tool.description, "format": { "type": "grammar", "syntax": grammar.syntax.as_str(), "definition": grammar.definition } }))
    }));
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

/// Whether the tool `name` takes text in a grammar, not JSON: its calls go
/// as custom tool calls.
fn freeform(request: &Request<'_>, name: &str) -> bool {
    request.tools.iter().any(|tool| tool.name == name && tool.grammar.is_some())
}

/// The calls in `input` to a tool that takes text in a grammar.
fn freeform_calls<'a>(request: &Request<'_>, input: &'a [Item]) -> std::collections::HashSet<&'a str> {
    input.iter().filter_map(|item| if let Item::ToolCall(call) = item { freeform(request, &call.name).then_some(call.call_id.as_str()) } else { None }).collect()
}

/// `tool` as a function the model may call.
fn function(tool: &Tool) -> Value {
    json!({ "type": "function", "name": tool.name, "description": tool.description, "parameters": tool.parameters, "strict": tool.strict })
}

/// One conversation item in the Responses API's input format. Where tools
/// are held back, what the search found is added to the model's tools
/// right after its answer (`additional_tools`), in a namespace of their own.
fn item_json(
    item: &Item,
    request: &Request<'_>,
    holds_back: bool,
    images: &std::collections::HashMap<&str, Vec<&crate::Image>>,
    custom: &std::collections::HashSet<&str>,
) -> Vec<Value> {
    let one = match item {
        Item::User(text) => json!({ "role": "user", "content": text }),
        Item::Assistant(text) => json!({ "role": "assistant", "content": text }),
        // The text the model wrote, as it wrote it.
        Item::ToolCall(call) if custom.contains(call.call_id.as_str()) => {
            let input = serde_json::from_str::<Value>(&call.arguments).ok().and_then(|args| args[crate::GRAMMAR_INPUT].as_str().map(str::to_owned)).unwrap_or_default();
            json!({ "type": "custom_tool_call", "call_id": call.call_id, "name": call.name, "input": input })
        }
        // A custom tool's output is text: an image goes in words.
        Item::ToolResult { call_id, output } if custom.contains(call_id.as_str()) => {
            let (text, _) = crate::split_images(output, images.get(call_id.as_str()).map(Vec::as_slice).unwrap_or_default(), false);
            json!({ "type": "custom_tool_call_output", "call_id": call_id, "output": text })
        }
        Item::ToolCall(call) => json!({
            "type": "function_call",
            "call_id": call.call_id,
            "namespace": if holds_back && request.held_back(&call.name).is_some() { FOUND } else { NAMESPACE },
            "name": call.name,
            "arguments": call.arguments,
        }),
        // Its images go in its output, each after what's said of it.
        Item::ToolResult { call_id, output } => {
            let (text, shown) = crate::split_images(output, images.get(call_id.as_str()).map(Vec::as_slice).unwrap_or_default(), request.images);
            if shown.is_empty() {
                json!({ "type": "function_call_output", "call_id": call_id, "output": text })
            } else {
                let mut parts = vec![json!({ "type": "input_text", "text": text })];
                for image in shown {
                    parts.push(json!({ "type": "input_text", "text": image.said() }));
                    parts.push(json!({ "type": "input_image", "image_url": image.data_url(), "detail": "high" }));
                }
                json!({ "type": "function_call_output", "call_id": call_id, "output": parts })
            }
        }
        // Said with the result it's part of.
        Item::Image { .. } => return Vec::new(),
        Item::Found { call_id, names } => {
            let said = json!({ "type": "function_call_output", "call_id": call_id, "output": found_text(names) });
            let found: Vec<Value> = names.iter().filter_map(|name| request.held_back(name)).map(function).collect();
            if !holds_back || found.is_empty() {
                return vec![said];
            }
            let added = json!({ "type": "additional_tools", "role": "developer", "tools": [{ "type": "namespace", "name": FOUND, "description": FOUND_ABOUT, "tools": found }] });
            return vec![said, added];
        }
        Item::Opaque(opaque) => opaque.value.clone(),
    };
    vec![one]
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
                    finds_tools: match m["supports_search_tool"].as_bool() {
                        Some(true) => FindsTools::Yes,
                        Some(false) => FindsTools::No,
                        None => FindsTools::Unknown,
                    },
                    sees: crate::sees_by(&m["input_modalities"]),
                    // What Codex gives it to change files with is what it
                    // was trained on.
                    harness: if m["apply_patch_tool_type"].is_string() { Harness::Codex } else { Harness::of_name(id) },
                })
            })
            .collect();
    }
    let ids = body["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str());
    ids.map(|id| ModelInfo { harness: Harness::of_name(id), ..ModelInfo::named(id) }).collect()
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
        // Text in a tool's grammar: JSON from here on, as every call is.
        "custom_tool_call" => Some(Item::ToolCall(ToolCall {
            call_id: item["call_id"].as_str()?.to_owned(),
            name: item["name"].as_str()?.to_owned(),
            arguments: json!({ crate::GRAMMAR_INPUT: item["input"].as_str().unwrap_or_default() }).to_string(),
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
            hold_back: false,
            images: false,
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
        let tools = [Tool {
            grammar: None,
            loading: crate::Loading::UpFront,
            strict: true,
            name: "read".into(),
            description: "Read a file.".into(),
            parameters: json!({"type": "object"}),
        }];
        let input = [
            Item::User("open a.rs".into()),
            Item::Opaque(Opaque::new(Api::Responses, json!({"type": "reasoning", "encrypted_content": "xyz"}))),
            // Claude's thinking means nothing to OpenAI: it stays out.
            Item::Opaque(Opaque::new(Api::Anthropic, json!({"type": "thinking", "thinking": "", "signature": "s"}))),
            Item::ToolCall(call.clone()),
            Item::ToolResult { call_id: "call_1".into(), output: "fn main() {}".into() },
        ];
        let request = Request {
            model: "m",
            instructions: None,
            input: &input,
            tools: &tools,
            reasoning_effort: None,
            reasoning_summary: false,
            session_id: None,
            max_output_tokens: None,
            hold_back: false,
            images: false,
        };
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
    fn what_the_search_found_is_added_where_it_found_it() {
        use crate::testing::*;
        let (tools, input) = (held_back_tools(), found_and_called());
        let body = request_body(&request(&input, &tools, true));
        // Up front: rein's own, and the search, which rein runs: a function of its own.
        let names: Vec<&Value> = body["tools"][0]["tools"].as_array().unwrap().iter().map(|t| &t["name"]).collect();
        assert_eq!(names, [&json!("read"), &json!("tool_search")]);
        assert_eq!(body["tools"].as_array().unwrap().len(), 1, "no tool of OpenAI's: a ChatGPT plan turns them away");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2], json!({"type": "function_call_output", "call_id": "s1", "output": "Found: mcp__linear__create_issue. Call them as any other tool."}));
        let found =
            json!({"type": "function", "name": "mcp__linear__create_issue", "description": "mcp__linear__create_issue, said.", "parameters": {"type": "object"}, "strict": false});
        assert_eq!(
            body["input"][3],
            json!({"type": "additional_tools", "role": "developer", "tools": [{"type": "namespace", "name": "mcp", "description": FOUND_ABOUT, "tools": [found]}]})
        );
        assert_eq!(body["input"][4]["namespace"], "mcp", "called where it was added");
        assert_eq!(body["input"][5], json!({"type": "function_call_output", "call_id": "c1", "output": "made"}));
        assert_eq!(body["input"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn an_image_goes_in_its_tools_output_after_whats_said_of_it() {
        use crate::testing::*;
        let input = pictured();
        let body = request_body(&request(&input, &[], false));
        assert_eq!(
            body["input"][2]["output"],
            json!([
                {"type": "input_text", "text": "Image a.png, 2×1."},
                {"type": "input_text", "text": "[image a.png, 2×1]"},
                {"type": "input_image", "image_url": "data:image/png;base64,cG5nIQ==", "detail": "high"},
            ])
        );
        assert_eq!(body["input"].as_array().unwrap().len(), 4, "the image is said with its result, not as an item of its own");
        let blind = request_body(&Request { images: false, ..request(&input, &[], false) });
        assert_eq!(blind["input"][2]["output"], "Image a.png, 2×1.\n[image a.png, 2×1: this model can't see images, so it isn't shown]");
    }

    #[test]
    fn a_chatgpt_models_list_says_whether_it_sees() {
        let chatgpt = json!({"models": [{"slug": "a", "input_modalities": ["text", "image"]}, {"slug": "b", "input_modalities": ["text"]}, {"slug": "c"}]});
        let sees: Vec<crate::Sees> = parse_models(&chatgpt).iter().map(|m| m.sees).collect();
        assert_eq!(sees, [crate::Sees::Yes, crate::Sees::No, crate::Sees::Unknown]);
    }

    #[test]
    fn held_back_by_nobody_every_tool_is_shown_and_the_search_said_in_words() {
        use crate::testing::*;
        let (tools, input) = (held_back_tools(), found_and_called());
        let body = request_body(&request(&input, &tools, false));
        let names: Vec<&Value> = body["tools"][0]["tools"].as_array().unwrap().iter().map(|t| &t["name"]).collect();
        assert_eq!(names, [&json!("read"), &json!("mcp__linear__create_issue")], "no search: nothing to find");
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2], json!({"type": "function_call_output", "call_id": "s1", "output": "Found: mcp__linear__create_issue. Call them as any other tool."}));
        assert_eq!(body["input"][3]["namespace"], "rein");
    }

    #[test]
    fn a_chatgpt_models_list_says_whether_it_finds_tools() {
        let chatgpt = json!({"models": [{"slug": "new", "supports_search_tool": true}, {"slug": "old", "supports_search_tool": false}, {"slug": "unsaid"}]});
        let finds: Vec<FindsTools> = parse_models(&chatgpt).iter().map(|m| m.finds_tools).collect();
        assert_eq!(finds, [FindsTools::Yes, FindsTools::No, FindsTools::Unknown]);
    }

    #[test]
    fn a_model_gets_codexs_tools_where_codex_gives_them_to_it() {
        let chatgpt = json!({"models": [{"slug": "gpt-6.1-sol", "apply_patch_tool_type": "freeform"}, {"slug": "plain"}, {"slug": "gpt-5.5"}]});
        let given: Vec<Harness> = parse_models(&chatgpt).iter().map(|m| m.harness).collect();
        assert_eq!(given, [Harness::Codex, Harness::Rein, Harness::Codex], "the list's word, else the name's");
        let keyed = json!({"data": [{"id": "gpt-6.1-sol"}, {"id": "gpt-4.1"}, {"id": "o3"}, {"id": "codex-mini-latest"}, {"id": "gpt-5"}]});
        let given: Vec<Harness> = parse_models(&keyed).iter().map(|m| m.harness).collect();
        assert_eq!(given, [Harness::Codex, Harness::Rein, Harness::Rein, Harness::Codex, Harness::Codex]);
    }

    #[test]
    fn a_tool_in_a_grammar_goes_as_codex_sends_it_and_its_calls_come_back_as_json() {
        let patch = Tool {
            grammar: Some(crate::Grammar { syntax: crate::Syntax::Lark, definition: "start: x".into() }),
            loading: crate::Loading::UpFront,
            strict: true,
            name: "apply_patch".into(),
            description: "Patch files.".into(),
            parameters: json!({"type": "object"}),
        };
        let read = Tool { grammar: None, loading: crate::Loading::UpFront, strict: true, name: "read".into(), description: "Read".into(), parameters: json!({"type": "object"}) };
        let tools = [read, patch];
        let call = decode_item(&json!({"type": "custom_tool_call", "call_id": "p1", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch"})).unwrap();
        let Item::ToolCall(called) = &call else { panic!("{call:?}") };
        assert_eq!(serde_json::from_str::<Value>(&called.arguments).unwrap(), json!({"input": "*** Begin Patch\n*** End Patch"}), "JSON inside rein");
        let input = [Item::User("fix it".into()), call.clone(), Item::ToolResult { call_id: "p1".into(), output: "Success.".into() }];
        let body = request_body(&crate::testing::request(&input, &tools, false));
        assert_eq!(body["tools"][0]["tools"].as_array().unwrap().len(), 1, "the namespace holds the functions");
        assert_eq!(
            body["tools"][1],
            json!({"type": "custom", "name": "apply_patch", "description": "Patch files.", "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}})
        );
        assert_eq!(
            body["input"][1],
            json!({"type": "custom_tool_call", "call_id": "p1", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch"}),
            "the text as the model wrote it"
        );
        assert_eq!(body["input"][2], json!({"type": "custom_tool_call_output", "call_id": "p1", "output": "Success."}));
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
        assert_eq!(
            parse_models(&chatgpt),
            [ModelInfo {
                harness: crate::Harness::Rein,
                id: "a".into(),
                name: "Model A".into(),
                context_window: Some(500_000),
                max_output: None,
                efforts: Efforts::Unknown,
                finds_tools: FindsTools::Unknown,
                sees: crate::Sees::Unknown
            }]
        );
        let api_key = json!({"data": [{"id": "b"}]});
        assert_eq!(
            parse_models(&api_key),
            [ModelInfo {
                harness: crate::Harness::Rein,
                id: "b".into(),
                name: "b".into(),
                context_window: None,
                max_output: None,
                efforts: Efforts::Unknown,
                finds_tools: FindsTools::Unknown,
                sees: crate::Sees::Unknown
            }]
        );
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
            hold_back: false,
            images: false,
        };
        assert_eq!(request_body(&request)["reasoning"]["effort"], "none");
    }
}
