//! Replays saved response streams through the decoder: no network, no tokens.
//!
//! The `synthetic-*` streams are hand-written in each format's documented
//! events. Record real ones with `rein --record tests/fixtures/<name>.sse "<prompt>"`.

use rein_llm::decode::{Decoder, Translate};
use rein_llm::openai::EventDecoder;
use rein_llm::serde_json;
use rein_llm::{Api, Error, Event, Item, Opaque, ToolCall, Usage};

const HELLO: &[u8] = include_bytes!("fixtures/synthetic-hello.sse");

/// Feeds `stream` in `chunk`-byte pieces, the way a network might deliver it.
fn replay(stream: &[u8], chunk: usize) -> Vec<rein_llm::Result<Event>> {
    replay_with(EventDecoder::default(), stream, chunk)
}

fn replay_with<T: Translate>(mut decoder: Decoder<T>, stream: &[u8], chunk: usize) -> Vec<rein_llm::Result<Event>> {
    let mut events = Vec::new();
    for piece in stream.chunks(chunk) {
        decoder.push(piece);
        while let Some(event) = decoder.pop() {
            events.push(event);
        }
    }
    events.extend(decoder.finish());
    while let Some(event) = decoder.pop() {
        events.push(event);
    }
    events
}

fn text_and_usage(events: Vec<rein_llm::Result<Event>>) -> (String, Option<Usage>) {
    let mut text = String::new();
    let mut usage = None;
    for event in events {
        match event.expect("no errors") {
            Event::TextDelta(delta) => text.push_str(&delta),
            Event::Item(_) | Event::Thought(_) | Event::Note(_) => {}
            Event::Completed { usage: u } => usage = Some(u),
        }
    }
    (text, usage)
}

#[test]
fn replays_a_whole_stream() {
    let (text, usage) = text_and_usage(replay(HELLO, HELLO.len()));
    assert_eq!(text, "Hello, world! 👋");
    assert_eq!(usage, Some(Usage { input_tokens: 12, cached_input_tokens: 4, output_tokens: 7, reasoning_tokens: 2 }));
}

#[test]
fn replays_a_real_recorded_stream() {
    // Recorded from gpt-6.1-sol through Sign in with ChatGPT.
    let real = include_bytes!("fixtures/real-hello.sse");
    let (text, usage) = text_and_usage(replay(real, 100));
    assert_eq!(text, "Hello, world!");
    assert_eq!(usage, Some(Usage { input_tokens: 13, cached_input_tokens: 0, output_tokens: 8, reasoning_tokens: 0 }));
}

#[test]
fn replays_a_real_tool_call() {
    // Recorded from gpt-6.1-sol through Sign in with ChatGPT, with one tool
    // in the `rein` namespace. The model answered by calling it.
    let real = include_bytes!("fixtures/real-tool-call.sse");
    let events: Vec<Event> = replay(real, 64).into_iter().map(|e| e.expect("no errors")).collect();
    let calls: Vec<&ToolCall> = events
        .iter()
        .filter_map(|e| match e {
            Event::Item(Item::ToolCall(call)) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read");
    assert_eq!(calls[0].arguments, r#"{"path":"crates/rein-cli/Cargo.toml"}"#);
    assert!(matches!(events.last(), Some(Event::Completed { .. })));
}

/// Recorded streams include the account's `safety_identifier`; fixtures are
/// public, so it has to be replaced with `user-REDACTED` before committing.
#[test]
fn fixtures_contain_no_account_identifiers() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        let prefix = "\"safety_identifier\":\"user-";
        for (i, _) in text.match_indices(prefix) {
            let value = &text[i + prefix.len()..];
            assert!(value.starts_with("REDACTED\""), "{} has an unredacted safety_identifier", path.display());
        }
    }
}

#[test]
fn any_chunking_gives_the_same_result() {
    // One byte at a time splits every line and even the emoji's four bytes.
    let whole = text_and_usage(replay(HELLO, HELLO.len()));
    for chunk in [1, 2, 3, 7, 64] {
        assert_eq!(text_and_usage(replay(HELLO, chunk)), whole, "chunk size {chunk}");
    }
}

#[test]
fn a_stream_cut_off_before_completing_is_an_error() {
    let cut = &HELLO[..HELLO.len() / 2];
    let events = replay(cut, 16);
    assert!(matches!(events.last(), Some(Err(Error::Interrupted))));
}

#[test]
fn a_failed_response_carries_its_error_code() {
    let stream = br#"event: response.failed
data: {"type":"response.failed","response":{"status":"failed","error":{"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached"}}}

"#;
    let events = replay(stream, stream.len());
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.code(), Some("subscription_sharing_usage_limit_exceeded"));
}

#[test]
fn nothing_is_decoded_after_the_response_ends() {
    let mut stream = HELLO.to_vec();
    stream.extend_from_slice(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n");
    let (text, _) = text_and_usage(replay(&stream, 32));
    assert!(!text.contains("late"));
}

#[test]
fn replays_claude_thinking_and_a_tool_call() {
    let stream = include_bytes!("fixtures/synthetic-claude-tool-call.sse");
    for chunk in [1, 5, 64, stream.len()] {
        let events: Vec<Event> = replay_with(rein_llm::anthropic::decoder("claude-opus-5-5"), stream, chunk).into_iter().map(|e| e.expect("no errors")).collect();
        let items: Vec<&Item> = events.iter().filter_map(|e| if let Event::Item(item) = e { Some(item) } else { None }).collect();
        let thinking = serde_json::json!({ "type": "thinking", "thinking": "**Finding the manifest**\nThe user wants the crate's name.", "signature": "EqQBCkgIARABGAIiQL" });
        assert_eq!(
            items,
            [
                &Item::Opaque(Opaque::new(Api::Anthropic, thinking)),
                &Item::Assistant("I'll read the manifest.".into()),
                &Item::ToolCall(ToolCall { call_id: "toolu_01A09q90qw90lq917835lq9".into(), name: "read".into(), arguments: r#"{"path": "Cargo.toml"}"#.into() }),
            ],
            "chunk size {chunk}"
        );
        assert_eq!(events.last(), Some(&Event::Completed { usage: Usage { input_tokens: 2_437, cached_input_tokens: 2_048, output_tokens: 89, reasoning_tokens: 41 } }));
    }
}
