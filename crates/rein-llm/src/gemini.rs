//! Gemini's own API: `POST /models/{model}:streamGenerateContent?alt=sse`.
//!
//! Gemini speaks Chat Completions too, but only its own API carries its
//! thought signatures: opaque strings on the parts of an answer, which the
//! next request has to send back on the same parts, or the model loses the
//! thread of what it was doing (and turns a function call without one
//! away). rein keeps each as an item just before the part it came on.
//!
//! A conversation's turns are `contents`: the user's (what you said, and
//! what tools gave back) and the model's (what it said and called). The key
//! goes in `x-goog-api-key`.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::client::{Client, Optional};
use crate::decode::{Decoder, Translate};
use crate::sse::SseEvent;
use crate::{Api, Error, Event, Item, ModelInfo, Opaque, Request, Result, ToolCall, Usage};

pub const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// What Gemini takes, in place of a signature, on a function call it didn't
/// make: one from another provider, or one rein made for you with `@`.
const UNSIGNED: &str = "skip_thought_signature_validator";

pub(crate) const OPTIONAL: Optional = &[
    ("thinkinglevel", "thinkingLevel", "level"),
    ("thinking_level", "thinkingLevel", "level"),
    ("thinking level", "thinkingLevel", "level"),
    ("includethoughts", "includeThoughts", "thoughts"),
    ("include_thoughts", "includeThoughts", "thoughts"),
];

pub(crate) fn endpoint(base_url: &str, model: &str) -> String {
    format!("{base_url}/models/{model}:streamGenerateContent?alt=sse")
}

/// The models this key can use for chat, with their windows.
pub(crate) async fn models(client: &Client) -> Result<Vec<ModelInfo>> {
    let mut models = Vec::new();
    let mut path = "models?pageSize=1000".to_owned();
    loop {
        let page = client.get(&path).await?;
        models.extend(parse_models(&page));
        match page["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
            Some(token) => path = format!("models?pageSize=1000&pageToken={token}"),
            None => return Ok(models),
        }
    }
}

fn parse_models(page: &Value) -> Vec<ModelInfo> {
    let models = page["models"].as_array().into_iter().flatten();
    models
        .filter(|m| m["supportedGenerationMethods"].as_array().is_none_or(|methods| methods.iter().any(|x| x == "generateContent")))
        .filter_map(|m| {
            let id = m["name"].as_str()?.trim_start_matches("models/");
            Some(ModelInfo {
                id: id.to_owned(),
                name: m["displayName"].as_str().unwrap_or(id).to_owned(),
                context_window: m["inputTokenLimit"].as_u64(),
                max_output: m["outputTokenLimit"].as_u64(),
            })
        })
        .collect()
}

pub(crate) fn request_body(request: &Request, omitted: &[&str]) -> Value {
    let mut body = json!({ "contents": contents(&request.input) });
    if let Some(instructions) = &request.instructions {
        body["systemInstruction"] = json!({ "parts": [{ "text": instructions }] });
    }
    if !request.tools.is_empty() {
        let declarations: Vec<Value> =
            request.tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "parametersJsonSchema": t.parameters })).collect();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    let mut thinking = json!({});
    if request.reasoning_summary && !omitted.contains(&"thoughts") {
        thinking["includeThoughts"] = json!(true);
    }
    if let Some(effort) = &request.reasoning_effort
        && !omitted.contains(&"level")
    {
        let level = match effort.as_str() {
            "minimal" => "MINIMAL",
            "low" => "LOW",
            "medium" => "MEDIUM",
            _ => "HIGH",
        };
        thinking["thinkingLevel"] = json!(level);
    }
    if thinking.as_object().is_some_and(|t| !t.is_empty()) {
        body["generationConfig"] = json!({ "thinkingConfig": thinking });
    }
    body
}

/// rein's items as Gemini's turns. A signature goes on the part after it;
/// a function call with none, when its turn has no signed call, gets the
/// value that says so.
fn contents(input: &[Item]) -> Vec<Value> {
    let names: HashMap<&str, &str> = input
        .iter()
        .filter_map(|item| match item {
            Item::ToolCall(call) => Some((call.call_id.as_str(), call.name.as_str())),
            _ => None,
        })
        .collect();
    let mut contents: Vec<Value> = Vec::new();
    let mut signature: Option<String> = None;
    for item in input {
        let (role, mut part) = match item {
            Item::User(text) if !text.is_empty() => ("user", json!({ "text": text })),
            Item::ToolResult { call_id, output } => {
                let name = names.get(call_id.as_str()).copied().unwrap_or("tool");
                ("user", json!({ "functionResponse": { "id": call_id, "name": name, "response": { "output": output } } }))
            }
            Item::Assistant(text) => ("model", json!({ "text": text })),
            Item::ToolCall(call) => {
                let args: Value = serde_json::from_str(&call.arguments).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
                ("model", json!({ "functionCall": { "id": call.call_id, "name": call.name, "args": args } }))
            }
            // A signed thought goes back whole, signature and all.
            Item::Opaque(opaque) if opaque.api == Api::Gemini && opaque.value["thought"] == true => ("model", opaque.value.clone()),
            Item::Opaque(opaque) if opaque.api == Api::Gemini => {
                if let Some(sig) = opaque.value["thoughtSignature"].as_str() {
                    signature = Some(sig.to_owned());
                }
                continue;
            }
            _ => continue,
        };
        if role == "model"
            && part.get("thoughtSignature").is_none()
            && let Some(sig) = signature.take()
        {
            part["thoughtSignature"] = json!(sig);
        }
        match contents.last_mut() {
            Some(last) if last["role"] == role => last["parts"].as_array_mut().expect("parts is a list").push(part),
            _ => contents.push(json!({ "role": role, "parts": [part] })),
        }
    }
    // A signature that came on nothing rein keeps (an empty part) goes back
    // the way it came.
    if let Some(sig) = signature
        && let Some(last) = contents.last_mut().filter(|c| c["role"] == "model")
    {
        last["parts"].as_array_mut().expect("parts is a list").push(json!({ "text": "", "thoughtSignature": sig }));
    }
    for content in contents.iter_mut().filter(|c| c["role"] == "model") {
        let parts = content["parts"].as_array_mut().expect("parts is a list");
        let calls = || parts.iter().filter(|p| p.get("functionCall").is_some());
        if calls().next().is_some() && calls().all(|p| p.get("thoughtSignature").is_none()) {
            let first = parts.iter_mut().find(|p| p.get("functionCall").is_some()).expect("found above");
            first["thoughtSignature"] = json!(UNSIGNED);
        }
    }
    contents
}

/// Reads Gemini's stream; see [`crate::decode`].
pub type EventDecoder = Decoder<Generate>;

pub fn decoder() -> EventDecoder {
    Decoder::new(Generate::default())
}

/// Gemini's chunks, as rein's events. Each chunk carries a few parts;
/// text shows as it comes, and the answer ends when the stream does.
#[derive(Debug, Default)]
pub struct Generate {
    parts: Vec<Kept>,
    usage: Usage,
    finish: Option<String>,
}

/// One part of the answer that goes into the conversation, and the
/// signature it came with.
#[derive(Debug)]
enum Kept {
    /// A summary of the thinking: it goes back only when it came signed,
    /// since the signature has to go back on the part it came on.
    Thought(String, Option<String>),
    Text(String, Option<String>),
    Call(ToolCall, Option<String>),
    /// A signature on a part with nothing else in it.
    Signature(String),
}

impl Translate for Generate {
    fn event(&mut self, sse: &SseEvent, out: &mut Vec<Result<Event>>) {
        if sse.data.trim().is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(&sse.data) {
            Ok(v) => v,
            Err(e) => return out.push(Err(e.into())),
        };
        if v["error"].is_object() {
            let error = &v["error"];
            let message = error["message"].as_str().unwrap_or("stream error").to_owned();
            return out.push(Err(Error::api(None, error["status"].as_str().map(str::to_owned), message, None)));
        }
        if let Some(reason) = v["promptFeedback"]["blockReason"].as_str() {
            return out.push(Err(Error::api(None, Some("refusal".into()), format!("Gemini wouldn't take this ({reason})"), None)));
        }
        let candidate = &v["candidates"][0];
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            self.part(part, out);
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish = Some(reason.to_owned());
        }
        if v["usageMetadata"].is_object() {
            self.count(&v["usageMetadata"]);
        }
    }

    /// Gemini ends an answer by closing the stream.
    fn closed(&mut self, out: &mut Vec<Result<Event>>) {
        if self.finish.is_some() { self.end(out) } else { out.push(Err(Error::Interrupted)) }
    }
}

impl Generate {
    fn part(&mut self, part: &Value, out: &mut Vec<Result<Event>>) {
        let signature = part["thoughtSignature"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let text = part["text"].as_str().unwrap_or_default();
        if part["thought"] == true {
            if !text.is_empty() {
                out.push(Ok(Event::Thought(text.to_owned())));
            }
            match self.parts.last_mut() {
                Some(Kept::Thought(kept, kept_signature)) => {
                    kept.push_str(text);
                    if kept_signature.is_none() {
                        *kept_signature = signature;
                    }
                }
                _ => self.parts.push(Kept::Thought(text.to_owned(), signature)),
            }
            return;
        }
        if let Some(call) = part.get("functionCall") {
            let call_id = call["id"].as_str().filter(|id| !id.is_empty()).map_or_else(crate::new_call_id, str::to_owned);
            let name = call["name"].as_str().unwrap_or_default().to_owned();
            let arguments = if call["args"].is_object() { call["args"].to_string() } else { "{}".to_owned() };
            self.parts.push(Kept::Call(ToolCall { call_id, name, arguments }, signature));
            return;
        }
        if !text.is_empty() {
            out.push(Ok(Event::TextDelta(text.to_owned())));
            if let Some(Kept::Text(kept, kept_signature)) = self.parts.last_mut() {
                kept.push_str(text);
                if kept_signature.is_none() {
                    *kept_signature = signature;
                }
            } else {
                self.parts.push(Kept::Text(text.to_owned(), signature));
            }
        } else if let Some(signature) = signature {
            match self.parts.last_mut() {
                Some(Kept::Text(_, kept)) if kept.is_none() => *kept = Some(signature),
                _ => self.parts.push(Kept::Signature(signature)),
            }
        }
    }

    fn count(&mut self, usage: &Value) {
        let n = |key: &str| usage[key].as_u64().unwrap_or(0);
        let thoughts = n("thoughtsTokenCount");
        self.usage = Usage {
            input_tokens: n("promptTokenCount") + n("toolUsePromptTokenCount"),
            cached_input_tokens: n("cachedContentTokenCount"),
            output_tokens: n("candidatesTokenCount") + thoughts,
            reasoning_tokens: thoughts,
        };
    }

    fn end(&mut self, out: &mut Vec<Result<Event>>) {
        match self.finish.as_deref() {
            Some("MAX_TOKENS") => return out.push(Err(Error::Incomplete("the answer reached its length limit".into()))),
            Some(reason @ ("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII")) => {
                return out.push(Err(Error::api(None, Some("refusal".into()), format!("Gemini stopped the answer ({reason})"), None)));
            }
            Some(reason @ ("MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "MISSING_THOUGHT_SIGNATURE" | "MALFORMED_RESPONSE")) => {
                return out.push(Err(Error::Incomplete(format!("Gemini stopped: {reason}"))));
            }
            _ => {}
        }
        let signed = |signature: Option<String>, out: &mut Vec<Result<Event>>| {
            if let Some(signature) = signature {
                out.push(Ok(Event::Item(Item::Opaque(Opaque::new(Api::Gemini, json!({ "thoughtSignature": signature }))))));
            }
        };
        for part in std::mem::take(&mut self.parts) {
            match part {
                Kept::Thought(text, Some(signature)) => {
                    let thought = json!({ "text": text, "thought": true, "thoughtSignature": signature });
                    out.push(Ok(Event::Item(Item::Opaque(Opaque::new(Api::Gemini, thought)))));
                }
                Kept::Thought(_, None) => {}
                Kept::Text(text, signature) => {
                    signed(signature, out);
                    out.push(Ok(Event::Item(Item::Assistant(text))));
                }
                Kept::Call(call, signature) => {
                    signed(signature, out);
                    out.push(Ok(Event::Item(Item::ToolCall(call))));
                }
                Kept::Signature(signature) => signed(Some(signature), out),
            }
        }
        out.push(Ok(Event::Completed { usage: self.usage }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    fn request(input: Vec<Item>) -> Request {
        Request {
            model: "gemini-3.8-flash".into(),
            instructions: Some("be brief".into()),
            input,
            tools: vec![Tool { name: "read".into(), description: "Read a file.".into(), parameters: json!({"type": "object"}) }],
            reasoning_effort: Some("xhigh".into()),
            reasoning_summary: true,
            session_id: None,
            max_output_tokens: None,
        }
    }

    fn signature(sig: &str) -> Item {
        Item::Opaque(Opaque::new(Api::Gemini, json!({ "thoughtSignature": sig })))
    }

    fn call(id: &str) -> Item {
        Item::ToolCall(ToolCall { call_id: id.into(), name: "read".into(), arguments: r#"{"path":"a.rs"}"#.into() })
    }

    fn result(id: &str) -> Item {
        Item::ToolResult { call_id: id.into(), output: "fn main() {}".into() }
    }

    #[test]
    fn items_become_turns_with_their_signatures() {
        let input = vec![
            Item::User("read a.rs and b.rs".into()),
            signature("s1"),
            call("c1"),
            call("c2"),
            result("c1"),
            result("c2"),
            Item::Assistant("Both read.".into()),
            signature("s2"),
        ];
        let body = request_body(&request(input), &[]);
        assert_eq!(body["systemInstruction"], json!({ "parts": [{ "text": "be brief" }] }));
        assert_eq!(body["generationConfig"], json!({ "thinkingConfig": { "includeThoughts": true, "thinkingLevel": "HIGH" } }));
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"], json!({ "type": "object" }));
        let contents = body["contents"].as_array().unwrap();
        let roles: Vec<&str> = contents.iter().map(|c| c["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "model", "user", "model"]);
        assert_eq!(contents[1]["parts"][0], json!({ "functionCall": { "id": "c1", "name": "read", "args": { "path": "a.rs" } }, "thoughtSignature": "s1" }));
        assert_eq!(contents[1]["parts"][1].get("thoughtSignature"), None, "only the first of calls made together is signed");
        assert_eq!(contents[2]["parts"][1], json!({ "functionResponse": { "id": "c2", "name": "read", "response": { "output": "fn main() {}" } } }));
        assert_eq!(contents[3]["parts"], json!([{ "text": "Both read." }, { "text": "", "thoughtSignature": "s2" }]), "a signature on an empty part goes back that way");
    }

    #[test]
    fn calls_gemini_didnt_make_say_so() {
        // A call rein made for `@`, and one from another provider's turn.
        let input = vec![Item::User("look at @a.rs".into()), call("mention_1"), result("mention_1"), Item::Assistant("ok".into())];
        let contents = request_body(&request(input), &[])["contents"].clone();
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], UNSIGNED);
        assert_eq!(contents[3]["parts"][0].get("thoughtSignature"), None, "text needs none");
        let claude = Item::Opaque(Opaque::new(Api::Anthropic, json!({ "type": "thinking", "signature": "x" })));
        let contents = request_body(&request(vec![Item::User("hi".into()), claude, Item::Assistant("hello".into())]), &[])["contents"].clone();
        assert_eq!(contents[1]["parts"], json!([{ "text": "hello" }]), "another provider's thinking stays out");
    }

    #[test]
    fn leaves_out_what_the_model_doesnt_take() {
        let body = request_body(&request(vec![Item::User("hi".into())]), &["level"]);
        assert_eq!(body["generationConfig"]["thinkingConfig"], json!({ "includeThoughts": true }));
        let body = request_body(&request(vec![Item::User("hi".into())]), &["level", "thoughts"]);
        assert_eq!(body.get("generationConfig"), None);
    }

    fn stream(chunks: &[Value], closed: bool) -> Vec<Result<Event>> {
        let text: String = chunks.iter().map(|c| format!("data: {c}\r\n\r\n")).collect();
        let mut decoder = decoder();
        for piece in text.as_bytes().chunks(5) {
            decoder.push(piece);
        }
        let mut out = Vec::new();
        while let Some(event) = decoder.pop() {
            out.push(event);
        }
        if closed {
            out.extend(decoder.finish());
            while let Some(event) = decoder.pop() {
                out.push(event);
            }
        }
        out
    }

    fn parts(parts: Value) -> Value {
        json!({ "candidates": [{ "content": { "role": "model", "parts": parts } }] })
    }

    #[test]
    fn reads_thoughts_text_calls_and_signatures() {
        let chunks = [
            parts(json!([{ "text": "**Planning**", "thought": true }])),
            parts(json!([{ "text": "Let me " }])),
            parts(json!([{ "text": "look.", "thoughtSignature": "sigT" }])),
            parts(json!([{ "functionCall": { "name": "read", "args": { "path": "a.rs" } }, "thoughtSignature": "sigC" }, { "functionCall": { "id": "fc_2", "name": "glob", "args": {} } }])),
            json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": "" }] }, "finishReason": "STOP" }],
                    "usageMetadata": { "promptTokenCount": 1000, "cachedContentTokenCount": 800, "candidatesTokenCount": 20, "thoughtsTokenCount": 30 } }),
        ];
        let events: Vec<Event> = stream(&chunks, true).into_iter().map(|e| e.unwrap()).collect();
        assert_eq!(events[0], Event::Thought("**Planning**".into()));
        assert_eq!(events[1..3], [Event::TextDelta("Let me ".into()), Event::TextDelta("look.".into())]);
        let items: Vec<&Item> = events.iter().filter_map(|e| if let Event::Item(item) = e { Some(item) } else { None }).collect();
        assert_eq!(items[0], &signature("sigT"));
        assert_eq!(items[1], &Item::Assistant("Let me look.".into()));
        assert_eq!(items[2], &signature("sigC"));
        assert!(matches!(items[3], Item::ToolCall(c) if c.name == "read" && c.arguments == r#"{"path":"a.rs"}"# && c.call_id.starts_with("call_")));
        assert!(matches!(items[4], Item::ToolCall(c) if c.call_id == "fc_2" && c.arguments == "{}"));
        assert_eq!(events.last(), Some(&Event::Completed { usage: Usage { input_tokens: 1000, cached_input_tokens: 800, output_tokens: 50, reasoning_tokens: 30 } }));
    }

    #[test]
    fn a_signed_thought_goes_back_whole() {
        let chunks = [
            parts(json!([{ "text": "**Plan", "thought": true }])),
            parts(json!([{ "text": "ning**", "thought": true, "thoughtSignature": "sigTh" }])),
            json!({ "candidates": [{ "content": { "parts": [{ "text": "Done." }] }, "finishReason": "STOP" }] }),
        ];
        let events: Vec<Event> = stream(&chunks, true).into_iter().map(|e| e.unwrap()).collect();
        let thought = Item::Opaque(Opaque::new(Api::Gemini, json!({ "text": "**Planning**", "thought": true, "thoughtSignature": "sigTh" })));
        let items: Vec<Item> = events.iter().filter_map(|e| if let Event::Item(item) = e { Some(item.clone()) } else { None }).collect();
        assert_eq!(items, [thought.clone(), Item::Assistant("Done.".into())]);
        let contents = request_body(&request([vec![Item::User("hi".into())], items].concat()), &[])["contents"].clone();
        assert_eq!(contents[1]["parts"], json!([{ "text": "**Planning**", "thought": true, "thoughtSignature": "sigTh" }, { "text": "Done." }]), "on the part it came on");
    }

    #[test]
    fn the_answer_ends_when_the_stream_closes_after_a_finish() {
        let unfinished = [parts(json!([{ "text": "half" }]))];
        assert!(!stream(&unfinished, false).iter().any(|e| matches!(e, Ok(Event::Completed { .. }))), "not before it closes");
        assert!(matches!(stream(&unfinished, true).last(), Some(Err(Error::Interrupted))), "closed without a finish: cut off");
        let long = [json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": "MAX_TOKENS" }] })];
        assert!(matches!(stream(&long, true).last(), Some(Err(Error::Incomplete(_)))));
        let blocked = [json!({ "promptFeedback": { "blockReason": "SAFETY" } })];
        assert!(matches!(stream(&blocked, false).last(), Some(Err(e)) if e.code() == Some("refusal")));
        let failed = [json!({ "error": { "code": 429, "message": "quota", "status": "RESOURCE_EXHAUSTED" } })];
        assert!(matches!(stream(&failed, false).last(), Some(Err(e)) if e.code() == Some("RESOURCE_EXHAUSTED")));
    }

    #[test]
    fn reads_the_model_list() {
        let page = json!({ "models": [
            { "name": "models/gemini-3.8-flash", "displayName": "Gemini 3.8 Flash", "inputTokenLimit": 1048576, "outputTokenLimit": 65536, "supportedGenerationMethods": ["generateContent", "countTokens"] },
            { "name": "models/text-embedding-005", "supportedGenerationMethods": ["embedContent"] },
        ] });
        assert_eq!(
            parse_models(&page),
            [ModelInfo { id: "gemini-3.8-flash".into(), name: "Gemini 3.8 Flash".into(), context_window: Some(1_048_576), max_output: Some(65_536) }]
        );
    }
}
