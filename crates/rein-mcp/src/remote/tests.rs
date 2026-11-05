use super::*;
use crate::config::{Origin, Reach, Remote, Server};
use crate::{Failure, Launch, Servers, Status};
use std::collections::VecDeque;

/// What a fake server answers: a status, headers, and its body in parts,
/// as they'd come over the network.
struct Canned {
    status: u16,
    headers: Vec<(String, String)>,
    parts: Vec<String>,
}

impl Canned {
    fn json(value: Value) -> Canned {
        Canned { status: 200, headers: vec![("Content-Type".into(), "application/json".into())], parts: vec![value.to_string()] }
    }

    fn events(parts: &[&str]) -> Canned {
        Canned { status: 200, headers: vec![("Content-Type".into(), "text/event-stream".into())], parts: parts.iter().map(|part| (*part).to_owned()).collect() }
    }

    fn status(status: u16) -> Canned {
        Canned { status, headers: Vec::new(), parts: Vec::new() }
    }

    fn with(mut self, name: &str, value: &str) -> Canned {
        self.headers.push((name.into(), value.into()));
        self
    }
}

struct Parts(VecDeque<Vec<u8>>);

impl Body for Parts {
    fn chunk(&mut self) -> Fut<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.pop_front();
        Box::pin(async move { Ok(next) })
    }
}

/// A request as the fake server saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(own, _)| own.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    fn method(&self) -> &str {
        self.body.get("method").and_then(Value::as_str).unwrap_or_default()
    }
}

type Handler = dyn Fn(&Seen, usize) -> Canned + Send + Sync;

/// A server at a URL, in this process: `handler` answers each request,
/// told how many came before it.
struct Fake {
    handler: Box<Handler>,
    seen: Mutex<Vec<Seen>>,
}

impl Web for Fake {
    fn send(&self, request: Request) -> Fut<'_, Result<Response, String>> {
        let body = request.body.as_deref().map_or(Value::Null, |body| serde_json::from_str(body).unwrap());
        let seen = Seen { method: request.method, headers: request.headers, body };
        let n = {
            let mut all = self.seen.lock().unwrap();
            all.push(seen.clone());
            all.len() - 1
        };
        let canned = (self.handler)(&seen, n);
        Box::pin(async move { Ok(Response { status: canned.status, headers: canned.headers, body: Box::new(Parts(canned.parts.into_iter().map(String::into_bytes).collect())) }) })
    }
}

fn fake(handler: impl Fn(&Seen, usize) -> Canned + Send + Sync + 'static) -> Arc<Fake> {
    Arc::new(Fake { handler: Box::new(handler), seen: Mutex::default() })
}

/// The answer to `seen`, a request, with `result`.
fn result(seen: &Seen, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": seen.body["id"], "result": result })
}

fn hello(seen: &Seen) -> Value {
    result(seen, json!({ "protocolVersion": "2025-11-25", "capabilities": { "tools": {} }, "serverInfo": { "name": "fake", "version": "1" } }))
}

fn tools(seen: &Seen) -> Value {
    result(seen, json!({ "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }))
}

fn said(seen: &Seen, text: &str) -> Value {
    result(seen, json!({ "content": [{ "type": "text", "text": text }] }))
}

/// A server at `url` with `headers`, and how to reach it over `web`.
fn at(url: &str, headers: &[(&str, &str)], web: Arc<Fake>, tokens: Option<Arc<dyn Tokens>>) -> (Server, Launch) {
    let headers = headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
    let server = Server {
        name: "far".into(),
        origin: Origin::Yours,
        reach: Reach::Remote(Remote { url: url.into(), headers, wire: crate::config::Wire::Http }),
        allow: Vec::new(),
        offered: Default::default(),
    };
    let launch = Launch {
        sandbox: Arc::new(rein_sandbox::Sandbox::missing(rein_sandbox::Places::default(), rein_sandbox::Missing::NotHere)),
        root: std::env::temp_dir(),
        caches: std::env::temp_dir(),
        keys: Arc::new(crate::Keys::new([("REIN_TEST_FAR_KEY".to_owned(), "FAKE-far-key".to_owned())].into())),
        web: Some(web),
        tokens,
        by_mode: Arc::default(),
    };
    (server, launch)
}

#[tokio::test]
async fn a_server_at_a_url_is_talked_to_in_json_and_in_events() {
    let web = fake(|seen, _| match seen.method() {
        "initialize" => Canned::json(hello(seen)).with("Mcp-Session-Id", "s1"),
        "notifications/initialized" => Canned::status(202),
        // A request of the server's first, its answer split where it may be.
        "tools/list" => {
            let answer = format!("data: {}\r\n\r\n", tools(seen));
            let (one, two) = answer.split_at(10);
            Canned::events(&[": a comment\r\n", "data: {\"jsonrpc\":\"2.0\",\"id\":\"p1\",\"method\":\"ping\"}\r\n\r\n", one, two])
        }
        "tools/call" => Canned::events(&["id: e0\ndata:\n\n", &format!("event: message\ndata: {}\n\n", said(seen, "hi"))]),
        _ => Canned::status(202),
    });
    let (server, launch) = at("https://far.example/mcp", &[("X-Key", "${REIN_TEST_FAR_KEY}")], web.clone(), None);
    let servers = Servers::start(vec![server], launch);
    let listing = servers.list().await;
    assert_eq!(listing.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["mcp__far__echo"], "{:?}", listing.note);
    assert_eq!(servers.call("mcp__far__echo", "{}").await, Ok("hi".into()));
    let seen = web.seen.lock().unwrap().clone();
    assert_eq!(seen[0].header("accept"), Some("application/json, text/event-stream"));
    assert_eq!((seen[0].header("mcp-session-id"), seen[0].header("mcp-protocol-version")), (None, None), "hello comes first");
    assert!(seen[1..].iter().all(|seen| seen.header("mcp-session-id") == Some("s1") && seen.header("mcp-protocol-version") == Some("2025-11-25")), "{seen:?}");
    assert!(seen.iter().all(|seen| seen.header("x-key") == Some("FAKE-far-key") && seen.method == Method::Post), "your variable, filled in");
    let ping = seen.iter().find(|seen| seen.body["id"] == "p1").expect("the server's ping answered");
    assert_eq!(ping.body["result"], json!({}));
}

#[tokio::test]
async fn an_answer_whose_stream_ends_before_it_comes_is_picked_up_where_it_was() {
    let web = fake(|seen, _| match (seen.method, seen.method()) {
        (_, "initialize") => Canned::json(hello(seen)),
        (_, "tools/list") => Canned::json(tools(seen)),
        (Method::Post, "tools/call") => Canned::events(&["id: e1\nretry: 10\ndata:\n\n"]),
        (Method::Get, _) => {
            Canned::events(&[&format!("id: e2\ndata: {}\n\n", json!({ "jsonrpc": "2.0", "id": 3, "result": { "content": [{ "type": "text", "text": "later" }] } }))])
        }
        _ => Canned::status(202),
    });
    let (server, launch) = at("https://far.example/mcp", &[], web.clone(), None);
    let servers = Servers::start(vec![server], launch);
    servers.list().await;
    assert_eq!(servers.call("mcp__far__echo", "{}").await, Ok("later".into()));
    let seen = web.seen.lock().unwrap().clone();
    let resumed = seen.iter().find(|seen| seen.method == Method::Get).expect("picked up again");
    assert_eq!((resumed.header("last-event-id"), resumed.header("accept")), (Some("e1"), Some("text/event-stream")));
}

#[tokio::test]
async fn a_call_whose_session_ended_is_made_again_in_a_new_one() {
    let web = fake(|seen, n| match seen.method() {
        "initialize" => Canned::json(hello(seen)).with("Mcp-Session-Id", if n == 0 { "old" } else { "new" }),
        "tools/list" => Canned::json(tools(seen)),
        "tools/call" if seen.header("mcp-session-id") == Some("old") => Canned::status(404),
        "tools/call" => Canned::json(said(seen, "again")),
        _ => Canned::status(202),
    });
    let (server, launch) = at("https://far.example/mcp", &[], web.clone(), None);
    let servers = Servers::start(vec![server], launch);
    servers.list().await;
    assert_eq!(servers.call("mcp__far__echo", "{}").await, Ok("again".into()), "not told it had stopped: nothing was lost");
    let calls: Vec<Option<String>> =
        web.seen.lock().unwrap().iter().filter(|seen| seen.method() == "tools/call").map(|seen| seen.header("mcp-session-id").map(str::to_owned)).collect();
    assert_eq!(calls, [Some("old".to_owned()), Some("new".to_owned())]);
}

/// Hands out `tokens` in turn as each is turned down.
struct Signed {
    tokens: Mutex<Vec<&'static str>>,
    renewed: Mutex<Vec<String>>,
}

impl Tokens for Signed {
    fn token<'a>(&'a self, _url: &'a str) -> Fut<'a, Result<Option<String>, String>> {
        let token = self.tokens.lock().unwrap().first().map(|token| (*token).to_owned());
        Box::pin(async move { Ok(token) })
    }

    fn renew<'a>(&'a self, _url: &'a str, rejected: &'a str) -> Fut<'a, Result<Option<String>, String>> {
        self.renewed.lock().unwrap().push(rejected.to_owned());
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|token| *token != rejected);
        let next = tokens.first().map(|token| (*token).to_owned());
        Box::pin(async move { Ok(next) })
    }
}

/// A server that takes only `good` as its token, and says where to sign in.
fn signing_in(good: &'static str) -> Arc<Fake> {
    fake(move |seen, _| {
        if seen.header("authorization") != Some(&format!("Bearer {good}")) {
            return Canned::status(401).with("WWW-Authenticate", r#"Bearer resource_metadata="https://far.example/.well-known/oauth-protected-resource/mcp""#);
        }
        match seen.method() {
            "initialize" => Canned::json(hello(seen)),
            "tools/list" => Canned::json(tools(seen)),
            _ => Canned::status(202),
        }
    })
}

#[tokio::test]
async fn one_that_wants_a_sign_in_says_where_and_a_token_turned_down_is_renewed() {
    let (server, launch) = at("https://far.example/mcp", &[], signing_in("t2"), None);
    let servers = Servers::start(vec![server.clone()], launch);
    let status = servers.started("far").await;
    assert!(matches!(&status, Some(Status::Failed(Failure::SignIn(challenge))) if challenge.contains("oauth-protected-resource/mcp")), "{status:?}");

    let tokens = Arc::new(Signed { tokens: Mutex::new(vec!["t1", "t2"]), renewed: Mutex::default() });
    let web = signing_in("t2");
    let (_, launch) = at("https://far.example/mcp", &[], web.clone(), Some(tokens.clone()));
    let servers = Servers::start(vec![server], launch);
    assert!(matches!(servers.started("far").await, Some(Status::Ready(_))));
    assert_eq!(*tokens.renewed.lock().unwrap(), ["t1"], "the one turned down, renewed once");
    let sent: Vec<Option<String>> = web.seen.lock().unwrap().iter().map(|seen| seen.header("authorization").map(str::to_owned)).collect();
    assert_eq!(sent[..2], [Some("Bearer t1".to_owned()), Some("Bearer t2".to_owned())]);
}

#[tokio::test]
async fn a_key_of_yours_isnt_put_aside_for_a_sign_in() {
    let tokens = Arc::new(Signed { tokens: Mutex::new(vec!["t2"]), renewed: Mutex::default() });
    let web = signing_in("t2");
    let (server, launch) = at("https://far.example/mcp", &[("Authorization", "Bearer ${REIN_TEST_FAR_KEY}")], web.clone(), Some(tokens.clone()));
    let servers = Servers::start(vec![server], launch);
    let status = servers.started("far").await;
    assert!(matches!(&status, Some(Status::Failed(Failure::Other(why))) if why.contains("401")), "the key you gave was turned down: {status:?}");
    assert!(tokens.renewed.lock().unwrap().is_empty());
    assert_eq!(web.seen.lock().unwrap()[0].header("authorization"), Some("Bearer FAKE-far-key"), "yours, not rein's sign-in");
}

#[tokio::test]
async fn plain_http_reaches_only_this_computer() {
    for url in ["https://far.example/mcp", "http://localhost:3000/mcp", "http://127.0.0.1/mcp", "http://[::1]:8080/mcp", "HTTPS://Far.Example"] {
        assert_eq!(reachable(url), Ok(()), "{url}");
    }
    for url in ["http://far.example/mcp", "http://localhost.evil.example/mcp", "ftp://far.example", "far.example/mcp", "http://me@far.example"] {
        assert!(reachable(url).is_err(), "{url}");
    }
    let web = fake(|_, _| panic!("nothing is sent"));
    let (server, launch) = at("http://far.example/mcp", &[], web, None);
    let status = Servers::start(vec![server], launch).started("far").await;
    assert!(matches!(&status, Some(Status::Failed(Failure::Other(why))) if why.contains("plain http")), "{status:?}");
}

#[test]
fn events_are_read_as_the_standard_has_them() {
    let mut events = Events::default();
    let mut all = events.push(b": hello\r\nid: 1\r\ndata: one\r\ndata: two\r\rdata:three\nretry: 25\n\nevent: x\n").unwrap();
    all.extend(events.push(b"data: {}\n\n").unwrap());
    let seen: Vec<(String, Option<String>, Option<u64>)> = all.into_iter().map(|event| (event.data, event.id, event.retry)).collect();
    assert_eq!(seen, [("one\ntwo".to_owned(), Some("1".to_owned()), None), ("three".to_owned(), None, Some(25)), ("{}".to_owned(), None, None)]);
    let long = vec![b'a'; LONGEST + 1];
    assert!(Events::default().push(&long).is_err(), "a line that never ends doesn't fill rein's memory");
}
