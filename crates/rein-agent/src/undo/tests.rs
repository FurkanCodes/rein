use super::*;

/// Nothing hidden.
const NOTHING: Hides = &|_: &Path| false;

/// A project of its own with `files` in it, and a folder for its copies.
fn place(name: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("rein-undo-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (project, copies) = (base.join("project"), base.join("copies"));
    std::fs::create_dir_all(&project).unwrap();
    for (path, text) in files {
        write(&project, path, text);
    }
    (project.canonicalize().unwrap(), copies)
}

fn write(project: &Path, path: &str, text: &str) {
    let path = project.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(project: &Path, path: &str) -> String {
    std::fs::read_to_string(project.join(path)).unwrap_or_else(|e| format!("({e})"))
}

/// Following `project`, with copies in `copies`, in the conversation `session`.
fn undo(project: &Path, copies: &Path, session: &str) -> Undo {
    let mut undo = Undo::new(project.to_owned());
    undo.keep_in(copies.to_owned());
    undo.open(session);
    undo
}

/// `work` done as a command is, looked at before and after.
fn command(undo: &mut Undo, work: impl FnOnce()) -> Changed {
    assert_eq!(undo.before_all(NOTHING), None);
    work();
    undo.after_all(NOTHING)
}

fn names(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|&path| path.to_owned()).collect()
}

#[test]
fn what_a_command_did_is_found_and_put_back_mode_and_all() {
    let (project, copies) = place("command", &[("a.rs", "one\n"), ("gone.txt", "keep me\n"), ("run.sh", "echo hi\n")]);
    #[cfg(unix)]
    std::fs::set_permissions(project.join("run.sh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("tidy it up", NOTHING);
    let changed = command(&mut undo, || {
        write(&project, "a.rs", "two\n");
        std::fs::remove_file(project.join("gone.txt")).unwrap();
        std::fs::remove_file(project.join("run.sh")).unwrap();
        write(&project, "src/new.rs", "made\n");
    });
    assert_eq!((changed.changed.clone(), changed.made.clone(), changed.removed.clone()), (names(&["a.rs"]), names(&["src/new.rs"]), names(&["gone.txt", "run.sh"])));
    assert!(changed.unkept.is_empty());
    let listed = undo.listed(NOTHING);
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].said.as_str(), listed[0].changed, listed[0].made, listed[0].removed), ("tidy it up", 1, 1, 2));
    let plan = undo.plan(listed[0].turn, NOTHING);
    assert_eq!((plan.put_back.clone(), plan.take_away.clone()), (names(&["a.rs", "gone.txt", "run.sh"]), names(&["src/new.rs"])));

    let done = undo.undo(listed[0].turn, NOTHING);
    assert_eq!(done.put_back, plan.put_back);
    assert!(done.failed.is_empty(), "{:?}", done.failed);
    assert_eq!((read(&project, "a.rs"), read(&project, "gone.txt")), ("one\n".to_owned(), "keep me\n".to_owned()));
    assert!(!project.join("src/new.rs").exists());
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(project.join("run.sh")).unwrap().permissions()) & 0o777, 0o755, "its mode too");
    assert!(undo.listed(NOTHING).is_empty(), "undone, and gone from the list");
}

#[test]
fn what_changed_between_turns_isnt_reins() {
    let (project, copies) = place("between", &[("a.rs", "one\n")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("look", NOTHING);
    assert!(command(&mut undo, || {}).is_empty());
    // You, in your editor, before the next turn.
    write(&project, "a.rs", "yours\n");
    undo.turn("look again", NOTHING);
    assert!(command(&mut undo, || {}).is_empty());
    assert!(undo.listed(NOTHING).is_empty());
}

#[test]
fn a_file_changed_since_rein_did_is_left_as_it_is() {
    let (project, copies) = place("since", &[("a.rs", "one\n"), ("b.rs", "one\n")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("change both", NOTHING);
    command(&mut undo, || {
        write(&project, "a.rs", "two\n");
        write(&project, "b.rs", "two\n");
    });
    write(&project, "b.rs", "yours\n");
    let done = undo.undo(0, NOTHING);
    assert_eq!((done.put_back, done.left), (names(&["a.rs"]), names(&["b.rs"])));
    assert_eq!((read(&project, "a.rs"), read(&project, "b.rs")), ("one\n".to_owned(), "yours\n".to_owned()));
}

#[test]
fn undoing_an_earlier_turn_undoes_the_later_ones_too() {
    let (project, copies) = place("earlier", &[("a.rs", "one\n")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("first", NOTHING);
    command(&mut undo, || write(&project, "a.rs", "two\n"));
    undo.turn("second", NOTHING);
    command(&mut undo, || write(&project, "a.rs", "three\n"));
    let listed = undo.listed(NOTHING);
    assert_eq!(listed.iter().map(|turn| turn.said.as_str()).collect::<Vec<_>>(), ["second", "first"]);
    let done = undo.undo(listed[1].turn, NOTHING);
    assert_eq!((done.turns, done.said.as_str()), (2, "first"));
    assert_eq!(read(&project, "a.rs"), "one\n");
}

#[test]
fn a_file_one_turn_made_and_a_later_one_changed_is_taken_away() {
    let (project, copies) = place("made-then-changed", &[("a.rs", "one\n")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("make it", NOTHING);
    command(&mut undo, || write(&project, "new.rs", "made\n"));
    undo.turn("change it", NOTHING);
    command(&mut undo, || write(&project, "new.rs", "changed\n"));
    let done = undo.undo(0, NOTHING);
    assert_eq!((done.take_away, done.put_back), (names(&["new.rs"]), Vec::<String>::new()));
    assert!(!project.join("new.rs").exists());
}

#[test]
fn a_conversation_keeps_its_last_turns_that_changed_something() {
    let (project, copies) = place("thirty", &[("a.rs", "0")]);
    let mut undo = undo(&project, &copies, "s1");
    for turn in 0..=TURNS {
        undo.turn(&format!("turn {turn}"), NOTHING);
        command(&mut undo, || write(&project, "a.rs", &turn.to_string()));
    }
    // Turns that change nothing don't push those out.
    for _ in 0..=TURNS {
        undo.turn("only looking", NOTHING);
        command(&mut undo, || {});
    }
    let listed = undo.listed(NOTHING);
    assert_eq!(listed.len(), TURNS, "the oldest went");
    assert_eq!((listed[0].said.as_str(), listed[TURNS - 1].said.as_str()), ("turn 30", "turn 1"));
}

#[test]
fn a_conversation_continued_after_a_week_keeps_its_own_copies() {
    let (project, copies) = place("week", &[("a.rs", "one\n")]);
    let mut first = undo(&project, &copies, "s1");
    first.turn("change it", NOTHING);
    command(&mut first, || write(&project, "a.rs", "two\n"));
    drop(first);
    let week = SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 60 * 60);
    std::fs::File::options().write(true).open(copies.join("conversation-s1/turns.json")).unwrap().set_modified(week).unwrap();
    let mut again = undo(&project, &copies, "s1");
    let listed = again.listed(NOTHING);
    assert_eq!(listed.len(), 1, "its own turns are still there");
    assert_eq!(again.undo(listed[0].turn, NOTHING).put_back, ["a.rs"]);
    assert_eq!(read(&project, "a.rs"), "one\n");
}

#[test]
fn only_the_last_ten_other_conversations_keep_their_copies() {
    let (project, copies) = place("ten", &[]);
    for other in 0..12 {
        let mut theirs = undo(&project, &copies, &format!("other{other:02}"));
        theirs.turn("theirs", NOTHING);
        command(&mut theirs, || write(&project, &format!("o{other}.rs"), "x"));
        // One after another, a moment apart.
        let at = SystemTime::now() - std::time::Duration::from_secs(100 - other);
        std::fs::File::options().write(true).open(copies.join(format!("conversation-other{other:02}/turns.json"))).unwrap().set_modified(at).unwrap();
    }
    let mut last = undo(&project, &copies, "last");
    last.listed(NOTHING);
    let mut kept: Vec<String> = std::fs::read_dir(&copies).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    kept.sort();
    assert_eq!(kept.len(), 10, "{kept:?}");
    assert_eq!(kept[0], "conversation-other02", "the two oldest went");
}

#[test]
fn a_file_too_big_to_keep_is_said_and_never_put_back() {
    let (project, copies) = place("big", &[("small.txt", "s\n")]);
    std::fs::File::create(project.join("big.bin")).unwrap().set_len(BIGGEST + 1).unwrap();
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("clean", NOTHING);
    let changed = command(&mut undo, || std::fs::remove_file(project.join("big.bin")).unwrap());
    assert_eq!((changed.removed.clone(), changed.unkept.clone()), (names(&["big.bin"]), names(&["big.bin"])));
    let plan = undo.plan(0, NOTHING);
    assert_eq!((plan.lost.clone(), plan.does_anything()), (names(&["big.bin"]), false));
}

#[test]
fn what_gitignore_leaves_out_isnt_followed() {
    let (project, copies) = place("ignored", &[(".gitignore", "target/\n"), ("target/out.o", "built\n"), ("a.rs", "one\n")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("build", NOTHING);
    let changed = command(&mut undo, || std::fs::remove_dir_all(project.join("target")).unwrap());
    assert!(changed.is_empty(), "{changed:?}");
}

#[test]
fn a_stopped_command_is_the_turn_it_ran_in() {
    let (project, copies) = place("stopped", &[("a.rs", "one\n")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("go", NOTHING);
    assert_eq!(undo.before_all(NOTHING), None);
    write(&project, "a.rs", "half done\n");
    // Esc: no look after. The next turn starts.
    undo.turn("next", NOTHING);
    let listed = undo.listed(NOTHING);
    assert_eq!((listed.len(), listed[0].said.as_str()), (1, "go"));
}

#[test]
fn made_and_gone_within_a_turn_leaves_nothing_to_undo() {
    let (project, copies) = place("made-gone", &[("a.rs", "one\n")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("go", NOTHING);
    command(&mut undo, || write(&project, "tmp.txt", "x\n"));
    command(&mut undo, || std::fs::remove_file(project.join("tmp.txt")).unwrap());
    assert!(undo.listed(NOTHING).is_empty());
}

#[test]
fn what_was_kept_is_there_when_rein_starts_again() {
    let (project, copies) = place("again", &[("a.rs", "one\n")]);
    let mut first = undo(&project, &copies, "s1");
    first.turn("change it", NOTHING);
    command(&mut first, || write(&project, "a.rs", "two\n"));
    drop(first);
    let mut again = undo(&project, &copies, "s1");
    let listed = again.listed(NOTHING);
    assert_eq!(listed.len(), 1);
    assert_eq!(again.undo(listed[0].turn, NOTHING).put_back, ["a.rs"]);
    assert_eq!(read(&project, "a.rs"), "one\n");
    // Another conversation's turns are its own.
    let mut other = undo(&project, &copies, "s2");
    assert!(other.listed(NOTHING).is_empty());
}

#[test]
fn another_conversation_clearing_out_never_takes_this_ones_copies() {
    let (project, copies) = place("two", &[("a.rs", "one\n")]);
    let mut mine = undo(&project, &copies, "s1");
    mine.turn("change it", NOTHING);
    command(&mut mine, || write(&project, "a.rs", "two\n"));
    // A month-old conversation, gone over by the next rein to start.
    let old = copies.join("conversation-old");
    std::fs::create_dir_all(&old).unwrap();
    let turns = std::fs::File::create(old.join("turns.json")).unwrap();
    turns.set_modified(SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 60 * 60)).unwrap();
    let mut theirs = undo(&project, &copies, "s2");
    theirs.turn("their turn", NOTHING);
    command(&mut theirs, || write(&project, "b.rs", "theirs\n"));
    theirs.turn("and on", NOTHING);
    assert!(!old.exists(), "the old one swept away");
    assert_eq!(mine.undo(0, NOTHING).put_back, ["a.rs"]);
    assert_eq!(read(&project, "a.rs"), "one\n");
}

#[test]
fn a_symlink_swapped_in_is_never_kept() {
    let (project, copies) = place("swapped", &[("a.rs", "one\n")]);
    let secret = project.parent().unwrap().join("secret.txt");
    std::fs::write(&secret, "the key\n").unwrap();
    let mut undo = undo(&project, &copies, "s1");
    let seen = undo.now("a.rs").unwrap();
    std::fs::remove_file(project.join("a.rs")).unwrap();
    std::os::unix::fs::symlink(&secret, project.join("a.rs")).unwrap();
    assert_eq!(undo.kept("a.rs", seen).copy, None, "never through the link");
}

#[test]
fn a_project_with_too_many_files_isnt_followed_and_says_why_once() {
    let (project, copies) = place("many", &[("a.rs", "1"), ("b.rs", "2"), ("c.rs", "3")]);
    let mut undo = undo(&project, &copies, "s1");
    undo.most = (2, MOST_BYTES);
    undo.turn("go", NOTHING);
    assert_eq!(undo.before_all(NOTHING), Some(Off::Files(3)));
    assert_eq!(undo.before_all(NOTHING), None, "once");
    std::fs::remove_file(project.join("a.rs")).unwrap();
    assert!(undo.after_all(NOTHING).is_empty());
    // What rein's own tools change is still kept.
    undo.before_one("b.rs");
    write(&project, "b.rs", "changed");
    undo.after_one("b.rs");
    assert_eq!(undo.undo(0, NOTHING).put_back, ["b.rs"]);
    assert_eq!(read(&project, "b.rs"), "2");
}

#[test]
fn without_a_folder_only_reins_own_changes_are_followed() {
    let (project, _) = place("memory", &[("a.rs", "one\n")]);
    let mut undo = Undo::new(project.clone());
    undo.turn("go", NOTHING);
    assert_eq!(undo.before_all(NOTHING), None);
    write(&project, "a.rs", "by a command\n");
    assert!(undo.after_all(NOTHING).is_empty());
    undo.before_one("a.rs");
    write(&project, "a.rs", "by edit\n");
    undo.after_one("a.rs");
    assert_eq!(undo.undo(0, NOTHING).put_back, ["a.rs"]);
    assert_eq!(read(&project, "a.rs"), "by a command\n");
}

/// How long looking takes in a project as big as rein follows: the first
/// time, when every file is copied, and after, when only what changed is.
#[test]
#[ignore = "measures: run with --nocapture to see the times"]
fn how_long_looking_takes_at_the_most_files() {
    let (project, copies) = place("timing", &[]);
    for folder in 0..200 {
        for file in 0..(MOST_FILES / 200) {
            write(&project, &format!("src/m{folder}/f{file}.rs"), &format!("// {folder} {file}\n{}\n", "fn x() {}\n".repeat(20)));
        }
    }
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("time it", NOTHING);
    let time = |undo: &mut Undo, what: &str, work: &mut dyn FnMut(&mut Undo)| {
        let start = std::time::Instant::now();
        work(undo);
        eprintln!("{what}: {:?}", start.elapsed());
    };
    time(&mut undo, "first look, every file copied", &mut |undo| assert_eq!(undo.before_all(NOTHING), None));
    time(&mut undo, "looking after, nothing changed", &mut |undo| assert!(undo.after_all(NOTHING).is_empty()));
    write(&project, "src/m1/f1.rs", "changed\n");
    time(&mut undo, "a look before, one file changed", &mut |undo| assert_eq!(undo.before_all(NOTHING), None));
    let used = std::process::Command::new("du").args(["-sk"]).arg(&copies).output().unwrap();
    eprintln!("copies take: {}", String::from_utf8_lossy(&used.stdout).trim());
    let used = std::process::Command::new("du").args(["-sk"]).arg(&project).output().unwrap();
    eprintln!("the project takes: {}", String::from_utf8_lossy(&used.stdout).trim());
    std::fs::remove_dir_all(project.parent().unwrap()).unwrap();
}

/// On APFS a copy is a clone: keeping 500 MB of files takes next to no room.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "measures: writes 500 MB, run with --nocapture to see the room taken"]
fn copies_are_clones_that_take_no_room() {
    let (project, copies) = place("room", &[]);
    let block = "x".repeat(5 << 20);
    for file in 0..100 {
        write(&project, &format!("data/{file}.bin"), &block);
    }
    let free = || {
        let out = std::process::Command::new("df").args(["-k"]).arg(&project).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().nth(1).and_then(|line| line.split_whitespace().nth(3)).and_then(|kb| kb.parse::<i64>().ok()).unwrap()
    };
    let before = free();
    let mut undo = undo(&project, &copies, "s1");
    undo.turn("keep it", NOTHING);
    assert_eq!(undo.before_all(NOTHING), None);
    let taken = before - free();
    eprintln!("500 MB of files kept, taking {taken} KB");
    assert!(taken < 50 << 10, "copies took {taken} KB");
    std::fs::remove_dir_all(project.parent().unwrap()).unwrap();
}
