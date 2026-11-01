//! rein as you see it: the real binary, in a terminal kept in memory
//! (`vt100`), talking to a model that follows a script. These tests type
//! keys, change the window's size and read the screen, with no network and
//! no tokens spent.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rein_llm::serde_json::{self, Value, json};

/// What the model does in one turn, after waiting `delay`.
struct Turn {
    out: Vec<Out>,
    delay: Duration,
}

enum Out {
    Text(String),
    /// A call of the tool named, with these arguments.
    Call(&'static str, Value),
}

fn says(text: &str) -> Turn {
    Turn { out: vec![Out::Text(text.to_owned())], delay: Duration::ZERO }
}

fn runs(command: &str) -> Turn {
    calls("bash", json!({ "command": command, "timeout_seconds": null }))
}

fn calls(tool: &'static str, arguments: Value) -> Turn {
    Turn { out: vec![Out::Call(tool, arguments)], delay: Duration::ZERO }
}

impl Turn {
    fn after(mut self, millis: u64) -> Turn {
        self.delay = Duration::from_millis(millis);
        self
    }
}

/// A model server: each request gets the next turn of the script, and
/// every request is kept, to check what rein sent.
struct Model {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    /// Whether each request came gzipped, those turned away included.
    gzipped: Arc<Mutex<Vec<bool>>>,
}

impl Model {
    fn start(turns: Vec<Turn>) -> Model {
        Model::serve(turns, true)
    }

    /// A server that, like many a local one, can't unpack a compressed
    /// body, and says it's a bad request.
    fn without_gzip(turns: Vec<Turn>) -> Model {
        Model::serve(turns, false)
    }

    fn serve(turns: Vec<Turn>, takes_gzip: bool) -> Model {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}/v1", listener.local_addr().unwrap().port());
        let (requests, gzipped) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        let (seen, how) = (requests.clone(), gzipped.clone());
        std::thread::spawn(move || {
            let mut turns = turns.into_iter();
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { break };
                let Some((head, body)) = read_request(&mut socket) else { continue };
                let reply = if head.starts_with("GET") {
                    http(200, "application/json", json!({ "data": [{ "id": "test-model" }] }).to_string().as_bytes())
                } else {
                    let gzip = head.lines().any(|line| line.to_ascii_lowercase().starts_with("content-encoding:") && line.contains("gzip"));
                    how.lock().unwrap().push(gzip);
                    if gzip && !takes_gzip {
                        http(400, "application/json", br#"{"detail":"Bad Request"}"#)
                    } else {
                        let body = if gzip { gunzip(&body) } else { body };
                        seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                        let turn = turns.next().unwrap_or_else(|| says("(the script has no more turns)"));
                        std::thread::sleep(turn.delay);
                        http(200, "text/event-stream", sse(&turn).as_bytes())
                    }
                };
                let _ = socket.write_all(&reply);
            }
        });
        Model { base, requests, gzipped }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn gzipped(&self) -> Vec<bool> {
        self.gzipped.lock().unwrap().clone()
    }
}

fn gunzip(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body).read_to_end(&mut out).unwrap();
    out
}

fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|n| n.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    while data.len() < head_end + length {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Some((head, data[head_end..].to_vec()))
}

fn http(status: u16, kind: &str, body: &[u8]) -> Vec<u8> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let mut reply = format!("HTTP/1.1 {status} {reason}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
    reply.extend_from_slice(body);
    reply
}

/// A turn as the Responses API streams it.
fn sse(turn: &Turn) -> String {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let mut out = String::new();
    let mut event = |v: Value| out += &format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    for item in &turn.out {
        match item {
            Out::Text(text) => {
                for piece in text.split_inclusive(' ') {
                    event(json!({ "type": "response.output_text.delta", "delta": piece }));
                }
                let message = json!({ "type": "message", "content": [{ "type": "output_text", "text": text }] });
                event(json!({ "type": "response.output_item.done", "item": message }));
            }
            Out::Call(tool, arguments) => {
                let call_id = format!("call_{}", CALLS.fetch_add(1, Ordering::Relaxed));
                let call = json!({ "type": "function_call", "call_id": call_id, "namespace": "rein", "name": tool, "arguments": arguments.to_string() });
                event(json!({ "type": "response.output_item.done", "item": call }));
            }
        }
    }
    let usage = json!({ "input_tokens": 1200, "input_tokens_details": { "cached_tokens": 1000 }, "output_tokens": 20, "output_tokens_details": { "reasoning_tokens": 0 } });
    event(json!({ "type": "response.completed", "response": { "usage": usage } }));
    out
}

/// What the terminal was asked to do besides drawing.
#[derive(Default)]
struct Seen {
    bells: usize,
    /// What was put on the clipboard (OSC 52), decoded.
    copied: Vec<String>,
}

impl vt100::Callbacks for Seen {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], data: &[u8]) {
        self.copied.push(String::from_utf8(unbase64(data)).unwrap());
    }
}

fn unbase64(data: &[u8]) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let digits: Vec<u8> = data.iter().copied().filter(|&c| c != b'=').map(value).collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &d)| n | u32::from(d) << (18 - 6 * i));
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    out
}

/// rein running in a pseudo-terminal, read through a terminal emulator.
struct Term {
    screen: Arc<Mutex<vt100::Parser<Seen>>>,
    keyboard: File,
    child: Child,
    dir: PathBuf,
}

impl Term {
    fn start(model: &Model, name: &str, rows: u16, cols: u16) -> Term {
        let dir = std::env::temp_dir().join(format!("rein-screen-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Term::spawn(model, dir, rows, cols)
    }

    /// rein again, in the same folder and with the same home, once this one
    /// has left.
    fn again(mut self, model: &Model) -> Term {
        self.wait_exit();
        let dir = std::mem::take(&mut self.dir);
        let (rows, cols) = self.screen.lock().unwrap().screen().size();
        Term::spawn(model, dir, rows, cols)
    }

    fn spawn(model: &Model, dir: PathBuf, rows: u16, cols: u16) -> Term {
        let (project, home) = (dir.join("project"), dir.join("home"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: openpty writes two descriptors into the ints we pass.
        let opened = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
        assert_eq!(opened, 0, "can't open a pseudo-terminal");
        // SAFETY: openpty just gave us these, and nothing else owns them.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

        let mut command = Command::new(env!("CARGO_BIN_EXE_rein"));
        command
            .current_dir(&project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("REIN_HOME", &home)
            .env("TERM", "xterm-256color")
            .env("OPENAI_API_KEY", "test")
            .env("OPENAI_BASE_URL", &model.base)
            // Copies go to the terminal (OSC 52), never this computer's clipboard.
            .env("SSH_CONNECTION", "screen-test")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only calls that are safe between fork and exec.
        unsafe {
            command.pre_exec(|| {
                // Its own session, with this terminal as its own: so it gets
                // SIGWINCH when the window changes size.
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(command);

        let screen = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(rows, cols, 1000, Seen::default())));
        let shown = screen.clone();
        let mut output = File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            while let Ok(n @ 1..) = output.read(&mut buf) {
                shown.lock().unwrap().process(&buf[..n]);
            }
        });
        Term { screen, keyboard: File::from(master), child, dir }
    }

    fn keys(&mut self, keys: &str) {
        self.keyboard.write_all(keys.as_bytes()).unwrap();
    }

    /// The window changes size: the terminal first, then rein hears of it.
    fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.lock().unwrap().screen_mut().set_size(rows, cols);
        let size = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ reads the winsize we pass.
        unsafe { libc::ioctl(self.keyboard.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    }

    /// The screen's rows as text.
    fn rows(&self) -> Vec<String> {
        let parser = self.screen.lock().unwrap();
        let (_, cols) = parser.screen().size();
        parser.screen().rows(0, cols).collect()
    }

    fn text(&self) -> String {
        self.rows().join("\n")
    }

    /// Waits until the screen shows `text`; fails with the screen if it doesn't.
    fn wait_for(&self, text: &str) {
        self.wait_until(&format!("the screen to show {text:?}"), |screen| screen.contains(text));
    }

    fn wait_until(&self, what: &str, done: impl Fn(&str) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if done(&self.text()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}. The screen:\n{}", self.text());
    }

    fn bells(&self) -> usize {
        self.screen.lock().unwrap().callbacks().bells
    }

    fn wait_bells(&self, bells: usize) {
        let start = Instant::now();
        while self.bells() < bells && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(self.bells(), bells, "the bell rang {} times, not {bells}. The screen:\n{}", self.bells(), self.text());
    }

    /// The text in the input box's first line.
    fn in_box(&self) -> String {
        let rows = self.rows();
        let line = rows.iter().find(|row| row.starts_with("│ ")).cloned().unwrap_or_default();
        line.trim_start_matches("│ ").trim_end_matches('│').trim().trim_start_matches('›').trim().to_owned()
    }

    /// Waits for rein to exit.
    fn wait_exit(&mut self) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if self.child.try_wait().unwrap().is_some() {
                // Whatever it printed last is still on its way.
                std::thread::sleep(Duration::from_millis(100));
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("rein didn't exit. The screen:\n{}", self.text());
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !self.dir.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

const IDLE: &str = "type a message";

/// Long enough to wrap at any width these tests use.
const PARAGRAPH: &str = "A harness is everything around the model: the conversation, the tools, the checks before anything runs, and the screen you read it all on, laid out again whenever the window changes size.";

#[test]
fn an_answer_appears_above_the_box() {
    let model = Model::start(vec![says("Hello there, how can I help?")]);
    let mut term = Term::start(&model, "answer", 24, 80);
    term.wait_for(IDLE);
    term.keys("hi\r");
    term.wait_for("how can I help?");
    term.wait_for(IDLE);
    let screen = term.text();
    assert!(screen.contains("you    hi"), "{screen}");
    assert!(screen.contains("rein   Hello there, how can I help?"), "{screen}");
    assert!(screen.contains("context"), "the status bar is under the box:\n{screen}");
    assert!(model.requests()[0].to_string().contains("\"hi\""), "the model was sent the message");
    assert_eq!(model.gzipped(), [true], "the conversation went gzipped");
    assert_eq!(term.bells(), 0, "you were there: no bell");
    term.keys("/status\r");
    term.wait_for("fewer bytes)");
    let screen = term.text();
    let row = screen.lines().find(|row| row.contains("sent ")).unwrap_or_default();
    assert!(row.contains(" KB as ") && row.contains("(gzip, "), "{screen}");
}

#[test]
fn a_server_that_cant_unpack_gzip_gets_it_plain() {
    let model = Model::without_gzip(vec![says("Hello there."), says("Still here.")]);
    let mut term = Term::start(&model, "plain", 24, 90);
    term.wait_for(IDLE);
    term.keys("hi\r");
    term.wait_for("Hello there.");
    term.wait_for("(this server doesn't take compressed requests, so they go uncompressed)");
    term.wait_for(IDLE);
    term.keys("and again\r");
    term.wait_for("Still here.");
    assert_eq!(model.gzipped(), [true, false, false], "turned away once, then sent as it is");
    assert_eq!(model.requests().len(), 2, "both messages were answered");
    let notes = term.text().matches("doesn't take compressed").count();
    assert_eq!(notes, 1, "said once");
    term.wait_for(IDLE);
    term.keys("/status\r");
    term.wait_for("uncompressed: the server doesn't take compressed requests");
}

#[test]
fn a_command_is_asked_about_whole_and_runs_once_allowed() {
    let command = "echo one && echo two && echo three-four-five-six-seven-eight-nine-ten";
    let model = Model::start(vec![runs(command), says("All three lines printed.")]);
    let mut term = Term::start(&model, "approve", 24, 50);
    term.wait_for(IDLE);
    term.keys("go\r");
    term.wait_for("Allow this command?");
    // The whole command is there to judge, though it's wider than the box.
    let screen = term.text();
    for word in command.split(' ') {
        assert!(screen.contains(word), "{word:?} isn't on screen:\n{screen}");
    }
    let command_rows = screen.lines().filter(|row| row.contains("echo") || row.contains("three-four"));
    assert!(command_rows.clone().all(|row| !row.contains('…')), "nothing of it is cut short:\n{screen}");
    // Each answer's key comes first, so a narrow box can't cut it off.
    for key in ["› y", "a    Yes, and", "n    No", "esc  No, and"] {
        assert!(screen.contains(key), "{key:?} isn't on screen:\n{screen}");
    }
    // Keys too soon after a question still go to the box.
    std::thread::sleep(Duration::from_millis(700));
    term.keys("y");
    term.wait_for("All three lines printed.");
    let screen = term.text();
    assert!(screen.contains("● Run") && screen.contains("exit 0"), "{screen}");
    let sent = model.requests()[1].to_string();
    assert!(sent.contains("three-four-five-six-seven-eight-nine-ten"), "the output went back to the model: {sent}");
}

#[test]
fn a_narrower_window_rewraps_whole_words() {
    let model = Model::start(vec![says(PARAGRAPH)]);
    let mut term = Term::start(&model, "resize", 24, 100);
    term.wait_for(IDLE);
    term.keys("explain\r");
    term.wait_until("the whole answer", |screen| screen.contains("size.") && screen.contains(IDLE));
    for cols in [40, 63, 100, 31] {
        term.resize(24, cols);
        term.wait_until(&format!("the box at {cols} columns"), |screen| {
            // Drawn again whole: the old box, cut at the new edge, has no corner there.
            screen.lines().any(|row| row.starts_with('╭') && row.ends_with('╮') && row.chars().count() == usize::from(cols))
        });
        let rows = term.rows();
        let start = rows.iter().position(|row| row.starts_with("rein   ")).expect("the answer");
        let answer: Vec<&str> = rows[start..].iter().take_while(|row| !row.trim().is_empty()).map(|row| row.get(7..).unwrap_or("").trim()).collect();
        assert_eq!(answer.join(" "), PARAGRAPH, "at {cols} columns, every word is whole:\n{}", rows.join("\n"));
        assert!(answer.len() > 1 || cols == 100, "it wraps at {cols} columns");
        let boxes = rows.iter().filter(|row| row.contains('╭')).count();
        assert_eq!(boxes, 1, "one box, nothing left of an old one:\n{}", rows.join("\n"));
    }
}

#[test]
fn the_wheel_scrolls_back_and_pgdn_returns() {
    let lines: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    let model = Model::start(vec![says(&lines)]);
    let mut term = Term::start(&model, "scroll", 16, 60);
    term.wait_for(IDLE);
    term.keys("count\r");
    term.wait_for("line 40");
    assert!(!term.text().contains("line 20"));
    for _ in 0..5 {
        term.keys("\x1b[<64;10;5M");
    }
    // Five notches, three rows each.
    term.wait_for("15 more lines");
    let screen = term.text();
    assert!(screen.contains("line 20") && !screen.contains("line 40"), "scrolled back:\n{screen}");
    for _ in 0..3 {
        term.keys("\x1b[6~");
    }
    term.wait_until("the end of the conversation again", |screen| screen.contains("line 40") && !screen.contains("more lines"));
}

#[test]
fn a_message_typed_while_rein_works_waits_its_turn() {
    let model = Model::start(vec![says("First answer.").after(1500), says("Second answer.")]);
    let mut term = Term::start(&model, "queue", 24, 80);
    term.wait_for(IDLE);
    term.keys("first\r");
    term.wait_for("Thinking");
    term.keys("second\r");
    term.wait_for("queued: second");
    term.wait_for("Second answer.");
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].to_string().contains("\"second\""), "sent once the first turn was over");
}

#[test]
fn typing_a_slash_lists_the_commands() {
    let model = Model::start(Vec::new());
    let mut term = Term::start(&model, "slash", 24, 80);
    term.wait_for(IDLE);
    term.keys("/mo");
    term.wait_for("list the models");
    let screen = term.text();
    assert!(screen.contains("/mode") && screen.contains("/model"), "{screen}");
    assert!(!screen.contains("/status"), "only the ones that match:\n{screen}");
}

#[test]
fn leaving_leaves_the_conversation_in_the_terminal() {
    let model = Model::start(vec![says("Bye for now.")]);
    let mut term = Term::start(&model, "leave", 30, 80);
    term.wait_for(IDLE);
    term.keys("hello\r");
    term.wait_for("Bye for now.");
    term.wait_for(IDLE);
    term.keys("\x04");
    term.wait_exit();
    let screen = term.text();
    assert!(!screen.contains('╭'), "the box is gone:\n{screen}");
    assert!(screen.contains("you    hello") && screen.contains("rein   Bye for now."), "{screen}");
    assert!(screen.contains("rein -c continues this conversation"), "{screen}");
}

#[test]
fn what_you_sent_comes_back_in_the_next_run() {
    let model = Model::start(vec![says("On it."), says("Running them."), says("Fixed.")]);
    let mut term = Term::start(&model, "history", 24, 80);
    term.wait_for(IDLE);
    term.keys("fix the parser please\r");
    term.wait_for("On it.");
    term.wait_for(IDLE);
    term.keys("run the tests\r");
    term.wait_for("Running them.");
    term.wait_for(IDLE);
    term.keys("\x04");

    let mut term = term.again(&model);
    term.wait_for(IDLE);
    term.keys("\x1b[A");
    term.wait_until("the last message in the box", |_| term.in_box() == "run the tests");
    term.keys("\x1b[A");
    term.wait_until("the one before", |_| term.in_box() == "fix the parser please");
    term.keys("\x15");
    term.wait_for(IDLE);

    // Ctrl-R finds it by a word, wherever it is in the message.
    term.keys("\x12");
    term.wait_for("finding a message you sent");
    term.keys("pars");
    term.wait_until("only the message with the word", |screen| screen.contains("› fix the parser please") && !screen.contains("run the tests"));
    term.keys("\r");
    term.wait_until("it in the box", |screen| term.in_box() == "fix the parser please" && !screen.contains("finding"));
    assert_eq!(model.requests().len(), 2, "picked, not sent: nothing goes without you seeing it");
    term.keys("\r");
    term.wait_for("Fixed.");
    assert!(model.requests()[2].to_string().contains("fix the parser please"));
}

#[test]
fn copy_puts_the_last_answer_on_the_clipboard() {
    let model = Model::start(vec![says("It's a Rust workspace."), says("Run `cargo test --all`, then **commit**.")]);
    let mut term = Term::start(&model, "copy", 24, 80);
    term.wait_for(IDLE);
    term.keys("what is this\r");
    term.wait_for("Rust workspace.");
    term.wait_for(IDLE);
    term.keys("how do I check it\r");
    term.wait_for("then commit.");
    term.wait_for(IDLE);
    term.keys("/copy\r");
    term.wait_for("copied rein's last answer (1 line)");
    let copied = term.screen.lock().unwrap().callbacks().copied.clone();
    assert_eq!(copied, ["Run `cargo test --all`, then **commit**."], "as the model wrote it, Markdown and all");
}

#[test]
fn the_bell_rings_when_rein_needs_you_and_youre_away() {
    let model = Model::start(vec![runs("echo hi"), says("Done.")]);
    let mut term = Term::start(&model, "bell", 24, 80);
    term.wait_for(IDLE);
    // The window is in front: a question doesn't ring.
    term.keys("\x1b[I");
    term.keys("go\r");
    term.wait_for("Allow this command?");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(term.bells(), 0, "you were looking");
    // You go to another window while it waits: it rings, once.
    term.keys("\x1b[O");
    term.wait_bells(1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(term.bells(), 1);
    term.keys("y");
    term.wait_for("Done.");
    // Done, and you're still away: your turn.
    term.wait_bells(2);
}

#[test]
fn a_search_runs_without_asking() {
    let grep = json!({ "pattern": r"fn parse_\w+", "path": null, "glob": "*.rs", "ignore_case": false, "output": "lines", "context": null });
    let model = Model::start(vec![calls("grep", grep), says("It's in src/parser.rs, line 2.")]);
    let mut term = Term::start(&model, "search", 24, 80);
    let project = term.dir.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/parser.rs"), "use x;\nfn parse_list() {}\n").unwrap();
    std::fs::write(project.join("notes.md"), "fn parse_list in prose\n").unwrap();
    term.wait_for(IDLE);
    term.keys("where is parse_list\r");
    term.wait_for("line 2.");
    let screen = term.text();
    assert!(screen.contains(r#"● Search "fn parse_\w+" (*.rs)  1 match in 1 file"#), "{screen}");
    assert!(screen.contains("└ src/parser.rs"), "{screen}");
    assert!(!screen.contains("Allow"), "a search is never asked about:\n{screen}");
    let tools = model.requests()[0]["tools"].to_string();
    assert!(tools.contains("\"grep\"") && tools.contains("\"glob\""), "the model is told about both: {tools}");
    let sent = model.requests()[1].to_string();
    assert!(sent.contains(r"1 match in 1 file\nsrc/parser.rs\n2:fn parse_list() {}"), "the model got the line: {sent}");
}
