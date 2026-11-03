//! JSON-RPC over a server's standard input and output, one message a line,
//! as MCP's stdio transport has it. Two tasks do the talking: one writes
//! what rein sends, one reads what the server says and hands each answer to
//! whoever asked. Both stop when this is dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The longest message rein reads: an image fits, and a server stuck
/// printing doesn't fill rein's memory.
const LONGEST_LINE: usize = 8 << 20;

/// How much of what a server last wrote to its standard error rein keeps,
/// to say why it stopped.
const KEPT_ERRORS: usize = 2_000;

/// How long rein waits for a server's standard error to end once its
/// output has: what it said there last says why it stopped.
const ERRORS_END: Duration = Duration::from_secs(1);

/// What went wrong talking to a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// It couldn't be started.
    Start(String),
    /// Its output ended: it stopped, or rein stopped reading it. What it
    /// wrote last to its standard error, if anything.
    Stopped(String),
    /// It didn't answer `what` in time.
    TimedOut { what: String, after: Duration },
    /// It answered with an error of its own.
    Refused { code: i64, message: String },
    /// It answered with something rein can't read.
    Unreadable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Start(why) => f.write_str(why),
            Error::Stopped(said) if said.is_empty() => f.write_str("it stopped"),
            Error::Stopped(said) => write!(f, "it stopped, saying: {said}"),
            Error::TimedOut { what, after } => write!(f, "it didn't answer {what} within {}s", after.as_secs()),
            Error::Refused { code, message } => write!(f, "{message} (error {code})"),
            Error::Unreadable(what) => write!(f, "it said something rein can't read: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// The answer to a request, as the reader hands it over.
type Answer = Result<Value, Error>;

/// Requests waiting for their answers, by ID.
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Answer>>>>;

pub(crate) struct Rpc {
    /// Lines for the writer to send.
    lines: mpsc::UnboundedSender<String>,
    waiting: Waiting,
    next: AtomicU64,
    /// Why its output ended, once it has.
    ended: Arc<Mutex<Option<String>>>,
    /// The end of what it wrote to its standard error.
    errors: Arc<Mutex<String>>,
    /// The reader and the writer, stopped when this is dropped.
    _tasks: tokio::task::JoinSet<()>,
}

impl Rpc {
    /// Talks to a server whose output is `reader` and input `writer`;
    /// `errors`, if there's one, is its standard error, kept the end of.
    pub(crate) fn start(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        errors: Option<impl AsyncRead + Send + Unpin + 'static>,
    ) -> Rpc {
        let (lines, out) = mpsc::unbounded_channel();
        let waiting = Waiting::default();
        let ended = Arc::new(Mutex::new(None));
        let kept = Arc::new(Mutex::new(String::new()));
        let mut tasks = tokio::task::JoinSet::new();
        // Told when what it writes to standard error ends too, so why it
        // stopped is all there before anyone is told it did.
        let (errors_done, errors_ended) = oneshot::channel();
        if let Some(errors) = errors {
            tasks.spawn(keep_end(BufReader::new(errors), kept.clone(), errors_done));
        }
        tasks.spawn(write(writer, out));
        tasks.spawn(read(BufReader::new(reader), waiting.clone(), lines.clone(), ended.clone(), errors_ended));
        Rpc { lines, waiting, next: AtomicU64::new(1), ended, errors: kept, _tasks: tasks }
    }

    /// Whether its output is still being read.
    pub(crate) fn alive(&self) -> bool {
        locked(&self.ended).is_none()
    }

    /// Sends `method` with `params` and waits up to `timeout` for the
    /// answer. Dropped while it waits (Esc), or given up on, the request
    /// is cancelled, so the server can stop working on it.
    pub(crate) async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Error> {
        if !self.alive() {
            return Err(self.stopped());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        locked(&self.waiting).insert(id, tx);
        let mut asked = Asked { rpc: self, id, answered: false };
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let answer = tokio::time::timeout(timeout, rx).await;
        match answer {
            Ok(Ok(answer)) => {
                asked.answered = true;
                answer
            }
            // The reader ended, and let go of every request.
            Ok(Err(_)) => {
                asked.answered = true;
                Err(self.stopped())
            }
            Err(_) => Err(Error::TimedOut { what: method.to_owned(), after: timeout }),
        }
    }

    /// Sends `method` as a notification, which has no answer.
    pub(crate) fn notify(&self, method: &str, params: Option<Value>) {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message);
    }

    fn send(&self, message: &Value) {
        // A message is one line: serde_json escapes every newline in it.
        let _ = self.lines.send(message.to_string());
    }

    /// Why it stopped, with the end of what it wrote to standard error.
    fn stopped(&self) -> Error {
        let why = locked(&self.ended).clone().unwrap_or_default();
        let said = crate::clean::text(locked(&self.errors).trim());
        let last: Vec<&str> = said.lines().rev().take(3).collect();
        let said = last.into_iter().rev().collect::<Vec<_>>().join(" / ");
        Error::Stopped([why, said].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("; "))
    }
}

/// A request sent and not yet answered: forgotten when dropped, and if no
/// answer came, cancelled.
struct Asked<'a> {
    rpc: &'a Rpc,
    id: u64,
    answered: bool,
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        locked(&self.rpc.waiting).remove(&self.id);
        if !self.answered {
            self.rpc.notify("notifications/cancelled", Some(json!({ "requestId": self.id, "reason": "rein stopped waiting for it" })));
        }
    }
}

/// Writes each line that comes, until there are no more or the server
/// stops reading.
async fn write(mut writer: impl AsyncWrite + Unpin, mut lines: mpsc::UnboundedReceiver<String>) {
    while let Some(mut line) = lines.recv().await {
        line.push('\n');
        if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
}

/// Reads what the server says until it ends: answers go to whoever asked,
/// its own requests are answered, and its notifications (logs, progress,
/// a changed list) aren't anything rein needs.
async fn read(
    mut reader: impl AsyncBufRead + Unpin,
    waiting: Waiting,
    lines: mpsc::UnboundedSender<String>,
    ended: Arc<Mutex<Option<String>>>,
    errors_ended: oneshot::Receiver<()>,
) {
    let mut line = Vec::new();
    let why = loop {
        line.clear();
        match read_line(&mut reader, &mut line).await {
            Ok(Read::Line) => take(&line, &waiting, &lines),
            Ok(Read::Ended) => break String::new(),
            Ok(Read::TooLong) => break format!("it sent a message longer than {} MiB, so rein stopped reading it", LONGEST_LINE >> 20),
            Err(e) => break format!("its output couldn't be read: {e}"),
        }
    };
    // A server that's gone has closed its standard error too; one that
    // closed only its output isn't waited for long.
    let _ = tokio::time::timeout(ERRORS_END, errors_ended).await;
    *locked(&ended) = Some(why);
    // Whoever waits is told it stopped.
    locked(&waiting).clear();
}

/// One message from the server.
fn take(line: &[u8], waiting: &Waiting, lines: &mpsc::UnboundedSender<String>) {
    // What isn't JSON is a server printing where it shouldn't: let it be.
    let Ok(message) = serde_json::from_slice::<Value>(line) else { return };
    match (message.get("method").and_then(Value::as_str), message.get("id")) {
        // rein says it can do none of what servers may ask a client for
        // (sampling, roots, elicitation), so only `ping` is to be expected.
        (Some(method), Some(id)) => {
            let answer = if method == "ping" {
                json!({ "jsonrpc": "2.0", "id": id, "result": {} })
            } else {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("rein doesn't do {method}") } })
            };
            let _ = lines.send(answer.to_string());
        }
        (Some(_), None) => {}
        (None, Some(id)) => {
            let Some(tx) = id.as_u64().and_then(|id| locked(waiting).remove(&id)) else { return };
            let answer = match message.get("error") {
                Some(error) => Err(Error::Refused {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or_default(),
                    message: crate::clean::text(error.get("message").and_then(Value::as_str).unwrap_or("an error, unsaid")),
                }),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(answer);
        }
        (None, None) => {}
    }
}

enum Read {
    Line,
    Ended,
    TooLong,
}

/// Reads one line into `line`, without its newline, keeping no more than
/// [`LONGEST_LINE`] bytes of it.
async fn read_line(reader: &mut (impl AsyncBufRead + Unpin), line: &mut Vec<u8>) -> std::io::Result<Read> {
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if line.is_empty() { Read::Ended } else { Read::Line });
        }
        let (part, done) = match buf.iter().position(|b| *b == b'\n') {
            Some(at) => (&buf[..at], at + 1),
            None => (buf, buf.len()),
        };
        if line.len() + part.len() > LONGEST_LINE {
            return Ok(Read::TooLong);
        }
        line.extend_from_slice(part);
        let ended = done > part.len();
        reader.consume(done);
        if ended {
            return Ok(Read::Line);
        }
    }
}

/// Keeps the last [`KEPT_ERRORS`] bytes of what `errors` says, and says
/// when it ends by letting go of `_done`.
async fn keep_end(mut errors: impl AsyncBufRead + Unpin, kept: Arc<Mutex<String>>, _done: oneshot::Sender<()>) {
    let mut line = String::new();
    while errors.read_line(&mut line).await.is_ok_and(|n| n > 0) {
        let mut kept = locked(&kept);
        kept.push_str(&line);
        if kept.len() > KEPT_ERRORS {
            let mut cut = kept.len() - KEPT_ERRORS;
            while !kept.is_char_boundary(cut) {
                cut += 1;
            }
            kept.drain(..cut);
        }
        line.clear();
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// A server on the other end of a pipe: each line rein sends goes to
    /// `serve`, and what it gives back is said.
    fn served(serve: impl Fn(Value) -> Vec<String> + Send + 'static) -> Rpc {
        let (ours, theirs) = tokio::io::duplex(64 << 10);
        let (read_ours, write_ours) = tokio::io::split(ours);
        let (read_theirs, mut write_theirs) = tokio::io::split(theirs);
        tokio::spawn(async move {
            let mut lines = BufReader::new(read_theirs).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                for said in serve(serde_json::from_str(&line).unwrap()) {
                    write_theirs.write_all(format!("{said}\n").as_bytes()).await.unwrap();
                }
            }
        });
        Rpc::start(read_ours, write_ours, None::<tokio::io::Empty>)
    }

    #[tokio::test]
    async fn answers_go_to_whoever_asked_and_the_servers_own_noise_is_let_be() {
        let rpc = served(|message| {
            let id = &message["id"];
            match message["method"].as_str() {
                Some("echo") => vec![
                    "a log line where it shouldn't be".into(),
                    json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": { "data": "hi" } }).to_string(),
                    json!({ "jsonrpc": "2.0", "id": 99, "method": "ping" }).to_string(),
                    json!({ "jsonrpc": "2.0", "id": id, "result": message["params"] }).to_string(),
                ],
                Some("fail") => vec![json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": "no\u{1b}[2J such thing" } }).to_string()],
                _ => Vec::new(),
            }
        });
        let echoed = rpc.request("echo", json!({ "x": 1 }), Duration::from_secs(5)).await.unwrap();
        assert_eq!(echoed, json!({ "x": 1 }));
        let refused = rpc.request("fail", json!({}), Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(refused, Error::Refused { code: -32602, message: "no[2J such thing".into() }, "escape codes taken out");
        assert!(rpc.alive());
    }

    #[tokio::test]
    async fn a_server_asking_for_what_rein_doesnt_do_is_told_so_and_ping_is_answered() {
        let (ours, theirs) = tokio::io::duplex(64 << 10);
        let (read_ours, write_ours) = tokio::io::split(ours);
        let (read_theirs, mut write_theirs) = tokio::io::split(theirs);
        let _rpc = Rpc::start(read_ours, write_ours, None::<tokio::io::Empty>);
        write_theirs.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"sampling/createMessage\"}\n").await.unwrap();
        let mut lines = BufReader::new(read_theirs).lines();
        let ping: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ping, json!({ "jsonrpc": "2.0", "id": 7, "result": {} }));
        let other: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(other["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn one_given_up_on_is_cancelled_and_a_server_that_ends_lets_go_of_everyone() {
        let (ours, theirs) = tokio::io::duplex(64 << 10);
        let (read_ours, write_ours) = tokio::io::split(ours);
        let (read_theirs, write_theirs) = tokio::io::split(theirs);
        let rpc = Rpc::start(read_ours, write_ours, None::<tokio::io::Empty>);
        let timed_out = rpc.request("slow", json!({}), Duration::from_millis(50)).await.unwrap_err();
        assert_eq!(timed_out, Error::TimedOut { what: "slow".into(), after: Duration::from_millis(50) });
        let mut lines = BufReader::new(read_theirs).lines();
        let _asked = lines.next_line().await.unwrap().unwrap();
        let cancelled: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!((cancelled["method"].as_str(), cancelled["params"]["requestId"].as_u64()), (Some("notifications/cancelled"), Some(1)));
        // It goes while one waits: that one is told at once, not when its
        // time is up.
        let goes = async move {
            let _asked = lines.next_line().await;
            drop((lines, write_theirs));
            std::future::pending::<()>().await
        };
        let waiting = async {
            tokio::select! {
                answer = rpc.request("never", json!({}), Duration::from_secs(60)) => answer,
                () = goes => unreachable!("it waits forever"),
            }
        };
        let stopped = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("told at once").unwrap_err();
        assert!(matches!(stopped, Error::Stopped(_)), "{stopped:?}");
        assert!(!rpc.alive());
        assert!(matches!(rpc.request("again", json!({}), Duration::from_secs(5)).await, Err(Error::Stopped(_))));
    }

    #[tokio::test]
    async fn a_line_too_long_stops_the_reading_and_says_why() {
        let (mut theirs, ours) = tokio::io::duplex(1 << 20);
        let rpc = Rpc::start(ours, tokio::io::sink(), None::<tokio::io::Empty>);
        let writing = tokio::spawn(async move {
            let chunk = vec![b'x'; 1 << 20];
            for _ in 0..=(LONGEST_LINE >> 20) {
                if theirs.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        let stopped = rpc.request("x", json!({}), Duration::from_secs(10)).await.unwrap_err();
        assert!(stopped.to_string().contains("longer than 8 MiB"), "{stopped}");
        writing.abort();
    }

    #[tokio::test]
    async fn what_a_server_wrote_last_to_standard_error_says_why_it_stopped() {
        let (theirs, ours) = tokio::io::duplex(1024);
        let errors: &[u8] = b"starting\nError: GITHUB_TOKEN missing\n";
        let rpc = Rpc::start(ours, tokio::io::sink(), Some(errors));
        drop(theirs);
        while !locked(&rpc.errors).contains("missing") || rpc.alive() {
            tokio::task::yield_now().await;
        }
        let stopped = rpc.request("x", json!({}), Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(stopped.to_string(), "it stopped, saying: starting / Error: GITHUB_TOKEN missing");
    }
}
