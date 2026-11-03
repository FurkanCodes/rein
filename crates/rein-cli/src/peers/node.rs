//! This rein among the others: its files in the peers folder, the socket
//! it listens on, and what it lets in.
//!
//! What comes in is checked before anything else looks at it: the other
//! end must be you (the same user), send this rein's token, and, to say
//! anything but asking how it's doing or answering its question, be a rein
//! in the peers folder. A sender writing too often, or the same thing
//! twice, is turned away, which ends a loop of two reins answering each
//! other. Text goes to the conversation as another rein's, never yours.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rein_agent::peers::{DelegateArgs, MessageArgs};
use rein_agent::tools::ToolName;
use rein_llm::ToolCall;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use super::{Body, Info, Question, Reply, Request, State, Status};

/// What another rein sent, for the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Incoming {
    Message {
        from: String,
        text: String,
    },
    /// A task to do; how it went goes back to `reply_to`.
    Task {
        from: String,
        reply_to: String,
        id: String,
        text: String,
    },
    Result {
        from: String,
        id: String,
        done: bool,
        summary: String,
        files: Vec<String>,
    },
    Changed {
        from: String,
        path: String,
    },
}

impl Incoming {
    pub(crate) fn from(&self) -> &str {
        match self {
            Self::Message { from, .. } | Self::Task { from, .. } | Self::Result { from, .. } | Self::Changed { from, .. } => from,
        }
    }

    /// What the conversation is told: the words, and for a task or its
    /// result, what it is.
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Message { text, .. } => text.clone(),
            Self::Task { text, .. } => format!("A task for you, handed over by {}. When you're done, your last message goes back to it as the result.\n\n{text}", self.from()),
            Self::Result { id, done, summary, files, .. } => {
                let how = if *done { "is done" } else { "stopped before it was done" };
                let files = if files.is_empty() { "It changed no files.".to_owned() } else { format!("Files it changed: {}.", files.join(", ")) };
                format!("The task you handed over (task {id}) {how}. {files}\n\n{summary}")
            }
            Self::Changed { path, .. } => format!("changed {path}"),
        }
    }
}

/// What this rein lets in from others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Messages go into the conversation, and start a turn when it's idle.
    Accept,
    /// Messages wait in `/inbox` for you to let them in.
    Hold,
    /// Messages are turned away.
    Refuse,
}

/// The `peers` setting: whether this rein joins the others, and on what terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Joining {
    Off,
    On(Policy),
}

impl std::str::FromStr for Joining {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value.trim() {
            "accept" | "on" => Ok(Self::On(Policy::Accept)),
            "hold" => Ok(Self::On(Policy::Hold)),
            "refuse" => Ok(Self::On(Policy::Refuse)),
            "off" => Ok(Self::Off),
            other => Err(format!("peers should be accept, hold, refuse or off, not {other}")),
        }
    }
}

/// The most messages waiting to go into the conversation, and in `/inbox`.
const QUEUE: usize = 50;
/// How many messages one sender may send a minute.
const PER_MINUTE: usize = 20;
/// The files shown as being edited, and the events kept, per rein.
const KEPT: usize = 6;

/// What the server and the conversation share.
struct Shared {
    status: Status,
    policy: Policy,
    /// The folder this rein works in: who says they changed a file must too.
    root: PathBuf,
    token: String,
    folder: PathBuf,
    inbox: mpsc::Sender<Incoming>,
    held: VecDeque<Incoming>,
    /// The question you're asked now, and where an answer from another tab goes.
    waiting: Option<(u64, oneshot::Sender<bool>)>,
    sent: HashMap<String, Sender>,
}

/// What one sender sent lately.
#[derive(Default)]
struct Sender {
    times: VecDeque<Instant>,
    last: Option<(String, Instant)>,
}

fn locked(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct Node {
    info: Info,
    /// What the settings say to let in; trust mode holds even what they'd accept.
    policy: Policy,
    file: PathBuf,
    shared: Arc<Mutex<Shared>>,
    pub(crate) inbox: mpsc::Receiver<Incoming>,
    /// The server, and messages on their way: stopped when this rein ends.
    tasks: JoinSet<()>,
    next_question: u64,
    titled: bool,
}

impl Node {
    /// Joins the reins in `folder` as `name` (taken from the folder when
    /// `None`), in `workspace` (the git branch when `None`), working in `root`.
    pub(crate) async fn start(folder: &Path, name: Option<&str>, workspace: Option<&str>, root: &Path, model: &str, policy: Policy, titled: bool) -> Result<Node, String> {
        let folder = super::private_folder(folder)?;
        let peers = super::list(&folder).await;
        let name = match name {
            Some(name) => {
                let name = super::clean_name(name)?;
                if peers.iter().any(|p| p.name == name) {
                    return Err(format!("another rein is called {name} already; pick another name"));
                }
                name
            }
            None => super::free_name(&super::default_name(root), &peers),
        };
        let workspace = workspace.map(str::to_owned).unwrap_or_else(|| super::default_workspace(root));
        let pid = std::process::id();
        let socket = folder.join(format!("{pid}.sock"));
        // A rein that had this pid before is gone: this process has it now.
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).map_err(|e| format!("can't listen at {}: {e}", socket.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        }
        let token = rein_llm::new_session_id().map_err(|e| e.to_string())?;
        let info = Info { pid, name: name.clone(), workspace: workspace.clone(), root: root.to_owned(), socket, token: token.clone(), tty: super::tty() };
        let (tx, inbox) = mpsc::channel(QUEUE);
        let status = Status {
            name,
            workspace,
            root: root.to_owned(),
            model: model.to_owned(),
            state: State::Idle,
            doing: None,
            since: super::now(),
            tokens: 0,
            editing: Vec::new(),
            question: None,
            recent: Vec::new(),
        };
        let shared = Arc::new(Mutex::new(Shared {
            status,
            policy,
            root: root.to_owned(),
            token,
            folder: folder.clone(),
            inbox: tx,
            held: VecDeque::new(),
            waiting: None,
            sent: HashMap::new(),
        }));
        let mut tasks = JoinSet::new();
        tasks.spawn(serve(listener, shared.clone()));
        let file = folder.join(format!("{pid}.json"));
        // Closing the tab hangs up on rein, which would end it before any
        // Drop runs: its files go first, so no one finds a rein that's gone.
        for (kind, number) in [(tokio::signal::unix::SignalKind::hangup(), libc::SIGHUP), (tokio::signal::unix::SignalKind::terminate(), libc::SIGTERM)] {
            if let Ok(mut signal) = tokio::signal::unix::signal(kind) {
                let (file, socket) = (file.clone(), info.socket.clone());
                tasks.spawn(async move {
                    if signal.recv().await.is_some() {
                        let _ = std::fs::remove_file(&file);
                        let _ = std::fs::remove_file(&socket);
                        std::process::exit(128 + number);
                    }
                });
            }
        }
        let node = Node { file, info, policy, shared, inbox, tasks, next_question: 0, titled };
        node.save()?;
        if titled {
            crate::ui::screen::save_title();
        }
        node.title();
        Ok(node)
    }

    pub(crate) fn info(&self) -> &Info {
        &self.info
    }

    fn folder(&self) -> PathBuf {
        locked(&self.shared).folder.clone()
    }

    /// Writes who this rein is, whole: a temp file of its own, then a rename.
    fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.info).map_err(|e| e.to_string())?;
        rein_llm::store::write_private(&self.file, text.as_bytes()).map_err(|e| format!("can't write {}: {e}", self.file.display()))
    }

    /// The tab's title: what this rein is called and doing.
    fn title(&self) {
        if !self.titled {
            return;
        }
        let status = &locked(&self.shared).status;
        crate::ui::screen::title(&format!("{} {} · {}", status.state.mark(), status.name, status.state.word()));
    }

    pub(crate) async fn rename(&mut self, name: &str) -> Result<String, String> {
        let name = super::clean_name(name)?;
        if super::list(&self.folder()).await.iter().any(|p| p.name == name) {
            return Err(format!("another rein is called {name} already"));
        }
        self.info.name.clone_from(&name);
        locked(&self.shared).status.name.clone_from(&name);
        self.save()?;
        self.title();
        Ok(name)
    }

    pub(crate) fn set_workspace(&mut self, workspace: &str) -> Result<(), String> {
        let workspace = workspace.trim();
        if workspace.is_empty() || workspace.len() > 100 || workspace.chars().any(char::is_control) {
            return Err("a workspace's name is a line of up to 100 characters".into());
        }
        self.info.workspace = workspace.to_owned();
        locked(&self.shared).status.workspace = workspace.to_owned();
        self.save()
    }

    /// What's let in, in `mode`: in trust mode nothing asks you, so
    /// nothing another rein sends goes in without you letting it in.
    pub(crate) fn follow_mode(&self, mode: rein_agent::permissions::Mode) {
        let policy = match (self.policy, mode) {
            (Policy::Accept, rein_agent::permissions::Mode::Trust) => Policy::Hold,
            (policy, _) => policy,
        };
        locked(&self.shared).policy = policy;
    }

    /// What's let in now.
    pub(crate) fn policy(&self) -> Policy {
        locked(&self.shared).policy
    }

    pub(crate) fn set_model(&self, model: &str) {
        locked(&self.shared).status.model = model.to_owned();
    }

    /// Now `state`, doing `doing`; the tab says so.
    pub(crate) fn set_state(&self, state: State, doing: Option<String>) {
        {
            let mut shared = locked(&self.shared);
            if shared.status.state != state {
                shared.status.since = super::now();
            }
            shared.status.state = state;
            shared.status.doing = doing;
        }
        self.title();
    }

    pub(crate) fn doing(&self, what: &str) {
        locked(&self.shared).status.doing = Some(what.to_owned());
    }

    pub(crate) fn add_tokens(&self, tokens: u64) {
        let mut shared = locked(&self.shared);
        shared.status.tokens = shared.status.tokens.saturating_add(tokens);
    }

    fn note(&self, event: String) {
        let mut shared = locked(&self.shared);
        let recent = &mut shared.status.recent;
        recent.insert(0, event);
        recent.truncate(KEPT);
    }

    /// This rein changed `path`: it shows as being edited, and the reins in
    /// the same folder hear of it.
    pub(crate) fn changed(&mut self, path: &str) {
        let (folder, root) = {
            let mut shared = locked(&self.shared);
            let editing = &mut shared.status.editing;
            editing.retain(|p| p != path);
            editing.insert(0, path.to_owned());
            editing.truncate(KEPT);
            (shared.folder.clone(), shared.root.clone())
        };
        let path = path.to_owned();
        self.tasks.spawn(async move {
            for peer in super::list(&folder).await.into_iter().filter(|p| p.root == root) {
                let _ = super::ask(&peer, Body::Changed { path: path.clone() }).await;
            }
        });
    }

    /// You're asked `text`: it can be answered from another tab too. The
    /// answer from there arrives on what this returns.
    pub(crate) fn question(&mut self, text: &str) -> oneshot::Receiver<bool> {
        self.next_question += 1;
        let (tx, rx) = oneshot::channel();
        {
            let mut shared = locked(&self.shared);
            shared.waiting = Some((self.next_question, tx));
            shared.status.question = Some(Question { id: self.next_question, text: text.to_owned() });
        }
        self.set_state(State::Waiting, Some(text.to_owned()));
        rx
    }

    /// The question is answered, here or elsewhere.
    pub(crate) fn answered(&self) {
        {
            let mut shared = locked(&self.shared);
            shared.waiting = None;
            shared.status.question = None;
        }
        self.set_state(State::Working, None);
    }

    pub(crate) async fn peers(&self) -> Vec<Info> {
        super::list(&self.folder()).await
    }

    /// Sends `body` to the rein `to` names.
    pub(crate) async fn send(&self, to: &str, body: Body) -> Result<(Info, Reply), String> {
        let peers = self.peers().await;
        let peer = super::find(&peers, to, &self.info.workspace)?.clone();
        let what = match &body {
            Body::Message { text } => format!("you → {}: {}", peer.name, preview(text)),
            Body::Task { text, .. } => format!("you → {}: task: {}", peer.name, preview(text)),
            Body::Result { done, .. } => format!("you → {}: result ({})", peer.name, if *done { "done" } else { "not done" }),
            Body::Answer { yes, .. } => format!("you answered {}: {}", peer.name, if *yes { "yes" } else { "no" }),
            Body::Status | Body::Changed { .. } => String::new(),
        };
        let reply = super::ask(&peer, body).await?;
        if !what.is_empty() {
            self.note(what);
        }
        Ok((peer, reply))
    }

    /// What's waiting in `/inbox`, oldest first.
    pub(crate) fn held(&self) -> Vec<Incoming> {
        locked(&self.shared).held.iter().cloned().collect()
    }

    pub(crate) fn take_held(&self, i: usize) -> Option<Incoming> {
        locked(&self.shared).held.remove(i)
    }

    /// Keeps `incoming` in `/inbox` instead.
    pub(crate) fn hold(&self, incoming: Incoming) {
        let mut shared = locked(&self.shared);
        if shared.held.len() < QUEUE {
            shared.held.push_back(incoming);
        }
    }

    /// Does what the model asked of another rein.
    pub(crate) async fn tool(&self, call: &ToolCall) -> String {
        let error = rein_agent::tools::ERROR;
        match ToolName::of(call) {
            Some(ToolName::Peers) => {
                let peers = self.peers().await;
                if peers.is_empty() {
                    return "No other rein is running.".to_owned();
                }
                let mut lines = vec![format!("You are {} in workspace {}.", self.info.name, self.info.workspace)];
                for peer in &peers {
                    let status = match super::ask(peer, Body::Status).await {
                        Ok(Reply::Status(status)) => {
                            format!("{} ({}), {}{}", status.state.word(), status.model, status.doing.map(|d| format!("{d}, ")).unwrap_or_default(), status.root.display())
                        }
                        _ => "not answering".to_owned(),
                    };
                    lines.push(format!("- {} (workspace {}): {status}", peer.name, peer.workspace));
                }
                lines.join("\n")
            }
            Some(ToolName::Message) => match serde_json::from_str::<MessageArgs>(&call.arguments) {
                Ok(args) => said(self.send(&args.to, Body::Message { text: args.text }).await, None),
                Err(e) => format!("{error}bad arguments: {e}"),
            },
            Some(ToolName::Delegate) => match serde_json::from_str::<DelegateArgs>(&call.arguments) {
                Ok(args) => {
                    let id: String = rein_llm::new_session_id().unwrap_or_default().chars().take(8).collect();
                    said(self.send(&args.to, Body::Task { id: id.clone(), text: args.task }).await, Some(&id))
                }
                Err(e) => format!("{error}bad arguments: {e}"),
            },
            _ => format!("{error}there is no tool called {}", call.name),
        }
    }
}

/// What the model is told of how a message or task went.
fn said(sent: Result<(Info, Reply), String>, task: Option<&str>) -> String {
    let error = rein_agent::tools::ERROR;
    match sent {
        Ok((peer, Reply::Delivered)) => match task {
            Some(id) => format!("Handed to {} (task {id}). Its result will come back to you as a message when it's done.", peer.name),
            None => format!("Sent to {}.", peer.name),
        },
        Ok((peer, Reply::Held)) => format!("{} keeps messages for its user to let in; it'll see it once they do.", peer.name),
        Ok((peer, Reply::Refused { why })) => format!("{error}{} turned it away: {why}", peer.name),
        Ok((peer, Reply::Status(_))) => format!("{error}{} answered with something else", peer.name),
        Err(e) => format!("{error}{e}"),
    }
}

/// The first words of `text`, on one line.
pub(crate) fn preview(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 60 { format!("{}…", line.chars().take(59).collect::<String>()) } else { line }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_file(&self.info.socket);
        if self.titled {
            crate::ui::screen::restore_title();
        }
    }
}

/// Answers whoever connects, one request at a time, each in a few seconds.
async fn serve(listener: UnixListener, shared: Arc<Mutex<Shared>>) {
    loop {
        let mut stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // Out of files, say: try again shortly, not in a tight loop.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let reply = tokio::time::timeout(Duration::from_secs(2), handle(&mut stream, &shared)).await;
        if let Ok(Some(reply)) = reply
            && let Ok(line) = serde_json::to_string(&reply)
        {
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(format!("{line}\n").as_bytes())).await;
        }
    }
}

/// The reply to one connection's request; `None` for no reply at all.
async fn handle(stream: &mut UnixStream, shared: &Mutex<Shared>) -> Option<Reply> {
    let refused = |why: &str| Some(Reply::Refused { why: why.to_owned() });
    // You, and no one else on this computer.
    let cred = stream.peer_cred().ok()?;
    // SAFETY: getuid can't fail and touches nothing.
    if cred.uid() != unsafe { libc::getuid() } {
        return None;
    }
    // A connection that only checks this rein is alive sends nothing.
    let line = super::read_line(stream).await.ok()??;
    let Ok(request) = serde_json::from_str::<Request>(&line) else { return refused("rein can't read that request") };
    let (token, folder, workspace) = {
        let shared = locked(shared);
        (shared.token.clone(), shared.folder.clone(), shared.status.workspace.clone())
    };
    if !same(&request.token, &token) {
        return refused("wrong token");
    }
    // Who it is: a rein in the folder, by the process on the other end.
    let sender = cred.pid().and_then(|pid| u32::try_from(pid).ok()).and_then(|pid| sender(&folder, pid));
    let from = sender.as_ref().map(|info| if info.workspace == workspace { info.name.clone() } else { info.address() });
    let incoming = match (request.body, sender, from) {
        (Body::Status, _, _) => {
            let shared = locked(shared);
            return Some(Reply::Status(shared.status.clone()));
        }
        // Your answer, from the dashboard or another tab.
        (Body::Answer { question, yes }, _, _) => {
            let mut shared = locked(shared);
            return match shared.waiting.take() {
                Some((id, tx)) if id == question => {
                    let _ = tx.send(yes);
                    Some(Reply::Delivered)
                }
                other => {
                    shared.waiting = other;
                    refused("that question isn't being asked anymore")
                }
            };
        }
        (_, None, _) | (_, _, None) => return refused("only another rein can send that"),
        (Body::Message { text }, Some(_), Some(from)) => Incoming::Message { from, text },
        (Body::Task { id, text }, Some(info), Some(from)) => Incoming::Task { from, reply_to: info.address(), id, text },
        (Body::Result { id, done, summary, files }, Some(_), Some(from)) => Incoming::Result { from, id, done, summary, files },
        (Body::Changed { path }, Some(info), Some(from)) => {
            if info.root != locked(shared).root {
                return refused("that rein works in another folder");
            }
            Incoming::Changed { from, path }
        }
    };
    let mut shared = locked(shared);
    if let Some(why) = too_much(&mut shared.sent, &incoming) {
        return refused(why);
    }
    let what = format!("{} → you: {}", incoming.from(), preview(&incoming.text()));
    let reply = match (shared.policy, &incoming) {
        // A change is only a fact about a file: it never starts anything.
        (_, Incoming::Changed { .. }) => match shared.inbox.try_send(incoming) {
            Ok(()) => return Some(Reply::Delivered),
            Err(_) => return refused("it has too much waiting already"),
        },
        (Policy::Refuse, _) => return refused("it doesn't take messages from other reins"),
        (Policy::Hold, _) => {
            if shared.held.len() >= QUEUE {
                return refused("it has too much waiting already");
            }
            shared.held.push_back(incoming);
            Reply::Held
        }
        (Policy::Accept, _) => match shared.inbox.try_send(incoming) {
            Ok(()) => Reply::Delivered,
            Err(_) => return refused("it has too much waiting already"),
        },
    };
    let recent = &mut shared.status.recent;
    recent.insert(0, what);
    recent.truncate(KEPT);
    Some(reply)
}

/// Why `incoming` is one too many from its sender: too many this minute,
/// or the same as the last one, a moment ago. `None` lets it in.
fn too_much(sent: &mut HashMap<String, Sender>, incoming: &Incoming) -> Option<&'static str> {
    if matches!(incoming, Incoming::Changed { .. }) {
        return None;
    }
    let now = Instant::now();
    let minute = Duration::from_secs(60);
    let sender = sent.entry(incoming.from().to_owned()).or_default();
    while sender.times.front().is_some_and(|t| now.duration_since(*t) > minute) {
        sender.times.pop_front();
    }
    if sender.times.len() >= PER_MINUTE {
        return Some("too many messages this minute");
    }
    let text = incoming.text();
    if sender.last.as_ref().is_some_and(|(last, at)| *last == text && now.duration_since(*at) < minute) {
        return Some("it got that same message a moment ago");
    }
    sender.times.push_back(now);
    sender.last = Some((text, now));
    None
}

/// The rein in `folder` whose process is `pid`.
fn sender(folder: &Path, pid: u32) -> Option<Info> {
    let text = std::fs::read_to_string(folder.join(format!("{pid}.json"))).ok()?;
    serde_json::from_str::<Info>(&text).ok().filter(|info| info.pid == pid)
}

/// The tokens are the same, compared in a time that doesn't say how much
/// of them is.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(from: &str, text: &str) -> Incoming {
        Incoming::Message { from: from.into(), text: text.into() }
    }

    #[test]
    fn a_sender_is_turned_away_when_it_repeats_itself_or_writes_too_often() {
        let mut sent = HashMap::new();
        assert_eq!(too_much(&mut sent, &message("api", "done")), None);
        assert_eq!(too_much(&mut sent, &message("api", "done")), Some("it got that same message a moment ago"));
        assert_eq!(too_much(&mut sent, &message("web", "done")), None, "another sender may say the same");
        for n in 0..PER_MINUTE - 1 {
            assert_eq!(too_much(&mut sent, &message("api", &format!("step {n}"))), None);
        }
        assert_eq!(too_much(&mut sent, &message("api", "one more")), Some("too many messages this minute"));
        assert_eq!(too_much(&mut sent, &Incoming::Changed { from: "api".into(), path: "a".into() }), None, "a change isn't a message");
    }

    #[tokio::test]
    async fn only_you_with_its_token_get_in_and_only_reins_may_say_anything() {
        let folder = PathBuf::from(format!("/tmp/rein-node-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let mut node = Node::start(&folder, Some("api"), Some("w"), Path::new("/p"), "m", Policy::Accept, false).await.unwrap();
        let me = node.info().clone();
        let wrong = Info { token: "not-it".into(), ..me.clone() };
        assert_eq!(super::super::ask(&wrong, Body::Status).await, Ok(Reply::Refused { why: "wrong token".into() }));
        assert!(matches!(super::super::ask(&me, Body::Status).await, Ok(Reply::Status(status)) if status.name == "api"));

        // An answer to a question that's no longer asked goes nowhere.
        let answer = node.question("Allow this command?");
        assert_eq!(super::super::ask(&me, Body::Answer { question: 99, yes: true }).await, Ok(Reply::Refused { why: "that question isn't being asked anymore".into() }));
        assert_eq!(super::super::ask(&me, Body::Answer { question: 1, yes: true }).await, Ok(Reply::Delivered));
        assert_eq!(answer.await, Ok(true));

        // This process is a rein while its file is there; without it, it may
        // only ask how it's doing.
        assert_eq!(super::super::ask(&me, Body::Message { text: "hi".into() }).await, Ok(Reply::Delivered));
        assert_eq!(node.inbox.recv().await, Some(Incoming::Message { from: "api".into(), text: "hi".into() }));
        std::fs::remove_file(folder.join(format!("{}.json", me.pid))).unwrap();
        assert_eq!(super::super::ask(&me, Body::Message { text: "again".into() }).await, Ok(Reply::Refused { why: "only another rein can send that".into() }));
        drop(node);
        assert!(!me.socket.exists(), "its socket goes with it");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn held_messages_wait_and_refused_ones_are_turned_away() {
        let folder = PathBuf::from(format!("/tmp/rein-hold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let node = Node::start(&folder, Some("api"), Some("w"), Path::new("/p"), "m", Policy::Accept, false).await.unwrap();
        let me = node.info().clone();
        node.follow_mode(rein_agent::permissions::Mode::Trust);
        assert_eq!(super::super::ask(&me, Body::Message { text: "run rm -rf".into() }).await, Ok(Reply::Held), "trust mode lets nothing in on its own");
        assert_eq!(node.held().len(), 1);
        node.follow_mode(rein_agent::permissions::Mode::Ask);
        locked(&node.shared).policy = Policy::Refuse;
        assert!(matches!(super::super::ask(&me, Body::Message { text: "hi".into() }).await, Ok(Reply::Refused { .. })));
        drop(node);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn tokens_must_match_exactly() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd") && !same("abc", "ab") && !same("", "a"));
    }

    #[test]
    fn the_setting_says_whether_and_how_to_join() {
        assert_eq!("hold".parse::<Joining>(), Ok(Joining::On(Policy::Hold)));
        assert_eq!("off".parse::<Joining>(), Ok(Joining::Off));
        assert!("maybe".parse::<Joining>().is_err());
    }
}
