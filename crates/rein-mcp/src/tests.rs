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
    let (root, home) = (dir(name), dir(&format!("{name}-home")));
    std::fs::write(root.join("server.sh"), crate::FAKE_SERVER).unwrap();
    std::fs::write(root.join("readme.txt"), "hello").unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_test"), "FAKE-mcp-secret").unwrap();
    let places = rein_sandbox::Places::new(&root, Some(&home), None);
    let sandbox = Arc::new(Sandbox::detect(places).await);
    let server = Server {
        name: "fake".into(),
        origin: Origin::Yours,
        command: "sh".into(),
        args: vec!["server.sh".into()],
        env: [("GIVEN".to_owned(), "hello there".to_owned())].into(),
        access,
        allow: vec!["echo".into()],
    };
    (server, Launch { sandbox, root, caches: home.join(".cache/rein/mcp") }, home)
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
    assert_eq!(servers.call("mcp__fake__echo", r#"{"text":"hi"}"#).await, Ok("hi".into()));
    assert_eq!(servers.call("mcp__fake__given", "{}").await, Ok("hello there".into()), "the environment it was given");
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
    let again = servers.call("mcp__fake__echo", r#"{"text":"back"}"#).await.unwrap();
    assert!(again.starts_with("back\n[rein: the fake server had stopped, so rein started it again"), "{again}");
    assert_eq!(servers.call("mcp__fake__echo", r#"{"text":"on"}"#).await, Ok("on".into()), "said once");
}

#[tokio::test]
async fn one_that_cant_start_says_why() {
    let (mut server, launch, _) = fake("fails", Access::Outside).await;
    server.command = "rein-no-such-command".into();
    let mut exits = server.clone();
    (exits.name, exits.command, exits.args) = ("exits".into(), "sh".into(), vec!["-c".into(), "echo 'Error: no TOKEN' >&2; exit 1".into()]);
    let mut missing = server.clone();
    (missing.name, missing.env) = ("missing".into(), [("T".to_owned(), "${REIN_TEST_UNSET_VARIABLE}".to_owned())].into());
    let servers = Servers::start(vec![server, exits, missing], launch);
    let note = servers.list().await.note.unwrap();
    assert!(note.contains("fake didn't start: can't run rein-no-such-command"), "{note}");
    assert!(note.contains("exits didn't start: it stopped, saying: Error: no TOKEN"), "{note}");
    assert!(note.contains("missing didn't start: it needs REIN_TEST_UNSET_VARIABLE, which isn't set"), "{note}");
    assert!(servers.statuses().iter().all(|(_, status)| matches!(status, Status::Failed(_))));
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

#[tokio::test]
async fn without_a_sandbox_one_meant_for_it_doesnt_start() {
    let (server, mut launch, _) = fake("nosandbox", Access::Network).await;
    launch.sandbox = Arc::new(Sandbox::missing(rein_sandbox::Places::default(), rein_sandbox::Missing::NotHere));
    let servers = Servers::start(vec![server], launch);
    let note = servers.list().await.note.unwrap();
    assert!(note.contains("there's no sandbox here to start it in"), "{note}");
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
    let server = Server {
        name: "everything".into(),
        origin: Origin::Yours,
        command: "npx".into(),
        args: vec!["-y".into(), "@modelcontextprotocol/server-everything".into()],
        env: Default::default(),
        access: Access::Network,
        allow: Vec::new(),
    };
    let servers = Servers::start(vec![server], Launch { sandbox, root: root.clone(), caches: root.join("caches") });
    let listing = servers.list().await;
    println!("{:?}", listing.note);
    assert!(listing.tools.iter().any(|tool| tool.name == "mcp__everything__echo"), "{:?}", listing.note);
    let said = servers.call("mcp__everything__echo", r#"{"message":"from the sandbox"}"#).await;
    println!("{said:?}");
    assert!(said.unwrap().contains("from the sandbox"));
}
