//! Pages: what the model makes to be looked at, not read in a terminal (a
//! report, a diagram, a chart, a small tool), as Claude's artifacts are.
//! Each is one HTML file the model writes whole, kept per project in
//! rein's folder, never in the project, and served on this computer for
//! your browser. A page open there reloads itself when the model saves it
//! again, so you watch it change as you talk.
//!
//! Plain HTTP on 127.0.0.1, under a random token in the path: another user
//! of this computer, or a web page guessing ports, finds nothing. What a
//! page may load is held to the few places its description names, and it
//! sends no `Referer`, which would carry the token to them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinSet;

/// The largest page taken: a page carries its own styles and scripts, and
/// the model writes it whole.
pub const LARGEST: usize = 5 * 1024 * 1024;

/// The token's length, in base64url letters of 6 bits each.
const TOKEN: usize = 22;

/// How long a request may take to say what it wants.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const LONGEST_HEAD: u64 = 16 * 1024;

/// How often an open page is pinged, which finds one that's gone.
const PING: Duration = Duration::from_secs(15);

/// How long after a page's tab went that it counts as shown still: a page
/// reloading after a save is gone a moment, and opening it again then
/// would give you a second tab.
const RELOADING: Duration = Duration::from_secs(5);

/// What a page may load, and from where: its own inline styles and
/// scripts, scripts from the three CDNs the tool names, Google's fonts,
/// and images from anywhere. It talks to nothing but rein, and is framed
/// by nothing.
const POLICY: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-inline' https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com; \
    style-src 'self' 'unsafe-inline' https://fonts.googleapis.com https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com; \
    font-src 'self' data: https://fonts.gstatic.com https://cdnjs.cloudflare.com https://cdn.jsdelivr.net https://unpkg.com; \
    img-src 'self' data: blob: https:; media-src 'self' data: blob: https:; \
    connect-src 'self'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'";

/// What every page gets before its `</body>`: it reloads when the page is
/// saved again, and keeps where you'd scrolled to.
const RELOAD: &str = "<script>(()=>{const k='rein-page-scroll';try{const y=sessionStorage.getItem(k);\
if(y!==null){sessionStorage.removeItem(k);addEventListener('load',()=>scrollTo(0,+y));}}catch(e){}\
const s=new EventSource(location.pathname.replace(/\\.html$/,'.events'));\
s.addEventListener('saved',()=>{try{sessionStorage.setItem(k,String(scrollY));}catch(e){}location.reload();});})();</script>";

/// The pages for one project, and what serves them. Clones share both: the
/// tool saves through one, and what shows the conversation asks another
/// whether a page is open yet.
#[derive(Clone)]
pub struct Pages {
    shared: Arc<Shared>,
    /// Serving, once a page is first saved: dropped with the last clone,
    /// it stops listening, and every open page with it.
    server: Arc<Mutex<Option<Server>>>,
}

struct Server {
    base: String,
    _serving: JoinSet<()>,
}

struct Shared {
    folder: PathBuf,
    token: String,
    /// The name of each page saved, as it's saved: its open tabs reload.
    saved: broadcast::Sender<String>,
    /// The tabs showing each page.
    tabs: Mutex<HashMap<String, Tabs>>,
}

#[derive(Default)]
struct Tabs {
    open: usize,
    /// When the last one went.
    left: Option<Instant>,
    /// When rein last had it opened: a browser takes a moment to start.
    opened: Option<Instant>,
}

/// What the model asks of the page tool.
#[derive(Deserialize)]
struct Args {
    action: Action,
    name: String,
    html: Option<String>,
    old_text: Option<String>,
    new_text: Option<String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Save,
    Edit,
    Read,
}

impl std::fmt::Debug for Pages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is left out: it's what keeps the pages yours.
        f.debug_struct("Pages").field("folder", &self.shared.folder).finish_non_exhaustive()
    }
}

impl Pages {
    /// The pages for the project at `root`, in `rein`'s `pages` folder.
    pub fn for_project(rein: &Path, root: &Path) -> Result<Pages, String> {
        Pages::at(crate::session::folder(&rein.join("pages"), root))
    }

    /// Pages in exactly `folder`.
    pub fn at(folder: PathBuf) -> Result<Pages, String> {
        // 132 random bits, short enough for the link to fit under a step.
        let token: String = rein_llm::random_token().map_err(|e| e.to_string())?.chars().take(TOKEN).collect();
        let shared = Shared { folder, token, saved: broadcast::channel(16).0, tabs: Mutex::default() };
        Ok(Pages { shared: Arc::new(shared), server: Arc::default() })
    }

    pub fn folder(&self) -> &Path {
        &self.shared.folder
    }

    /// Where the page `name` is shown, once rein serves pages.
    pub fn link(&self, name: &str) -> Option<String> {
        lock(&self.server).as_ref().map(|server| format!("{}/{name}.html", server.base))
    }

    /// Whether a browser shows the page `name`, or did a moment ago, or
    /// is opening it.
    pub fn shown(&self, name: &str) -> bool {
        let recent = |at: Option<Instant>| at.is_some_and(|at| at.elapsed() < RELOADING);
        lock(&self.shared.tabs).get(name).is_some_and(|tabs| tabs.open > 0 || recent(tabs.left) || recent(tabs.opened))
    }

    /// The link to open for the page `name`, if no browser shows it: it
    /// counts as shown from now, so a save straight after opens no second
    /// tab while the first is still loading.
    pub fn to_open(&self, name: &str) -> Option<String> {
        let link = self.link(name)?;
        if self.shown(name) {
            return None;
        }
        lock(&self.shared.tabs).entry(name.to_owned()).or_default().opened = Some(Instant::now());
        Some(link)
    }

    /// The page a call of the tool is about, if it saves one: for what
    /// shows the conversation, to open it.
    pub fn saved_by(arguments: &str) -> Option<String> {
        let args: Args = serde_json::from_str(arguments).ok()?;
        (args.action != Action::Read).then(|| args.name.trim().to_owned())
    }

    /// Does what the model asked, with keys hidden by `hide` from what's
    /// saved.
    pub(crate) async fn run(&self, arguments: &str, hide: &dyn Fn(String) -> String) -> Result<String, String> {
        let args: Args = serde_json::from_str(arguments).map_err(|e| format!("bad arguments: {e}"))?;
        let name = args.name.trim();
        check_name(name)?;
        let path = self.shared.folder.join(format!("{name}.html"));
        let html = match args.action {
            Action::Read => return self.read(name),
            Action::Save => args.html.filter(|html| !html.trim().is_empty()).ok_or("a page to save needs its html, the whole page")?,
            Action::Edit => {
                let old = args.old_text.filter(|old| !old.is_empty()).ok_or("an edit needs old_text, copied exactly from the page")?;
                let new = args.new_text.ok_or("an edit needs new_text, which may be empty")?;
                let page = self.read(name)?;
                match page.matches(&old).count() {
                    0 => return Err(format!("old_text isn't in the page {name}: read it, and copy the text exactly")),
                    1 => page.replacen(&old, &new, 1),
                    n => return Err(format!("old_text is in the page {name} {n} times: add the lines around it to make it unique")),
                }
            }
        };
        if html.len() > LARGEST {
            return Err(format!("the page is {} KB, more than the {} KB a page may be", html.len() / 1024, LARGEST / 1024));
        }
        let existed = path.exists();
        rein_llm::store::write_private(&path, hide(html).as_bytes()).map_err(|e| format!("can't write {}: {e}", path.display()))?;
        let base = self.serve().await?;
        let _ = self.shared.saved.send(name.to_owned());
        let what = if existed { "Saved" } else { "Made" };
        Ok(format!("{what} the page {name}.\n{base}/{name}.html\nThe user sees it in their browser, which reloads it each time it's saved."))
    }

    /// The page `name` as it was saved, or what pages there are.
    fn read(&self, name: &str) -> Result<String, String> {
        let path = self.shared.folder.join(format!("{name}.html"));
        std::fs::read_to_string(&path).map_err(|_| match self.names().as_slice() {
            [] => format!("there's no page called {name}, and no pages yet"),
            names => format!("there's no page called {name}; there are: {}", names.join(", ")),
        })
    }

    /// The pages saved for this project, by name.
    pub fn names(&self) -> Vec<String> {
        names_in(&self.shared.folder)
    }

    /// Where pages are served, starting to serve them the first time.
    async fn serve(&self) -> Result<String, String> {
        if let Some(server) = lock(&self.server).as_ref() {
            return Ok(server.base.clone());
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("can't serve pages: {e}"))?;
        let port = listener.local_addr().map_err(|e| format!("can't serve pages: {e}"))?.port();
        let mut server = lock(&self.server);
        if let Some(server) = server.as_ref() {
            return Ok(server.base.clone());
        }
        let base = format!("http://127.0.0.1:{port}/{}", self.shared.token);
        let mut serving = JoinSet::new();
        serving.spawn(accept(listener, self.shared.clone()));
        *server = Some(Server { base: base.clone(), _serving: serving });
        Ok(base)
    }
}

/// The pages saved in `folder`, by name.
fn names_in(folder: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut names: Vec<String> =
        entries.filter_map(|entry| entry.ok()?.file_name().to_str()?.strip_suffix(".html").map(str::to_owned)).filter(|name| check_name(name).is_ok()).collect();
    names.sort();
    names
}

/// A page's name is its file's: lower-case words joined by `-`.
fn check_name(name: &str) -> Result<(), String> {
    let fits = (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !name.starts_with('-') && !name.ends_with('-');
    if fits { Ok(()) } else { Err(format!("a page's name is lower-case words joined by -, like build-times, at most 64 letters: not {name:?}")) }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    // Each connection is a task of this one's: they stop with it.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => {
                    connections.spawn(answer(socket, shared.clone()));
                }
                Err(_) => return,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

/// What a request asks for, under the token.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    List,
    Page(String),
    Events(String),
    Nothing,
}

/// What `target` asks for, if it's under `token`.
fn asked(target: &str, token: &str) -> Asked {
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let Some(rest) = path.strip_prefix('/').and_then(|rest| rest.strip_prefix(token)) else { return Asked::Nothing };
    let named = |name: &str, made: fn(String) -> Asked| if check_name(name).is_ok() { made(name.to_owned()) } else { Asked::Nothing };
    match rest {
        "" | "/" => Asked::List,
        _ => match rest.strip_prefix('/') {
            Some(file) if file.ends_with(".html") => named(file.trim_end_matches(".html"), Asked::Page),
            Some(file) if file.ends_with(".events") => named(file.trim_end_matches(".events"), Asked::Events),
            _ => Asked::Nothing,
        },
    }
}

/// Answers one request, then closes, unless it's a page's events, which
/// stay open as long as the page does.
async fn answer(socket: TcpStream, shared: Arc<Shared>) {
    let (read, mut write) = socket.into_split();
    let mut read = BufReader::new(read);
    let Ok(Some(target)) = tokio::time::timeout(HEAD_TIMEOUT, request(&mut read)).await else { return };
    match asked(&target, &shared.token) {
        Asked::Events(name) => events(read, write, &shared, name).await,
        Asked::Page(name) => match tokio::fs::read_to_string(shared.folder.join(format!("{name}.html"))).await {
            Ok(page) => {
                let _ = reply(&mut write, 200, "text/html; charset=utf-8", with_reload(&page).as_bytes()).await;
            }
            Err(_) => {
                let _ = reply(&mut write, 404, "text/plain; charset=utf-8", b"There's no such page.").await;
            }
        },
        Asked::List => {
            let _ = reply(&mut write, 200, "text/html; charset=utf-8", listing(&names_in(&shared.folder)).as_bytes()).await;
        }
        Asked::Nothing => {
            let _ = reply(&mut write, 404, "text/plain; charset=utf-8", b"Not found.").await;
        }
    }
}

/// The target of a `GET`, once its head is read; `None` for anything else.
async fn request(read: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<String> {
    let mut head = (&mut *read).take(LONGEST_HEAD);
    let mut line = String::new();
    head.read_line(&mut line).await.ok()?;
    let mut words = line.split_whitespace();
    let (method, target) = (words.next()? == "GET", words.next()?.to_owned());
    loop {
        line.clear();
        if head.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        if line.trim_end().is_empty() {
            break;
        }
    }
    method.then_some(target)
}

/// `page` with the script that reloads it, before its `</body>`.
fn with_reload(page: &str) -> String {
    match page.to_ascii_lowercase().rfind("</body>") {
        Some(at) => format!("{}{RELOAD}{}", &page[..at], &page[at..]),
        None => format!("{page}{RELOAD}"),
    }
}

/// The list of a project's pages, for the bare address.
fn listing(names: &[String]) -> String {
    let items: String = names.iter().map(|name| format!("<li><a href=\"{name}.html\">{name}</a></li>")).collect();
    let items = if items.is_empty() { "<p>None yet.</p>".to_owned() } else { format!("<ul>{items}</ul>") };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Pages</title><style>:root{{color-scheme:light dark}}body{{font:16px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:40rem;padding:0 16px}}</style>\
         </head><body><h1>Pages</h1>{items}</body></html>"
    )
}

async fn reply(write: &mut tokio::net::tcp::OwnedWriteHalf, status: u16, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: {POLICY}\r\n\
         Referrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(body).await
}

/// A tab showing a page, counted while it's open: dropped, however its
/// stream ends, it's counted gone.
struct Tab {
    shared: Arc<Shared>,
    name: String,
}

impl Tab {
    fn open(shared: &Arc<Shared>, name: String) -> Tab {
        lock(&shared.tabs).entry(name.clone()).or_default().open += 1;
        Tab { shared: shared.clone(), name }
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        if let Some(tabs) = lock(&self.shared.tabs).get_mut(&self.name) {
            tabs.open = tabs.open.saturating_sub(1);
            tabs.left = Some(Instant::now());
        }
    }
}

/// A page's events: `saved` each time it's saved, until its tab goes.
async fn events(mut read: BufReader<tokio::net::tcp::OwnedReadHalf>, mut write: tokio::net::tcp::OwnedWriteHalf, shared: &Arc<Shared>, name: String) {
    let mut saved = shared.saved.subscribe();
    let _tab = Tab::open(shared, name.clone());
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n";
    if write.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING, PING);
    let mut sink = [0u8; 256];
    loop {
        let said = tokio::select! {
            got = saved.recv() => match got {
                Ok(page) if page != name => continue,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => "event: saved\ndata: \n\n",
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = ping.tick() => ": \n\n",
            // The browser closed the tab: it sends nothing else on this.
            closed = read.read(&mut sink) => match closed {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            },
        };
        if write.write_all(said.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// The page tool, as the model is told of it.
pub fn spec() -> rein_llm::Tool {
    rein_llm::Tool {
        grammar: None,
        loading: rein_llm::Loading::UpFront,
        strict: true,
        name: crate::tools::ToolName::Page.as_str().into(),
        description: "Make a page for the user to look at in their browser: a report, a diagram, a chart, a comparison, a \
                      small tool. Use it when something is clearer seen than read in a terminal, or the user asks for a \
                      page, a visual or an artifact; not for code that belongs in the project. It opens in their browser \
                      and reloads there each time you save it. save writes the whole page; edit changes one piece of it \
                      (old_text exactly once); read gives it back. There's no Artifact tool here: a skill that says to \
                      publish with one, or on claude.ai, means this tool, and the link it gives is the page's address.\n\
                      The page is one self-contained HTML file: a <title> of a few words; styles and scripts inline; \
                      scripts from cdnjs.cloudflare.com, cdn.jsdelivr.net/npm or unpkg.com only, each at an exact \
                      version (react@18.3.1, never react@18 or latest); fonts from Google Fonts only. It can't fetch \
                      from other servers, so put its data in it. Define colours as custom properties on :root, again \
                      for dark under @media (prefers-color-scheme: dark), and give body a background. It must work at \
                      phone width: a 16px gutter, nothing wider than the screen. Design it with care: a clear \
                      hierarchy, readable type, real content rather than placeholders."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["save", "edit", "read"], "description": "save writes the whole page (again, to replace it); edit replaces old_text with new_text; read gives the page as saved." },
                "name": { "type": "string", "description": "The page's name: lower-case words joined by -, like build-times. The same name is the same page." },
                "html": { "type": ["string", "null"], "description": "For save: the whole HTML document, from <!doctype html>. Null otherwise." },
                "old_text": { "type": ["string", "null"], "description": "For edit: text copied exactly from the page, found once. Null otherwise." },
                "new_text": { "type": ["string", "null"], "description": "For edit: what goes in its place. Null otherwise." }
            },
            "required": ["action", "name", "html", "old_text", "new_text"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-pages-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn call(action: &str, name: &str, html: Option<&str>, old: Option<&str>, new: Option<&str>) -> String {
        serde_json::json!({ "action": action, "name": name, "html": html, "old_text": old, "new_text": new }).to_string()
    }

    fn same(text: String) -> String {
        text
    }

    /// `GET path` on the server at `link`'s port, as it answers: its head
    /// and body together.
    async fn get(link: &str, path: &str) -> String {
        let port = link.trim_start_matches("http://127.0.0.1:").split('/').next().unwrap();
        let mut socket = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).await.unwrap();
        socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut said = String::new();
        socket.read_to_string(&mut said).await.unwrap();
        said
    }

    fn path_of(link: &str) -> String {
        format!("/{}", link.trim_start_matches("http://127.0.0.1:").split_once('/').unwrap().1)
    }

    #[tokio::test]
    async fn a_saved_page_is_served_under_its_token_with_what_reloads_it() {
        let pages = Pages::at(dir("serve")).unwrap();
        assert_eq!(pages.link("report"), None, "nothing is served before a page is saved");
        let said = pages.run(&call("save", "report", Some("<!doctype html><title>R</title><body><h1>Hi</h1></body>"), None, None), &same).await.unwrap();
        let link = pages.link("report").unwrap();
        assert_eq!(said.lines().take(2).collect::<Vec<_>>(), ["Made the page report.", link.as_str()]);

        let served = get(&link, &path_of(&link)).await;
        assert!(served.starts_with("HTTP/1.1 200 OK"), "{served}");
        assert!(served.contains("Referrer-Policy: no-referrer") && served.contains("connect-src 'self'"), "{served}");
        let body = served.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.contains("<h1>Hi</h1><script>") && body.ends_with("</script></body>"), "{body}");

        let wrong = path_of(&link).replace(&pages.shared.token, "guessed");
        assert!(get(&link, &wrong).await.starts_with("HTTP/1.1 404"));
        let listed = get(&link, &format!("/{}/", pages.shared.token)).await;
        assert!(listed.contains("<a href=\"report.html\">report</a>"), "{listed}");
    }

    #[test]
    fn the_token_is_short_and_never_the_same() {
        let (a, b) = (Pages::at(dir("token-a")).unwrap(), Pages::at(dir("token-b")).unwrap());
        assert_eq!((a.shared.token.len(), b.shared.token.len()), (TOKEN, TOKEN));
        assert_ne!(a.shared.token, b.shared.token);
    }

    #[test]
    fn only_a_page_under_the_token_is_asked_for() {
        let token = "tok";
        assert_eq!(asked("/tok/a-b.html", token), Asked::Page("a-b".into()));
        assert_eq!(asked("/tok/a-b.events?x", token), Asked::Events("a-b".into()));
        assert_eq!(asked("/tok/", token), Asked::List);
        for target in ["/tok/../keys.html", "/tok/A.html", "/tokx/a.html", "/a.html", "/tok/a/b.html", "/tok/.html", "/tok/a.htm"] {
            assert_eq!(asked(target, token), Asked::Nothing, "{target}");
        }
    }

    #[tokio::test]
    async fn a_name_that_isnt_one_writes_nothing() {
        let folder = dir("names");
        let pages = Pages::at(folder.clone()).unwrap();
        for name in ["../escape", "Report", "a/b", "", "-a", &"a".repeat(65)] {
            let said = pages.run(&call("save", name, Some("<p>x</p>"), None, None), &same).await;
            assert!(said.is_err(), "{name}: {said:?}");
        }
        assert!(!folder.exists() && !folder.with_file_name("escape.html").exists());
    }

    #[tokio::test]
    async fn an_edit_changes_the_one_place_it_names() {
        let pages = Pages::at(dir("edit")).unwrap();
        pages.run(&call("save", "p", Some("<p>one</p><p>two</p><p>two</p>"), None, None), &same).await.unwrap();
        assert!(pages.run(&call("edit", "p", None, Some("two"), Some("2")), &same).await.unwrap_err().contains("2 times"));
        assert!(pages.run(&call("edit", "p", None, Some("three"), Some("3")), &same).await.unwrap_err().contains("isn't in the page"));
        let said = pages.run(&call("edit", "p", None, Some("one"), Some("1")), &same).await.unwrap();
        assert!(said.starts_with("Saved the page p"), "{said}");
        assert_eq!(pages.run(&call("read", "p", None, None, None), &same).await.unwrap(), "<p>1</p><p>two</p><p>two</p>");
        assert_eq!(pages.run(&call("read", "q", None, None, None), &same).await.unwrap_err(), "there's no page called q; there are: p");
    }

    #[tokio::test]
    async fn keys_are_hidden_from_what_is_saved() {
        let pages = Pages::at(dir("keys")).unwrap();
        let hide = |text: String| text.replace("sk-secret", "[hidden]");
        pages.run(&call("save", "p", Some("<p>sk-secret</p>"), None, None), &hide).await.unwrap();
        assert_eq!(std::fs::read_to_string(pages.folder().join("p.html")).unwrap(), "<p>[hidden]</p>");
    }

    #[tokio::test]
    async fn an_open_page_is_told_when_its_saved_and_counts_as_shown() {
        let pages = Pages::at(dir("events")).unwrap();
        pages.run(&call("save", "p", Some("<p>1</p>"), None, None), &same).await.unwrap();
        pages.run(&call("save", "other", Some("<p>1</p>"), None, None), &same).await.unwrap();
        assert!(!pages.shown("p"));

        let link = pages.link("p").unwrap();
        let port: u16 = link.trim_start_matches("http://127.0.0.1:").split('/').next().unwrap().parse().unwrap();
        let mut tab = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let events = path_of(&link).replace(".html", ".events");
        tab.write_all(format!("GET {events} HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut tab = BufReader::new(tab);
        let mut line = String::new();
        tab.read_line(&mut line).await.unwrap();
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
        while line != "\r\n" {
            line.clear();
            tab.read_line(&mut line).await.unwrap();
        }
        assert!(pages.shown("p") && !pages.shown("other"));

        pages.run(&call("save", "other", Some("<p>2</p>"), None, None), &same).await.unwrap();
        pages.run(&call("edit", "p", None, Some("1"), Some("2")), &same).await.unwrap();
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), tab.read_line(&mut line)).await.unwrap().unwrap();
        assert_eq!(line, "event: saved\n", "told of its own save, not the other page's");

        drop(tab);
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock(&pages.shared.tabs).get("p").is_some_and(|tabs| tabs.open > 0) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(pages.shown("p"), "a tab that's just gone may be reloading");
        lock(&pages.shared.tabs).get_mut("p").unwrap().left = Instant::now().checked_sub(RELOADING);
        assert!(!pages.shown("p"));
    }

    #[tokio::test]
    async fn a_page_is_opened_once_while_its_browser_starts() {
        let pages = Pages::at(dir("open")).unwrap();
        assert_eq!(pages.to_open("p"), None, "nothing to open before it's served");
        pages.run(&call("save", "p", Some("<p>1</p>"), None, None), &same).await.unwrap();
        assert_eq!(pages.to_open("p"), pages.link("p"));
        assert_eq!(pages.to_open("p"), None, "saved again before the tab connected");
        lock(&pages.shared.tabs).get_mut("p").unwrap().opened = Instant::now().checked_sub(RELOADING);
        assert_eq!(pages.to_open("p"), pages.link("p"), "the tab never came, or was closed: open it again");
    }

    /// Skills written for Claude Code (Anthropic's `project-artifact`) say
    /// to publish with its Artifact tool, which rein hasn't: the model is
    /// told this is that.
    #[test]
    fn a_skill_that_publishes_an_artifact_is_pointed_here() {
        let description = spec().description;
        assert!(description.contains("There's no Artifact tool here: a skill that says to publish with one"), "{description}");
    }

    #[test]
    fn what_shows_the_conversation_knows_which_page_was_saved() {
        assert_eq!(Pages::saved_by(&call("save", " p ", Some("x"), None, None)).as_deref(), Some("p"));
        assert_eq!(Pages::saved_by(&call("edit", "p", None, Some("a"), Some("b"))).as_deref(), Some("p"));
        assert_eq!(Pages::saved_by(&call("read", "p", None, None, None)), None);
    }
}
