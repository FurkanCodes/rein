//! Server-sent events (SSE), the text format streaming LLM APIs answer in:
//!
//! ```text
//! event: response.output_text.delta
//! data: {"type":"response.output_text.delta","delta":"Hel"}
//!
//! ```
//!
//! Each event is a few `field: value` lines ending in a blank line. Network
//! chunks can split that text anywhere, even inside a UTF-8 character, so the
//! parser buffers bytes until it has whole lines.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds the next chunk of the stream; returns every event it completed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            // A whole line never ends inside a UTF-8 character.
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if self.event.is_some() || !self.data.is_empty() {
                    events.push(SseEvent { event: self.event.take(), data: self.data.join("\n") });
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // a comment, often sent as a keep-alive
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (&*line, ""),
            };
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                _ => {} // `id` and `retry` don't matter for one-shot responses
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent { event: Some(name.into()), data: data.into() }
    }

    #[test]
    fn parses_events_and_skips_comments() {
        let mut p = SseParser::default();
        let events = p.push(b": keep-alive\n\nevent: a\ndata: {\"x\":1}\n\nevent: b\ndata: 2\n\n");
        assert_eq!(events, [event("a", "{\"x\":1}"), event("b", "2")]);
    }

    #[test]
    fn handles_any_chunk_boundary_and_crlf() {
        let input = "event: a\r\ndata: h\u{e9}llo\r\n\r\n".as_bytes();
        let mut p = SseParser::default();
        let events: Vec<_> = input.iter().flat_map(|b| p.push(&[*b])).collect();
        assert_eq!(events, [event("a", "h\u{e9}llo")]);
    }

    #[test]
    fn joins_multi_line_data() {
        let mut p = SseParser::default();
        assert_eq!(p.push(b"data: one\ndata: two\n\n"), [SseEvent { event: None, data: "one\ntwo".into() }]);
    }
}
