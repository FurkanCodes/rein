use super::*;
use crate::config::{Origin, Reach, Remote, Server, Wire};
use crate::remote::Fut;
use crate::{Failure, Launch, Servers, Status};
use tokio::sync::mpsc;

/// A stream's body: what the fake server pushes into it, as it comes.
struct Stream(mpsc::UnboundedReceiver<Vec<u8>>);

impl Body for Stream {
    fn chunk(&mut self) -> Fut<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move { Ok(self.0.recv().await) })
    }
}

/// An answer's body that's all there at once, or empty.
struct Done(Option<Vec<u8>>);

impl Body for Done {
    fn chunk(&mut self) -> Fut<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// A request as the fake server saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(own, _)| own.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

/// A server that speaks the HTTP with SSE: its stream says to send to
/// `endpoint`, and answers each message sent there on the stream. A POST
/// to the stream's own URL, as Streamable HTTP's, is turned away (405).
struct Old {
    endpoint: String,
    /// Only with this token, when it's `Some`.
    wants: Option<&'static str>,
    stream: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    streams: AtomicU64,
    seen: Mutex<Vec<Seen>>,
}

fn old(endpoint: &str) -> Arc<Old> {
    Arc::new(Old { endpoint: endpoint.into(), wants: None, stream: Mutex::default(), streams: AtomicU64::new(0), seen: Mutex::default() })
}

impl Old {
    /// Sends `message` on the stream, as an event.
    fn event(&self, message: &Value) {
        if let Some(stream) = locked(&self.stream).as_ref() {
            let _ = stream.send(format!("event: message\ndata: {message}\n\n").into_bytes());
        }
    }

    /// Ends the stream, as a server that's done with the session does.
    fn end(&self) {
        locked(&self.stream).take();
    }

    fn answer(&self, seen: &Seen) {
        let id = &seen.body["id"];
        let result = match seen.body["method"].as_str().unwrap_or_default() {
            "initialize" => json!({ "protocolVersion": "2024-11-05", "capabilities": { "tools": {} }, "serverInfo": { "name": "old" } }),
            "tools/list" => {
                // A request of its own first, as a server may.
                self.event(&json!({ "jsonrpc": "2.0", "id": "p1", "method": "ping" }));
                json!({ "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] })
            }
            "tools/call" => json!({ "content": [{ "type": "text", "text": "said over the stream" }] }),
            _ => return,
        };
        self.event(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
}

impl Web for Old {
    fn send(&self, request: Request) -> Fut<'_, Result<Response, String>> {
        let body = request.body.as_deref().map_or(Value::Null, |body| serde_json::from_str(body).unwrap_or_default());
        let seen = Seen { method: request.method, url: request.url.clone(), headers: request.headers, body };
        locked(&self.seen).push(seen.clone());
        let response = |status: u16, kind: &str, body: Box<dyn Body>| Response { status, headers: vec![("Content-Type".into(), kind.into())], body };
        let token = self.wants.map(|token| format!("Bearer {token}"));
        let answer = if token.is_some() && seen.header("authorization") != token.as_deref() {
            let mut turned = response(401, "application/json", Box::new(Done(None)));
            turned.headers.push(("WWW-Authenticate".into(), r#"Bearer resource_metadata="https://old.example/.well-known/oauth-protected-resource""#.into()));
            turned
        } else {
            match (seen.method, seen.url.as_str()) {
                (Method::Get, _) => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let _ = tx.send(format!(": hello\n\nevent: endpoint\ndata: {}\n\n", self.endpoint).into_bytes());
                    *locked(&self.stream) = Some(tx);
                    self.streams.fetch_add(1, Ordering::Relaxed);
                    response(200, "text/event-stream", Box::new(Stream(rx)))
                }
                (Method::Post, url) if url.contains("/messages") => {
                    self.answer(&seen);
                    response(202, "text/plain", Box::new(Done(Some(b"Accepted".to_vec()))))
                }
                (Method::Post, _) => response(405, "text/plain", Box::new(Done(None))),
            }
        };
        Box::pin(async move { Ok(answer) })
    }
}

/// A server at `url`, spoken to as `wire` says, and how to reach it.
fn at(url: &str, wire: Wire, web: Arc<Old>) -> (Server, Launch) {
    let headers = [("X-Key".to_owned(), "${REIN_TEST_OLD_KEY}".to_owned())].into();
    let server = Server { name: "old".into(), origin: Origin::Yours, reach: Reach::Remote(Remote { url: url.into(), headers, wire }), allow: Vec::new() };
    let launch = Launch {
        sandbox: Arc::new(rein_sandbox::Sandbox::missing(rein_sandbox::Places::default(), rein_sandbox::Missing::NotHere)),
        root: std::env::temp_dir(),
        caches: std::env::temp_dir(),
        keys: Arc::new(crate::Keys::new([("REIN_TEST_OLD_KEY".to_owned(), "FAKE-old-key".to_owned())].into())),
        web: Some(web),
        tokens: None,
    };
    (server, launch)
}

#[tokio::test]
async fn a_server_that_speaks_sse_is_talked_to_over_its_stream() {
    let web = old("/messages?sessionId=s1");
    let (server, launch) = at("https://old.example/sse", Wire::Sse, web.clone());
    let servers = Servers::start(vec![server], launch);
    let listing = servers.list().await;
    assert_eq!(listing.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["mcp__old__echo"], "{:?}", listing.note);
    assert_eq!(servers.call("mcp__old__echo", "{}").await, Ok("said over the stream".into()));
    let seen = locked(&web.seen).clone();
    assert_eq!((seen[0].method, seen[0].header("accept")), (Method::Get, Some("text/event-stream")), "the stream first");
    let posts: Vec<&Seen> = seen.iter().filter(|seen| seen.method == Method::Post).collect();
    assert!(posts.iter().all(|seen| seen.url == "https://old.example/messages?sessionId=s1"), "where it said, on its own server");
    assert!(seen.iter().all(|seen| seen.header("x-key") == Some("FAKE-old-key")), "your header, on the stream and each message");
    let ping = posts.iter().find(|seen| seen.body["id"] == "p1").expect("its ping answered");
    assert_eq!(ping.body["result"], json!({}));
}

#[tokio::test]
async fn one_that_turns_streamable_http_away_is_spoken_to_over_sse() {
    let web = old("https://old.example/messages?sessionId=s2");
    let (server, launch) = at("https://old.example/mcp", Wire::Http, web.clone());
    let servers = Servers::start(vec![server], launch);
    assert!(matches!(servers.started("old").await, Some(Status::Ready(_))), "{:?}", servers.statuses());
    let seen = locked(&web.seen).clone();
    assert_eq!((seen[0].method, seen[1].method), (Method::Post, Method::Get), "Streamable HTTP tried first, as the spec says");
}

#[tokio::test]
async fn where_to_send_on_another_server_is_turned_down() {
    let web = old("https://elsewhere.example/messages");
    let (server, launch) = at("https://old.example/sse", Wire::Sse, web.clone());
    let status = Servers::start(vec![server], launch).started("old").await;
    assert!(matches!(&status, Some(Status::Failed(Failure::Other(why))) if why.contains("another server")), "{status:?}");
    assert!(locked(&web.seen).iter().all(|seen| seen.method == Method::Get), "nothing sent there");
}

#[tokio::test]
async fn a_stream_that_ends_ends_the_session_and_the_next_call_starts_another() {
    let web = old("/messages?sessionId=s3");
    let (server, launch) = at("https://old.example/sse", Wire::Sse, web.clone());
    let servers = Servers::start(vec![server], launch);
    servers.list().await;
    web.end();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while servers.statuses()[0].1 != Status::Stopped {
        assert!(std::time::Instant::now() < deadline, "the session never ended: {:?}", servers.statuses());
        tokio::task::yield_now().await;
    }
    let again = servers.call("mcp__old__echo", "{}").await.unwrap();
    assert!(again.starts_with("said over the stream"), "{again}");
    assert_eq!(web.streams.load(Ordering::Relaxed), 2, "a stream of its own for the new session");
}

#[tokio::test]
async fn one_that_wants_a_sign_in_says_so_on_its_stream() {
    let web = Arc::new(Old { endpoint: "/messages".into(), wants: Some("t1"), stream: Mutex::default(), streams: AtomicU64::new(0), seen: Mutex::default() });
    let (server, launch) = at("https://old.example/sse", Wire::Sse, web);
    let status = Servers::start(vec![server], launch).started("old").await;
    assert!(matches!(&status, Some(Status::Failed(Failure::SignIn(challenge))) if challenge.contains("oauth-protected-resource")), "{status:?}");
}

#[test]
fn where_to_send_is_read_against_the_streams_url() {
    assert_eq!(resolve("https://old.example/v1/sse?x=1", "/messages?s=1").unwrap(), "https://old.example/messages?s=1");
    assert_eq!(resolve("https://old.example/v1/sse", "messages").unwrap(), "https://old.example/v1/messages");
    assert_eq!(resolve("https://old.example/sse", "https://OLD.example/m").unwrap(), "https://OLD.example/m", "letter case doesn't make it another server");
    for elsewhere in ["https://evil.example/m", "//evil.example/m", "http://old.example/m", "https://old.example:8443/m"] {
        assert!(resolve("https://old.example/sse", elsewhere).is_err(), "{elsewhere}");
    }
}
