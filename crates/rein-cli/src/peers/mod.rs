//! Other reins on this computer, each in a terminal of its own: how they
//! find each other and what they say.
//!
//! Each conversation puts three things in `~/.config/rein/peers`, a folder
//! only you can open (and that sandboxed commands can neither read nor
//! write, as all of rein's own folder):
//!
//! - `<pid>.sock`, where it listens;
//! - `<pid>.json`, who it is: its name, workspace, folder, and a random
//!   token that whoever writes to it must send back.
//!
//! A connection carries one request and one reply, each a line of JSON. A
//! rein is alive while its socket answers; the files of one that doesn't
//! are cleared away by whoever finds them. Nothing here depends on which
//! terminal rein runs in.

pub(crate) mod dashboard;
pub(crate) mod node;
pub(crate) mod talk;
pub(crate) mod worktree;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub(crate) use node::Node;

/// The most one request or reply may be.
pub(crate) const MAX_LINE: usize = 64 * 1024;
/// How long a peer has to answer, from connecting to its reply.
const ANSWER_WITHIN: Duration = Duration::from_secs(3);
/// The longest a socket's path may be: macOS allows 104 bytes, Linux 108.
const MAX_SOCKET_PATH: usize = 100;

/// Who a rein is, as its file in the peers folder says.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Info {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) workspace: String,
    /// The folder it works in.
    pub(crate) root: PathBuf,
    pub(crate) socket: PathBuf,
    /// What a request to it must carry.
    pub(crate) token: String,
    /// Its terminal, like `/dev/ttys004`, to tell windows apart.
    pub(crate) tty: Option<String>,
}

impl Info {
    /// `workspace/name`: what reaches it from anywhere.
    pub(crate) fn address(&self) -> String {
        format!("{}/{}", self.workspace, self.name)
    }
}

/// What a rein is doing, as its tab's title and the dashboard show it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Idle,
    Working,
    /// Waiting for you to answer a question.
    Waiting,
    /// Finished a task another rein handed it, or its turn.
    Done,
}

impl State {
    /// The mark in front of its name.
    pub(crate) const fn mark(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::Working => "●",
            Self::Waiting => "⚠",
            Self::Done => "✓",
        }
    }

    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "needs you",
            Self::Done => "done",
        }
    }
}

/// A question a rein is waiting on you for, which you can answer from
/// another tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Question {
    /// Which question: an answer to an earlier one is turned away.
    pub(crate) id: u64,
    pub(crate) text: String,
}

/// How a rein is getting on, for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    pub(crate) name: String,
    pub(crate) workspace: String,
    pub(crate) root: PathBuf,
    pub(crate) model: String,
    pub(crate) state: State,
    /// Its current step, like `Running cargo test`.
    pub(crate) doing: Option<String>,
    /// Seconds since the epoch when it went into `state`.
    pub(crate) since: u64,
    /// Tokens sent and received in this conversation.
    pub(crate) tokens: u64,
    /// The files it changed lately, newest first.
    pub(crate) editing: Vec<String>,
    pub(crate) question: Option<Question>,
    /// What went between it and other reins lately, newest first.
    pub(crate) recent: Vec<String>,
}

/// What a request asks for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Body {
    Status,
    Message {
        text: String,
    },
    /// A task handed over; the result goes back to whoever sent it.
    Task {
        id: String,
        text: String,
    },
    /// How a task handed over went.
    Result {
        id: String,
        done: bool,
        summary: String,
        files: Vec<String>,
    },
    /// The sender changed `path`, in the folder they share.
    Changed {
        path: String,
    },
    /// Your answer to `question`, given in another tab.
    Answer {
        question: u64,
        yes: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) token: String,
    pub(crate) body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Reply {
    /// It's in: the other rein reads it between its steps, or starts on it.
    Delivered,
    /// Kept for its user to let in (`/inbox`), as its settings ask.
    Held,
    Refused {
        why: String,
    },
    Status(Status),
}

/// Where the peers are: in rein's own folder, which only you can open.
pub(crate) fn folder(rein: &Path) -> PathBuf {
    rein.join("peers")
}

/// The other reins whose files are in `folder`, alive, except this
/// process. The files of those that don't answer are cleared away.
pub(crate) async fn list(folder: &Path) -> Vec<Info> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mine = std::process::id();
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        // Someone else's or broken: not a peer, and not ours to remove.
        let Some(info) = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Info>(&text).ok()) else { continue };
        if info.pid == mine {
            continue;
        }
        if alive(&info).await {
            found.push(info);
        } else {
            let _ = std::fs::remove_file(&info.socket);
            let _ = std::fs::remove_file(&path);
        }
    }
    found.sort_by_key(Info::address);
    found
}

/// Its socket takes a connection. A dead rein's doesn't: nothing listens.
async fn alive(info: &Info) -> bool {
    matches!(tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(&info.socket)).await, Ok(Ok(_)))
}

/// Sends `body` to `to`, and its reply.
pub(crate) async fn ask(to: &Info, body: Body) -> Result<Reply, String> {
    let request = serde_json::to_string(&Request { token: to.token.clone(), body }).map_err(|e| e.to_string())?;
    if request.len() >= MAX_LINE {
        return Err(format!("that's too long to send: {} KB at most", MAX_LINE / 1024));
    }
    let talk = async {
        let mut stream = UnixStream::connect(&to.socket).await.map_err(|e| format!("can't reach {}: {e}", to.name))?;
        stream.write_all(format!("{request}\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let line = read_line(&mut stream).await?.ok_or_else(|| format!("{} didn't answer", to.name))?;
        serde_json::from_str::<Reply>(&line).map_err(|_| format!("{} answered with something rein doesn't read", to.name))
    };
    tokio::time::timeout(ANSWER_WITHIN, talk).await.map_err(|_| format!("{} didn't answer in time", to.name))?
}

/// One line from `stream`, at most [`MAX_LINE`] long; `None` if it closed
/// without one.
pub(crate) async fn read_line(stream: &mut UnixStream) -> Result<Option<String>, String> {
    let mut reader = BufReader::new(stream.take(MAX_LINE as u64 + 1));
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        return Err(format!("a line longer than {} KB, or one that never ended", MAX_LINE / 1024));
    }
    Ok(Some(line.trim_end().to_owned()))
}

/// Which of `peers` `to` names: `workspace/name` exactly, or a name, in
/// `workspace` first, then anywhere it's the only one.
pub(crate) fn find<'a>(peers: &'a [Info], to: &str, workspace: &str) -> Result<&'a Info, String> {
    let to = to.trim().trim_start_matches('@');
    if let Some((ws, name)) = to.rsplit_once('/') {
        return peers.iter().find(|p| p.workspace == ws && p.name == name).ok_or_else(|| no_one(peers, to));
    }
    if let Some(here) = peers.iter().find(|p| p.workspace == workspace && p.name == to) {
        return Ok(here);
    }
    let named: Vec<&Info> = peers.iter().filter(|p| p.name == to).collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => Err(no_one(peers, to)),
        many => Err(format!("there are {} reins called {to}; say which: {}", many.len(), many.iter().map(|p| p.address()).collect::<Vec<_>>().join(", "))),
    }
}

fn no_one(peers: &[Info], to: &str) -> String {
    if peers.is_empty() {
        return format!("there's no rein called {to}: no other rein is running");
    }
    format!("there's no rein called {to}; there are {}", peers.iter().map(Info::address).collect::<Vec<_>>().join(", "))
}

/// A name as rein keeps it: lower case letters, digits and `-`, as a tab
/// shows it and `/send` types it.
pub(crate) fn clean_name(name: &str) -> Result<String, String> {
    let name: String = name.trim().to_lowercase().chars().map(|c| if c.is_whitespace() || c == '_' { '-' } else { c }).collect();
    if name.is_empty() || name.len() > 32 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("a name is up to 32 letters, digits and dashes, like api or web-2, not {name:?}"));
    }
    Ok(name)
}

/// The name a rein in `root` gets when none is given: its folder's.
pub(crate) fn default_name(root: &Path) -> String {
    let folder = root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let name: String = folder.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').chars().take(24).collect();
    if name.is_empty() { "rein".to_owned() } else { name }
}

/// `wanted`, or the first of `wanted-2`, `wanted-3`... none of `peers` has.
pub(crate) fn free_name(wanted: &str, peers: &[Info]) -> String {
    let taken = |name: &str| peers.iter().any(|p| p.name == name);
    if !taken(wanted) {
        return wanted.to_owned();
    }
    (2..).map(|n| format!("{wanted}-{n}")).find(|name| !taken(name)).unwrap_or_else(|| wanted.to_owned())
}

/// The workspace a rein in `root` is in when none is given: its git
/// branch, as tabs working on one feature are on one branch; or its
/// folder's name, outside git.
pub(crate) fn default_workspace(root: &Path) -> String {
    git_branch(root).unwrap_or_else(|| default_name(root))
}

/// The branch checked out where `root` is, read from git's own files.
fn git_branch(root: &Path) -> Option<String> {
    let dot_git = root.ancestors().map(|dir| dir.join(".git")).find(|path| path.exists())?;
    // A worktree's `.git` is a file that says where its git folder is.
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let dir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if dir.is_absolute() { dir } else { dot_git.parent()?.join(dir) }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_owned()),
        // Not on a branch: the commit it's on.
        None => Some(head.chars().take(7).collect()),
    }
}

/// The terminal rein runs in, like `/dev/ttys004`.
pub(crate) fn tty() -> Option<String> {
    // SAFETY: ttyname returns null or a pointer to a static buffer, which
    // is copied out at once, before anything else can call it.
    unsafe {
        let name = libc::ttyname(0);
        if name.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(name).to_string_lossy().into_owned())
    }
}

/// Now, in seconds since the epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The folder for the peers' files, made if it's missing, and checked: a
/// folder of yours that only you can open, not a link. `None` with why,
/// when it can't be used: rein then runs without peers.
pub(crate) fn private_folder(folder: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).recursive(true).create(folder) {
        Ok(()) => {}
        Err(e) => return Err(format!("can't make {}: {e}", folder.display())),
    }
    let meta = std::fs::symlink_metadata(folder).map_err(|e| format!("can't look at {}: {e}", folder.display()))?;
    // SAFETY: getuid can't fail and touches nothing.
    let me = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != me {
        return Err(format!("{} isn't a folder of yours", folder.display()));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("can't make {} private: {e}", folder.display()))?;
    }
    if folder.join("00000000.sock").as_os_str().len() > MAX_SOCKET_PATH {
        return Err(format!("{} is too long a path for a socket", folder.display()));
    }
    Ok(folder.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, workspace: &str) -> Info {
        Info { pid: 1, name: name.into(), workspace: workspace.into(), root: "/p".into(), socket: "/s".into(), token: "t".into(), tty: None }
    }

    #[test]
    fn a_name_is_found_in_its_workspace_first_then_anywhere_its_the_only_one() {
        let peers = [info("api", "health"), info("api", "checkout"), info("tests", "checkout")];
        assert_eq!(find(&peers, "api", "health").unwrap().workspace, "health");
        assert_eq!(find(&peers, "checkout/api", "health").unwrap().workspace, "checkout");
        assert_eq!(find(&peers, "tests", "health").unwrap().workspace, "checkout", "the only one called tests");
        assert!(find(&peers, "api", "other").unwrap_err().contains("say which: health/api, checkout/api"));
        assert!(find(&peers, "web", "health").unwrap_err().contains("there's no rein called web"));
        // A branch's name has slashes; the name never does.
        let peers = [info("api", "feature/peers")];
        assert_eq!(find(&peers, "feature/peers/api", "x").unwrap().name, "api");
    }

    #[test]
    fn names_are_kept_simple_and_never_taken_twice() {
        assert_eq!(clean_name(" Web Two ").unwrap(), "web-two");
        assert!(clean_name("a/b").is_err());
        assert!(clean_name("\x1b]2;x").is_err(), "nothing that reaches the terminal as a sequence");
        assert_eq!(default_name(Path::new("/home/me/My Project")), "my-project");
        assert_eq!(free_name("api", &[info("api", "w"), info("api-2", "w")]), "api-3");
    }

    #[test]
    fn the_workspace_is_the_branch_a_worktree_is_on_too() {
        let dir = std::env::temp_dir().join(format!("rein-peers-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo/.git")).unwrap();
        std::fs::create_dir_all(dir.join("repo/src")).unwrap();
        std::fs::write(dir.join("repo/.git/HEAD"), "ref: refs/heads/feature/peers\n").unwrap();
        assert_eq!(default_workspace(&dir.join("repo/src")), "feature/peers");
        std::fs::create_dir_all(dir.join("repo/.git/worktrees/wt")).unwrap();
        std::fs::write(dir.join("repo/.git/worktrees/wt/HEAD"), "ref: refs/heads/health\n").unwrap();
        std::fs::create_dir_all(dir.join("wt")).unwrap();
        std::fs::write(dir.join("wt/.git"), format!("gitdir: {}\n", dir.join("repo/.git/worktrees/wt").display())).unwrap();
        assert_eq!(default_workspace(&dir.join("wt")), "health");
        std::fs::create_dir_all(dir.join("plain")).unwrap();
        assert_eq!(default_workspace(&dir.join("plain")), "plain", "outside git: the folder");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn requests_with_something_unknown_are_turned_away() {
        assert!(serde_json::from_str::<Request>(r#"{"token":"t","body":{"kind":"approve_everything"}}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"token":"t","body":{"kind":"message","text":"hi","sudo":true}}"#).is_err());
        let ok: Request = serde_json::from_str(r#"{"token":"t","body":{"kind":"answer","question":3,"yes":true}}"#).unwrap();
        assert_eq!(ok.body, Body::Answer { question: 3, yes: true });
    }
}
