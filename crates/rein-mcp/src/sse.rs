//! The HTTP with SSE of MCP before Streamable HTTP (spec 2024-11-05), the
//! client's side, for servers that speak only it. rein opens one stream (a
//! GET, as `text/event-stream`), whose first event, `endpoint`, says where
//! to POST each message; every answer comes back on the stream, not in the
//! POST's. The stream is read for as long as the session lasts, by a task
//! the session holds, which stops when the session is let go.
//!
//! Where to send must be on the server that was reached: headers, keys
//! and tokens go with every message, and a server that named another one
//! could send them anywhere.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::remote::{Body, Event, Events, Method, Request, Response, Tokens, Web, failed};
use crate::rpc::{self, Error};

/// How long the first event, which says where to send, may take.
const FIRST: Duration = Duration::from_secs(30);

/// How long the stream may say nothing before it's given up on: the
/// session ends, and the next call starts another.
const IDLE: Duration = Duration::from_secs(300);

/// How long a message that wants no answer may take to be taken.
const TOLD: Duration = Duration::from_secs(10);

/// Requests waiting for their answers, by ID.
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Error>>>>>;

/// What a session and the task reading its stream share.
struct Shared {
    web: Arc<dyn Web>,
    tokens: Option<Arc<dyn Tokens>>,
    /// The stream's URL, which the server's sign-in is kept by.
    url: String,
    /// Where each message goes, as the stream said.
    endpoint: String,
    headers: Vec<(String, String)>,
    /// You gave it no `Authorization` of your own: it gets rein's sign-in.
    signs_in: bool,
    waiting: Waiting,
    /// Why the stream ended, once it has.
    ended: Mutex<Option<String>>,
}

/// A session with a server that speaks the HTTP with SSE.
pub(crate) struct Legacy {
    shared: Arc<Shared>,
    next: AtomicU64,
    /// Reads the stream; stopped when this is dropped.
    _reader: tokio::task::JoinSet<()>,
}

impl Legacy {
    /// Opens the stream at `url`, sending `headers`, both filled in, and
    /// waits for where to send.
    pub(crate) async fn open(web: Arc<dyn Web>, tokens: Option<Arc<dyn Tokens>>, url: String, headers: BTreeMap<String, String>) -> Result<Legacy, Error> {
        let signs_in = !headers.keys().any(|name| name.eq_ignore_ascii_case("authorization"));
        let headers: Vec<(String, String)> = headers.into_iter().collect();
        let mut renewed = false;
        let response = loop {
            let token = token(tokens.as_ref(), signs_in, &url).await?;
            let response = send(&web, Method::Get, &url, &headers, token.as_deref(), None).await?;
            let streams = response.header("content-type").is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
            match response.status {
                401 | 403 if signs_in => {
                    let challenge = response.header("www-authenticate").unwrap_or_default().to_owned();
                    if response.status == 401
                        && !renewed
                        && let (Some(token), Some(tokens)) = (&token, &tokens)
                    {
                        renewed = true;
                        if tokens.renew(&url, token).await.map_err(Error::Unreachable)?.is_some() {
                            continue;
                        }
                    }
                    return Err(Error::Unauthorized(challenge));
                }
                200..=299 if streams => break response,
                200..=299 => return Err(Error::Unreadable("it answered the stream with something else than events".into())),
                _ => return Err(failed(response).await),
            }
        };
        let mut body = response.body;
        let mut events = Events::default();
        let first = async {
            loop {
                let Some(bytes) = body.chunk().await.map_err(Error::Unreachable)? else { return Err(Error::Stopped("its stream ended before it said where to send".into())) };
                let mut said = events.push(&bytes)?.into_iter();
                // What follows it in the same part is the stream's too.
                if let Some(endpoint) = said.by_ref().find(|event| event.kind.as_deref() == Some("endpoint")) {
                    return Ok((endpoint.data, said.collect::<Vec<Event>>()));
                }
            }
        };
        let (endpoint, early) = tokio::time::timeout(FIRST, first).await.map_err(|_| Error::TimedOut { what: "where to send".into(), after: FIRST })??;
        let endpoint = resolve(&url, endpoint.trim())?;
        let shared = Arc::new(Shared { web, tokens, url, endpoint, headers, signs_in, waiting: Waiting::default(), ended: Mutex::default() });
        let mut reader = tokio::task::JoinSet::new();
        reader.spawn(read(shared.clone(), body, events, early));
        Ok(Legacy { shared, next: AtomicU64::new(1), _reader: reader })
    }

    pub(crate) fn alive(&self) -> bool {
        locked(&self.shared.ended).is_none()
    }

    /// Sends `method` with `params`, and waits up to `timeout` for its
    /// answer on the stream. Given up on, the server is told.
    pub(crate) async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Error> {
        if let Some(why) = locked(&self.shared.ended).clone() {
            return Err(Error::Stopped(why));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        locked(&self.shared.waiting).insert(id, tx);
        let _asked = Asked { waiting: &self.shared.waiting, id };
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        let answered = async {
            self.shared.post(&body).await?;
            rx.await.map_err(|_| Error::Stopped(locked(&self.shared.ended).clone().unwrap_or_else(|| "its stream ended".into())))?
        };
        if let Ok(answer) = tokio::time::timeout(timeout, answered).await {
            return answer;
        }
        let cancel = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id, "reason": "rein stopped waiting for it" } });
        let _ = tokio::time::timeout(TOLD, self.shared.post(&cancel.to_string())).await;
        Err(Error::TimedOut { what: method.to_owned(), after: timeout })
    }

    /// Sends `method`, which wants no answer.
    pub(crate) async fn notify(&self, method: &str, params: Option<Value>) {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        let _ = tokio::time::timeout(TOLD, self.shared.post(&message.to_string())).await;
    }
}

/// A request waiting for its answer, no longer waited for once dropped.
struct Asked<'a> {
    waiting: &'a Waiting,
    id: u64,
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        locked(self.waiting).remove(&self.id);
    }
}

impl Shared {
    /// Sends `body` where the stream said to: a token turned down is
    /// renewed once, if it can be without you.
    async fn post(&self, body: &str) -> Result<(), Error> {
        let mut renewed = false;
        loop {
            let token = token(self.tokens.as_ref(), self.signs_in, &self.url).await?;
            let response = send(&self.web, Method::Post, &self.endpoint, &self.headers, token.as_deref(), Some(body)).await?;
            match response.status {
                200..=299 => return Ok(()),
                401 | 403 if self.signs_in => {
                    let challenge = response.header("www-authenticate").unwrap_or_default().to_owned();
                    if response.status == 401
                        && !renewed
                        && let (Some(token), Some(tokens)) = (&token, &self.tokens)
                    {
                        renewed = true;
                        if tokens.renew(&self.url, token).await.map_err(Error::Unreachable)?.is_some() {
                            continue;
                        }
                    }
                    return Err(Error::Unauthorized(challenge));
                }
                // The server forgot the session the stream is.
                404 => {
                    *locked(&self.ended) = Some("its session ended".into());
                    return Err(Error::SessionEnded);
                }
                _ => return Err(failed(response).await),
            }
        }
    }

    /// One event of the stream: an answer to whoever waits for it, or a
    /// request of the server's, answered; anything else is let be.
    async fn take(&self, event: Event) {
        if event.kind.as_deref().is_some_and(|kind| kind != "message") {
            return;
        }
        let Ok(message) = serde_json::from_str::<Value>(&event.data) else { return };
        if let Some(reply) = rpc::reply(&message) {
            // Its answer is the server's to take; whether it does isn't rein's.
            let _ = tokio::time::timeout(TOLD, self.post(&reply.to_string())).await;
        } else if let Some((id, answer)) = rpc::answer(&message)
            && let Some(tx) = locked(&self.waiting).remove(&id)
        {
            let _ = tx.send(answer);
        }
    }
}

/// Reads the stream until it ends, or says nothing for [`IDLE`]: then the
/// session has ended, and whoever waits is told.
async fn read(shared: Arc<Shared>, mut body: Box<dyn Body>, mut events: Events, early: Vec<Event>) {
    for event in early {
        shared.take(event).await;
    }
    let why = loop {
        match tokio::time::timeout(IDLE, body.chunk()).await {
            Err(_) => break format!("its stream said nothing for {} minutes", IDLE.as_secs() / 60),
            Ok(Err(e)) => break format!("its stream broke off: {e}"),
            Ok(Ok(None)) => break "its stream ended".to_owned(),
            Ok(Ok(Some(bytes))) => match events.push(&bytes) {
                Ok(said) => {
                    for event in said {
                        shared.take(event).await;
                    }
                }
                Err(e) => break e.to_string(),
            },
        }
    };
    *locked(&shared.ended) = Some(why);
    // Their senders dropped, they're told the stream ended.
    locked(&shared.waiting).clear();
}

/// The token to send now, if rein signs in for this server.
async fn token(tokens: Option<&Arc<dyn Tokens>>, signs_in: bool, url: &str) -> Result<Option<String>, Error> {
    match (tokens, signs_in) {
        (Some(tokens), true) => tokens.token(url).await.map_err(Error::Unreachable),
        _ => Ok(None),
    }
}

/// Sends a request with what each one carries.
async fn send(web: &Arc<dyn Web>, method: Method, url: &str, own: &[(String, String)], token: Option<&str>, body: Option<&str>) -> Result<Response, Error> {
    let mut headers: Vec<(String, String)> = vec![("Accept".into(), if body.is_some() { "application/json".into() } else { "text/event-stream".into() })];
    if body.is_some() {
        headers.push(("Content-Type".into(), "application/json".into()));
    }
    if let Some(token) = token {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    }
    headers.extend(own.iter().cloned());
    web.send(Request { method, url: url.to_owned(), headers, body: body.map(str::to_owned) }).await.map_err(Error::Unreachable)
}

/// Where the stream at `url` said to send, `endpoint`, as a whole URL: on
/// the same server, or it isn't taken.
fn resolve(url: &str, endpoint: &str) -> Result<String, Error> {
    let at = origin(url).ok_or_else(|| Error::Unreadable("its URL has no server in it".into()))?;
    let full = if endpoint.starts_with("https://") || endpoint.starts_with("http://") {
        endpoint.to_owned()
    } else if endpoint.starts_with("//") {
        format!("{}:{endpoint}", at.split_once(':').map_or("https", |(scheme, _)| scheme))
    } else if endpoint.starts_with('/') {
        format!("{at}{endpoint}")
    } else {
        // Beside the stream's own path.
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let dir = path.rsplit_once('/').map_or(path, |(dir, _)| dir);
        format!("{dir}/{endpoint}")
    };
    if origin(&full).as_deref() != Some(at.as_str()) {
        return Err(Error::Unreadable("it said to send to another server, and rein sends only to the server it reached".into()));
    }
    Ok(full)
}

/// `url`'s scheme and server, as they're compared: `https://x.example:8443`.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    (!host.is_empty()).then(|| format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
