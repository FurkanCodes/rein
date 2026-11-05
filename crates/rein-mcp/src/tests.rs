use super::*;
use std::path::Path;

/// A folder for a test, outside the temp folders, which every command in
/// the sandbox may read and write, as a real project or home isn't.
fn dir(name: &str) -> PathBuf {
    let base = std::env::var_os("REIN_TEST_DIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rein-tests"), PathBuf::from);
    let dir = base.join(format!("mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A project with the fake server in it, and a home folder beside it with
/// a secret, for servers started with `access`.
async fn fake(name: &str, access: Access) -> (Server, Launch, PathBuf) {
    fake_in(name, Walled::Set(access)).await
}

/// The fake server, `name`, in `walls`.
async fn fake_in(name: &str, walls: Walled) -> (Server, Launch, PathBuf) {
    let (root, home) = (dir(name), dir(&format!("{name}-home")));
    std::fs::write(root.join("server.sh"), crate::FAKE_SERVER).unwrap();
    std::fs::write(root.join("readme.txt"), "hello").unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_test"), "FAKE-mcp-secret").unwrap();
    let places = rein_sandbox::Places::new(&root, Some(&home), None);
    let sandbox = Arc::new(Sandbox::detect(places).await);
    let local = Local { command: "sh".into(), args: vec!["server.sh".into()], env: [("GIVEN".to_owned(), "hello there".to_owned())].into(), access: walls };
    let server = Server { name: "fake".into(), origin: Origin::Yours, reach: Reach::Local(local), allow: vec!["echo".into()], offered: Default::default() };
    (server, Launch { sandbox, root, caches: home.join(".cache/rein/mcp"), keys: Arc::default(), web: None, tokens: None }, home)
}

/// The command of `server`, to change.
fn local(server: &mut Server) -> &mut Local {
    match &mut server.reach {
        Reach::Local(local) => local,
        Reach::Remote(_) => panic!("{} is reached at a URL", server.name),
    }
}

/// `server` in other walls.
fn walled(mut server: Server, access: Access) -> Server {
    local(&mut server).access = Walled::Set(access);
    server
}

#[tokio::test]
async fn a_server_lists_its_tools_cleaned_and_answers_calls() {
    let (server, launch, _) = fake("lists", Access::Outside).await;
    let servers = Servers::start(vec![server], launch);
    let listing = servers.list().await;
    let names: Vec<&str> = listing.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["mcp__fake__echo", "mcp__fake__peek", "mcp__fake__given", "mcp__fake__has", "mcp__fake__quit"], "every page");
    let echo = &listing.tools[0];
    assert_eq!(echo.description, "Says it back.[2J", "escape codes taken out");
    assert_eq!(echo.parameters.get("$schema"), None);
    assert_eq!(listing.tools[2].parameters, json!({ "type": "object", "properties": {} }), "what providers want of an empty one");
    assert_eq!(listing.allowed, ["mcp__fake__echo"]);
    assert_eq!(listing.note.as_deref(), Some("MCP servers: fake (5 tools)"));
    assert_eq!(listing.instructions, [("fake".to_owned(), "Echo before peeking.[2J".to_owned())], "what it says of using it, cleaned");
    assert_eq!(servers.call("mcp__fake__echo", r#"{"text":"hi"}"#).await, Ok("hi".into()));
    assert_eq!(servers.call("mcp__fake__given", "{}").await, Ok("hello there".into()), "the environment it was given");
    let picture = servers.call("mcp__fake__echo", r#"{"text":"picture"}"#).await.unwrap();
    assert_eq!((picture.text.as_str(), picture.images.len(), picture.images[0].media_type.as_str()), ("a picture", 1, "image/png"), "an image handed on");
    assert!(picture.images[0].data.starts_with(b"\x89PNG"));
    assert!(servers.call("mcp__fake__echo", "[1]").await.unwrap_err().contains("bad arguments"));
    assert!(servers.call("mcp__fake__nothing", "{}").await.unwrap_err().contains("no tool called"));
    assert_eq!(servers.statuses()[0].1, Status::Ready(vec!["echo".into(), "peek".into(), "given".into(), "has".into(), "quit".into()]));
}

#[tokio::test]
async fn a_server_that_stops_says_why_and_is_started_again_on_its_next_call() {
    let (server, launch, _) = fake("again", Access::Outside).await;
    let servers = Servers::start(vec![server], launch);
    servers.list().await;
    let quit = servers.call("mcp__fake__quit", "{}").await.unwrap_err();
    assert!(quit.contains("it stopped, saying: quitting as asked"), "{quit}");
    while servers.statuses()[0].1 != Status::Stopped {
        tokio::task::yield_now().await;
    }
    let again = servers.call("mcp__fake__echo", r#"{"text":"back"}"#).await.unwrap().text;
    assert!(again.starts_with("back\n[rein: the fake server had stopped, so rein started it again"), "{again}");
    assert_eq!(servers.call("mcp__fake__echo", r#"{"text":"on"}"#).await, Ok("on".into()), "said once");
}

#[tokio::test]
async fn one_that_cant_start_says_why() {
    let (mut server, launch, _) = fake("fails", Access::Outside).await;
    local(&mut server).command = "rein-no-such-command".into();
    let mut exits = server.clone();
    exits.name = "exits".into();
    (local(&mut exits).command, local(&mut exits).args) = ("sh".into(), vec!["-c".into(), "echo 'Error: no TOKEN' >&2; exit 1".into()]);
    let mut missing = server.clone();
    missing.name = "missing".into();
    local(&mut missing).env = [("T".to_owned(), "${REIN_TEST_UNSET_VARIABLE}".to_owned())].into();
    let servers = Servers::start(vec![server, exits, missing], launch);
    let note = servers.list().await.note.unwrap();
    assert!(note.contains("fake didn't start: rein-no-such-command isn't installed here: it's not on your PATH"), "{note}");
    assert!(note.contains("exits didn't start: it stopped, saying: Error: no TOKEN"), "{note}");
    assert!(note.contains("missing didn't start: it needs REIN_TEST_UNSET_VARIABLE, which isn't set"), "{note}");
    assert!(note.ends_with(". /mcp says what to do"), "{note}");
    let failures: Vec<Status> = servers.statuses().into_iter().map(|(_, status)| status).collect();
    assert_eq!(failures[2], Status::Failed(Failure::Needs("REIN_TEST_UNSET_VARIABLE".into())), "what to ask for, by name, not in words to search: {failures:?}");
    assert!(matches!(&failures[1], Status::Failed(Failure::Other(why)) if why.contains("no TOKEN")), "outside the sandbox, the sandbox isn't why: {failures:?}");
}

/// A variable your environment hasn't is filled in from what you gave
/// rein, which is then hidden like any key.
#[tokio::test]
async fn a_variable_you_gave_rein_fills_in_what_your_environment_hasnt() {
    let (mut server, launch, _) = fake("given", Access::Outside).await;
    local(&mut server).env = [("GIVEN".to_owned(), "${REIN_TEST_GIVEN_KEY}".to_owned())].into();
    let servers = Servers::start(Vec::new(), launch);
    assert_eq!(server.needs(&|name| servers.keys().get(name)).as_deref(), Some("REIN_TEST_GIVEN_KEY"));
    servers.keys().set("REIN_TEST_GIVEN_KEY", "FAKE-given-key");
    assert_eq!(server.needs(&|name| servers.keys().get(name)), None);
    servers.add(server);
    servers.list().await;
    assert_eq!(servers.call("mcp__fake__given", "{}").await, Ok("FAKE-given-key".into()));
    assert_eq!(servers.secrets(), ["FAKE-given-key", "FAKE-given-key"], "what its environment got, and what you gave");
    assert_eq!(format!("{:?}", servers.keys()), r#"["REIN_TEST_GIVEN_KEY"]"#, "Debug names the variable, never its value");
}

#[tokio::test]
async fn a_server_in_the_sandbox_reads_the_project_but_not_your_home_folder() {
    let (server, launch, home) = fake("walls", Access::Network).await;
    if !launch.sandbox.available() {
        return;
    }
    let caches = launch.caches.clone();
    let servers = Servers::start(vec![server], launch);
    let listing = servers.list().await;
    assert_eq!(listing.tools.len(), 5, "{:?}", listing.note);
    let peek = |path: &Path| json!({ "path": path }).to_string();
    assert_eq!(servers.call("mcp__fake__peek", &peek(Path::new("readme.txt"))).await, Ok("read".into()));
    assert_eq!(servers.call("mcp__fake__peek", &peek(&home.join(".ssh/id_test"))).await, Ok("blocked".into()));
    assert!(caches.join("fake").is_dir(), "a folder of its own, for what npx fetches");
}

/// One with no walls of its own runs outside the sandbox, as servers do
/// in Codex and Claude Code, whatever the mode: it reads what a walled one
/// can't.
#[tokio::test]
async fn a_server_without_walls_of_its_own_runs_outside_the_sandbox() {
    let (server, launch, home) = fake_in("unset", Walled::Unset).await;
    let servers = Servers::start(vec![server], launch);
    servers.list().await;
    let peek = json!({ "path": home.join(".ssh/id_test") }).to_string();
    assert_eq!(servers.call("mcp__fake__peek", &peek).await, Ok("read".into()));
    assert_eq!(servers.walls("fake"), Some(Access::Outside));
}

/// A server in the sandbox that fails may fail for what it can't reach,
/// and say so only in its own words ("the screen is locked"): the model
/// is told, once, what it runs in, and how you'd let it out.
#[tokio::test]
async fn a_walled_servers_failed_call_says_once_that_the_sandbox_may_be_why() {
    let (server, launch, _) = fake("hint", Access::Network).await;
    if !launch.sandbox.available() {
        return;
    }
    let servers = Servers::start(vec![server.clone()], launch.clone());
    servers.list().await;
    let echo = |text: &str| json!({ "text": text }).to_string();
    assert_eq!(servers.call("mcp__fake__echo", &echo("fine")).await, Ok("fine".into()), "only a failed call");
    assert!(!servers.failed_in_walls("fake"));
    let first = servers.call("mcp__fake__echo", &echo("!no screen")).await.unwrap_err();
    assert!(first.starts_with("!no screen\n[rein: the fake server runs in the sandbox: the project and the internet"), "{first}");
    assert!(first.contains("other apps or the screen") && first.contains("let it out of the sandbox from /mcp"), "{first}");
    assert_eq!(servers.call("mcp__fake__echo", &echo("!no screen")).await, Err("!no screen".into()), "said once");
    assert!(servers.failed_in_walls("fake"), "for /mcp to offer to let it out");
    servers.add(server.clone());
    assert!(!servers.failed_in_walls("fake"), "started again, it's forgotten");

    let outside = Servers::start(vec![walled(server, Access::Outside)], launch);
    outside.list().await;
    assert_eq!(outside.call("mcp__fake__echo", &echo("!no such repo")).await, Err("!no such repo".into()), "nothing walls it in");
}

#[tokio::test]
async fn without_a_sandbox_one_meant_for_it_doesnt_start() {
    let (server, mut launch, _) = fake("nosandbox", Access::Network).await;
    launch.sandbox = Arc::new(Sandbox::missing(rein_sandbox::Places::default(), rein_sandbox::Missing::NotHere));
    let servers = Servers::start(vec![server], launch);
    let note = servers.list().await.note.unwrap();
    assert!(note.contains("there's no sandbox here to start it in"), "{note}");
    assert_eq!(servers.statuses()[0].1, Status::Failed(Failure::NoSandbox));
}

/// One that fails in the sandbox may fail for what the sandbox keeps from
/// it: that's said, so you can be offered to let it out, and outside it
/// starts.
#[tokio::test]
async fn one_that_fails_in_the_sandbox_says_so_and_starts_outside() {
    let (mut server, launch, home) = fake("inwalls", Access::Network).await;
    if !launch.sandbox.available() {
        return;
    }
    // It reads a file in your home folder as it starts, as one that keeps
    // its settings there does.
    (local(&mut server).command, local(&mut server).args) =
        ("sh".into(), vec!["-c".into(), format!("cat {} > /dev/null && exec sh server.sh", home.join(".ssh/id_test").display())]);
    let servers = Servers::start(vec![server.clone()], launch);
    let status = servers.started("fake").await;
    assert!(matches!(&status, Some(Status::Failed(Failure::InWalls(_)))), "{status:?}");
    servers.add(walled(server, Access::Outside));
    assert!(matches!(servers.started("fake").await, Some(Status::Ready(_))), "outside, it starts");
}

/// Whether the process `pid` is still there.
fn running(pid: &str) -> bool {
    std::process::Command::new("kill").args(["-0", pid]).stderr(std::process::Stdio::null()).status().is_ok_and(|status| status.success())
}

/// Whether the process `pid` is gone within a few seconds: until it's
/// reaped, one that was killed is still there.
async fn gone(pid: &str) -> bool {
    let start = std::time::Instant::now();
    while running(pid) && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    !running(pid)
}

#[tokio::test]
async fn a_server_added_or_removed_as_rein_runs_is_in_the_next_listing_or_stopped() {
    let (mut server, launch, _) = fake("changes", Access::Outside).await;
    let pidfile = launch.root.join("pid");
    local(&mut server).env.insert("PIDFILE".into(), pidfile.display().to_string());
    let servers = Servers::start(Vec::new(), launch);
    assert!(servers.list().await.tools.is_empty() && servers.list().await.note.is_none());
    servers.add(server.clone());
    let all = ["echo", "peek", "given", "has", "quit"].map(String::from).to_vec();
    assert_eq!(servers.started("fake").await, Some(Status::Ready(all.clone())));
    assert_eq!(servers.started("other").await, None);
    let listing = servers.list().await;
    assert_eq!((listing.tools.len(), listing.allowed.as_slice()), (5, ["mcp__fake__echo".to_owned()].as_slice()));
    servers.allow("fake", &["*".into()]);
    assert_eq!(servers.list().await.allowed.len(), 5, "all of its tools, from the next listing");
    assert_eq!(servers.call("mcp__fake__given", "{}").await, Ok("hello there".into()), "the same server, still running");
    let first = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();

    let mut other = server.clone();
    local(&mut other).env.insert("GIVEN".into(), "the other one".into());
    servers.add(other);
    assert!(servers.call("mcp__fake__given", "{}").await.unwrap_err().contains("no tool"), "the old one's tools aren't called once it's replaced");
    servers.list().await;
    assert_eq!(servers.call("mcp__fake__given", "{}").await, Ok("the other one".into()));
    let second = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    assert_ne!(first, second);
    assert!(gone(&first).await, "the one it replaced was stopped");

    assert!(servers.remove("fake") && !servers.remove("fake"));
    assert!(servers.call("mcp__fake__echo", r#"{"text":"hi"}"#).await.unwrap_err().contains("no tool"));
    assert!(servers.list().await.tools.is_empty() && servers.statuses().is_empty());
    assert!(gone(&second).await, "a removed server is stopped");
}

#[tokio::test]
async fn a_server_removed_while_it_starts_isnt_left_running() {
    let (mut server, launch, _) = fake("slow", Access::Outside).await;
    let pidfile = launch.root.join("pid");
    // It starts, and never answers.
    (local(&mut server).command, local(&mut server).args) = ("sh".into(), vec!["-c".into(), format!("echo $$ > {}; exec sleep 600", pidfile.display())]);
    let servers = Servers::start(vec![server], launch);
    while !pidfile.exists() || std::fs::read_to_string(&pidfile).unwrap().trim().is_empty() {
        tokio::task::yield_now().await;
    }
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    assert!(running(&pid));
    assert!(servers.remove("fake"));
    assert!(gone(&pid).await, "its start was stopped, and the server with it");
}

#[test]
fn names_say_whose_tool_it_is() {
    assert!(is_mcp("mcp__github__get_issue") && !is_mcp("read"));
    assert_eq!(parts("mcp__github__get_issue"), Some(("github", "get_issue")));
    assert_eq!(parts("mcp__github__get__issue"), Some(("github", "get__issue")), "a server's name has no __");
    let long = listed(&json!({ "name": "x", "description": "é".repeat(LONGEST_DESCRIPTION + 10) })).unwrap();
    assert_eq!(long.description.chars().count(), LONGEST_DESCRIPTION + 1, "cut, with a … to say so");
    assert!(listed(&json!({ "description": "no name" })).is_none());
}

/// A real server, fetched from npm as it starts, as most are: `npx` works
/// in the sandbox, with its cache in the server's own folder. Opt in, with
/// Node and the internet:
/// `cargo test -p rein-mcp -- --ignored --nocapture a_server_from_npm`.
#[tokio::test]
#[ignore = "fetches and runs a real server from npm"]
async fn a_server_from_npm_starts_in_the_sandbox() {
    let root = dir("npm");
    let home = std::env::home_dir().expect("a home folder");
    let sandbox = Arc::new(Sandbox::detect(rein_sandbox::Places::new(&root, Some(&home), None)).await);
    assert!(sandbox.available(), "no sandbox here");
    let local =
        Local { command: "npx".into(), args: vec!["-y".into(), "@modelcontextprotocol/server-everything".into()], env: Default::default(), access: Walled::Set(Access::Network) };
    let server = Server { name: "everything".into(), origin: Origin::Yours, reach: Reach::Local(local), allow: Vec::new(), offered: Default::default() };
    let launch = Launch { sandbox, root: root.clone(), caches: root.join("caches"), keys: Arc::default(), web: None, tokens: None };
    let servers = Servers::start(vec![server], launch);
    let listing = servers.list().await;
    println!("{:?}", listing.note);
    assert!(listing.tools.iter().any(|tool| tool.name == "mcp__everything__echo"), "{:?}", listing.note);
    let said = servers.call("mcp__everything__echo", r#"{"message":"from the sandbox"}"#).await;
    println!("{said:?}");
    assert!(said.unwrap().text.contains("from the sandbox"));
}

#[test]
fn a_servers_tools_are_held_back_once_theyd_take_more_than_a_little_of_every_request() {
    let tool = |words: usize| Tool { name: "mcp__s__t".into(), description: "word ".repeat(words), parameters: json!({"type": "object"}), held_back: false };
    let few = [tool(10), tool(10)];
    let many: Vec<Tool> = (0..70).map(|_| tool(120)).collect();
    assert!(!holds_back(Offered::BySize, &few), "a search would cost more than they do");
    assert!(holds_back(Offered::BySize, &many));
    assert!(holds_back(Offered::OnSearch, &few), "as your file says");
    assert!(!holds_back(Offered::UpFront, &many), "as your file says");
    assert!(!holds_back(Offered::OnSearch, &[]), "nothing to find");
}
