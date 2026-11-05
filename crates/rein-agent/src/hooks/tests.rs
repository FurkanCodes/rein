use super::*;
use std::path::PathBuf;

/// A folder for a test, outside the temp folders, which every command in
/// the sandbox may read and write, as a real project or home isn't.
fn dir(name: &str) -> PathBuf {
    let base = std::env::var_os("REIN_TEST_DIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rein-tests"), PathBuf::from);
    let dir = base.join(format!("hooks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A project and rein folder of their own, with `hooks` as rein's file has
/// them: `{ "Event": [groups] }`.
fn hooks(name: &str, hooks: Value) -> (Hooks, PathBuf) {
    let dir = dir(name);
    std::fs::create_dir_all(dir.join("rein")).unwrap();
    std::fs::create_dir_all(dir.join("project")).unwrap();
    std::fs::write(dir.join("rein/hooks.json"), json!({ "hooks": hooks }).to_string()).unwrap();
    (Hooks::load(&dir.join("rein"), None, &dir.join("project"), &Chosen::default(), &[]), dir.join("project"))
}

fn here(root: &Path, mode: Mode) -> Here<'_> {
    Here { root, session_id: Some("s1"), transcript: None, mode }
}

fn command(line: &str) -> Value {
    json!({ "type": "command", "command": line })
}

fn bash(command: &str) -> ToolCall {
    ToolCall { call_id: "c1".into(), name: "bash".into(), arguments: json!({ "command": command, "timeout_seconds": null, "access": "write", "reason": null }).to_string() }
}

fn command_of(call: &ToolCall) -> String {
    serde_json::from_str::<Value>(&call.arguments).unwrap()["command"].as_str().unwrap().to_owned()
}

/// A hook that puts `prefix` before the command it's given, as rtk does.
fn prefixing(prefix: &str) -> Value {
    command(&format!(r#"sed -e 's/.*"command":"\([^"]*\)".*/{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","updatedInput":{{"command":"{prefix} \1"}}}}}}/'"#))
}

#[tokio::test]
async fn hooks_on_a_tool_run_in_turn_each_given_the_call_as_the_last_left_it() {
    let (hooks, root) = hooks("chain", json!({ "PreToolUse": [{ "matcher": "Bash", "hooks": [prefixing("rtk"), prefixing("nice")] }] }));
    let heard = hooks.on(Event::PreToolUse(&bash("cargo test")), &here(&root, Mode::Trust)).await;
    let (call, by) = heard.call.clone().expect("rewritten");
    assert_eq!(command_of(&call), "nice rtk cargo test", "the second was given the first's rewrite: no race");
    assert_eq!(by, ["sed", "sed"]);
    assert_eq!(serde_json::from_str::<Value>(&call.arguments).unwrap()["access"], "write", "what rein has and a hook doesn't stays");
    assert!(heard.no.is_none() && heard.failed.is_empty(), "{heard:?}");
}

#[tokio::test]
async fn the_first_hook_that_says_no_stops_the_rest() {
    let refuses = command("echo 'not on main' >&2; exit 2");
    let (hooks, root) = hooks("no", json!({ "PreToolUse": [{ "hooks": [refuses, prefixing("rtk")] }] }));
    let heard = hooks.on(Event::PreToolUse(&bash("git push")), &here(&root, Mode::Trust)).await;
    assert_eq!(heard.no, Some(By { hook: "echo".into(), said: "not on main".into() }));
    assert_eq!(heard.ran, ["echo"], "the rest didn't run");
    assert!(heard.call.is_none());
}

#[tokio::test]
async fn a_hook_runs_for_what_its_matcher_and_its_if_say() {
    let (hooks, root) = hooks(
        "match",
        json!({ "PreToolUse": [
            { "matcher": "Edit|Write", "hooks": [command("echo edit")] },
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo git", "if": "Bash(git *)" }] }
        ] }),
    );
    let here = here(&root, Mode::Trust);
    assert!(hooks.on(Event::PreToolUse(&bash("ls")), &here).await.ran.is_empty(), "not an edit, not git");
    assert_eq!(hooks.on(Event::PreToolUse(&bash("cargo fmt && git commit -m x")), &here).await.ran, ["echo"], "a part of it is git");
    assert!(glob("git *", "git status") && !glob("git *", "gitk") && glob("npm run test:*", "npm run test -- --watch") && glob("src/**", "src/a/b.rs"));
}

#[tokio::test]
async fn what_a_message_hook_prints_is_context_for_the_model_cut_where_its_long() {
    let long = format!("printf '%{}s' x", MOST_SAID + 50);
    let (hooks, root) =
        hooks("context", json!({ "UserPromptSubmit": [{ "hooks": [command("cat >/dev/null; echo '<codegraph_context>a.rs</codegraph_context>'"), command(&long)] }] }));
    let heard = hooks.on(Event::UserPromptSubmit("how does a.rs work?"), &here(&root, Mode::Trust)).await;
    assert_eq!(heard.context[0].said, "<codegraph_context>a.rs</codegraph_context>");
    assert!(heard.context[1].said.ends_with("[50 more characters cut: a hook gives the model at most 10000]"), "said, not dropped");
    let said = note(&heard.context[0]);
    assert!(is_note(&said) && said.contains("in its own words, not the user's"), "{said}");
}

#[tokio::test]
async fn a_hook_is_told_what_happened_as_claude_code_tells_it() {
    let (hooks, root) = hooks("given", json!({ "PreToolUse": [{ "hooks": [command("cat > given.json")] }] }));
    hooks.on(Event::PreToolUse(&bash("cargo test")), &here(&root, Mode::Trust)).await;
    let given: Value = serde_json::from_str(&std::fs::read_to_string(root.join("given.json")).unwrap()).unwrap();
    assert_eq!(given["hook_event_name"], "PreToolUse");
    assert_eq!((given["tool_name"].clone(), given["tool_input"]["command"].clone()), (json!("Bash"), json!("cargo test")));
    assert_eq!((given["session_id"].clone(), given["permission_mode"].clone(), given["cwd"].clone()), (json!("s1"), json!("bypassPermissions"), json!(root.display().to_string())));
}

#[tokio::test]
async fn your_hooks_run_as_you_would_in_every_mode_reaching_their_own_scripts() {
    // As handoff-at-limit.py lives in ~/.claude/hooks, which the sandbox
    // hides from what the model runs: walled in, the hook couldn't open
    // its own script, and every message in read-only mode was turned away.
    let home = dir("own-home");
    std::fs::create_dir_all(home.join(".claude/hooks")).unwrap();
    std::fs::write(home.join(".claude/hooks/say.sh"), "echo \"ran in $PWD\"; echo made > made.txt\n").unwrap();
    let line = format!("sh {}/.claude/hooks/say.sh", home.display());
    let (hooks, root) = hooks("own", json!({ "UserPromptSubmit": [{ "hooks": [command(&line)] }] }));
    for mode in Mode::ALL {
        let heard = hooks.on(Event::UserPromptSubmit("hi"), &here(&root, mode)).await;
        assert!(heard.failed.is_empty() && heard.no.is_none(), "{mode:?}: {heard:?}");
        assert_eq!(heard.context[0].said, format!("ran in {}", root.display()), "{mode:?}");
        assert!(root.join("made.txt").exists(), "{mode:?}: in the project, as you'd run it there");
        std::fs::remove_file(root.join("made.txt")).unwrap();
    }
}
