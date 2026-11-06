//! `install.sh`, run as people run it, against a small server that answers
//! as GitHub's releases do, with the real `rein` in a real archive.

use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VERSION: &str = "9.8.7";

/// How the question about PATH ends.
const QUESTION: &str = "[Y/n]";

/// A folder of its own, gone with the test's temp folder when it's cleaned.
fn folder(name: &str) -> PathBuf {
    static MADE: AtomicU32 = AtomicU32::new(0);
    let made = MADE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rein-install-{}-{made}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A program that says what `body` says. Made by a process of its own, as
/// another test forking just then could keep it from running.
fn program(path: &Path, body: &str) {
    let text = path.with_extension("txt");
    std::fs::write(&text, format!("#!/bin/sh\n{body}\n")).unwrap();
    assert!(Command::new("chmod").arg("755").arg(&text).status().unwrap().success());
    assert!(Command::new("cp").arg(&text).arg(path).status().unwrap().success());
}

/// The build for this computer, as a release names it.
fn this_target() -> String {
    let system = if cfg!(target_os = "macos") { "apple-darwin" } else { "unknown-linux-gnu" };
    format!("{}-{system}", std::env::consts::ARCH)
}

/// A release's files in a folder: the archive, with `rein` as `make` leaves
/// it, and checksums of it.
fn release_of(target: &str, make: impl Fn(&Path)) -> PathBuf {
    let dir = folder("release");
    let name = format!("rein-{VERSION}-{target}");
    std::fs::create_dir(dir.join(&name)).unwrap();
    make(&dir.join(&name));
    assert!(Command::new("tar").arg("-czf").arg(format!("{name}.tar.gz")).arg(&name).current_dir(&dir).status().unwrap().success());
    std::fs::remove_dir_all(dir.join(&name)).unwrap();
    let sum = format!("if command -v sha256sum >/dev/null; then sha256sum {name}.tar.gz; else shasum -a 256 {name}.tar.gz; fi > SHA256SUMS");
    assert!(Command::new("sh").arg("-c").arg(sum).current_dir(&dir).status().unwrap().success());
    dir
}

fn release() -> PathBuf {
    release_of(&this_target(), |dir| {
        std::fs::copy(env!("CARGO_BIN_EXE_rein"), dir.join("rein")).unwrap();
    })
}

/// A server that answers as GitHub's releases do, from `files`, sending
/// `/latest` on to `latest`. It keeps the paths it was asked for.
struct Releases {
    url: String,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Releases {
    fn serve(files: PathBuf, latest: &'static str) -> Releases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/releases", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                let mut request = [0; 4096];
                let read = socket.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).into_owned();
                let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
                log.lock().unwrap().push(path.clone());
                let file = path.strip_prefix(&format!("/releases/download/v{VERSION}/")).map(|file| files.join(file));
                let answer = if path == "/releases/latest" {
                    format!("HTTP/1.1 302 Found\r\nLocation: {latest}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
                } else if let Some(bytes) = file.and_then(|file| std::fs::read(file).ok()) {
                    let mut answer = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes();
                    answer.extend(bytes);
                    answer
                } else if path.contains("/tag/") || path == "/releases" {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                };
                let _ = socket.write_all(&answer);
            }
        });
        Releases { url, asked }
    }
}

const LATEST: &str = "/releases/tag/v9.8.7";

/// `install.sh` as `curl … | sh` runs it, into `place`, with `path` ahead
/// of the system's programs.
fn install(releases: &Releases, place: &Path, path: Option<&Path>) -> Output {
    let script = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh")).unwrap();
    let system = std::env::var("PATH").unwrap();
    let path = path.map_or(system.clone(), |first| format!("{}:{system}", first.display()));
    let mut sh = Command::new("sh")
        .env("REIN_RELEASES", &releases.url)
        .env("REIN_INSTALL_DIR", place)
        .env("HOME", folder("home"))
        .env("PATH", path)
        .env_remove("REIN_VERSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(&script).unwrap();
    sh.wait_with_output().unwrap()
}

/// How a run in a terminal went, and all it showed there.
struct Shown {
    ok: bool,
    text: String,
}

/// `install.sh` as `curl … | sh` runs it for someone at a terminal: the
/// script comes down a pipe, and the terminal shows it and takes `answer`.
/// A question with no `answer` ends the run, where a person would be left
/// looking at it.
fn in_a_terminal(releases: &Releases, place: &Path, home: &Path, answer: Option<&str>, set: impl FnOnce(&mut Command)) -> Shown {
    run_on_screen(releases, place, home, answer, true, set)
}

/// The same, where `own` says whether the terminal is the run's own to
/// read from, or only a screen for it to write to.
fn run_on_screen(releases: &Releases, place: &Path, home: &Path, answer: Option<&str>, own: bool, set: impl FnOnce(&mut Command)) -> Shown {
    let script = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh")).unwrap();
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty writes two descriptors into the ints we pass. Its
    // size is `*mut` on macOS and `*const` on Linux, and a raw pointer suits both.
    let opened = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut size) };
    assert_eq!(opened, 0, "can't open a pseudo-terminal");
    // SAFETY: openpty just gave us these, and nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for end in [&master, &slave] {
        // SAFETY: sets a flag on a descriptor we own. Another test's child
        // holding the terminal open would keep this one from ending.
        assert_ne!(unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) }, -1);
    }

    let mut command = Command::new("sh");
    command
        .env("REIN_RELEASES", &releases.url)
        .env("REIN_INSTALL_DIR", place)
        .env("HOME", home)
        .env("TERM", "xterm-256color")
        .env("SHELL", "/bin/zsh")
        .env_remove("REIN_VERSION")
        .env_remove("NO_COLOR")
        .env_remove("ZDOTDIR")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    if own {
        // SAFETY: only calls that are safe between fork and exec.
        unsafe {
            command.pre_exec(|| {
                // Its own session, with this terminal as its own, so that
                // /dev/tty is this. Standard error is the terminal in every test.
                libc::setsid();
                libc::ioctl(2, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
    } else {
        // SAFETY: only a call that's safe between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    set(&mut command);
    let mut sh = command.spawn().unwrap();
    drop(command);
    sh.stdin.take().unwrap().write_all(&script).unwrap();

    let shown = Arc::new(Mutex::new(Vec::new()));
    let log = shown.clone();
    let mut screen = File::from(master.try_clone().unwrap());
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = screen.read(&mut buf) {
            log.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let mut keyboard = File::from(master);
    let mut answer = answer;
    let mut answered = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = sh.try_wait().unwrap() {
            break status;
        }
        if !answered && String::from_utf8_lossy(&shown.lock().unwrap()).contains(QUESTION) {
            answered = true;
            match answer.take() {
                Some(keys) => keyboard.write_all(keys.as_bytes()).unwrap(),
                None => sh.kill().unwrap(),
            }
        }
        if Instant::now() > deadline {
            let _ = sh.kill();
            panic!("still running: {}", String::from_utf8_lossy(&shown.lock().unwrap()));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    reader.join().unwrap();
    let text = String::from_utf8_lossy(&shown.lock().unwrap()).into_owned();
    Shown { ok: status.success(), text }
}

/// The line that adds `place` to PATH, for all but fish.
fn line_for(place: &Path) -> String {
    format!("export PATH=\"{}:$PATH\"", place.display())
}

fn said(output: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

/// What's left in `place`, by name.
fn left(place: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(place).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn rein_says_its_version() {
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rein")).arg(flag).env("HOME", folder("version")).output().unwrap();
        assert!(output.status.success(), "{}", said(&output));
        assert_eq!(said(&output), format!("rein {}\n", env!("CARGO_PKG_VERSION")));
    }
}

#[test]
fn the_latest_build_for_this_computer_goes_in_and_runs_and_says_how_to_reach_it() {
    let releases = Releases::serve(release(), LATEST);
    let place = folder("place").join("new/bin");
    let output = install(&releases, &place, None);
    assert!(output.status.success(), "{}", said(&output));
    assert_eq!(left(&place), ["rein"], "nothing else stays behind");
    let ran = Command::new(place.join("rein")).arg("--version").env("HOME", folder("home")).output().unwrap();
    assert_eq!(said(&ran), format!("rein {}\n", env!("CARGO_PKG_VERSION")));
    let text = said(&output);
    assert!(text.contains(&format!("Downloading rein {VERSION} for {}", this_target())), "{text}");
    assert!(text.contains("isn't on your PATH yet") && text.contains(&format!("export PATH=\"{}:$PATH\"", place.display())), "{text}");
    // Run again, it updates what's there, and it's found on the PATH now.
    let output = install(&releases, &place, Some(&place));
    assert!(output.status.success() && said(&output).contains("Run `rein`"), "{}", said(&output));
    assert_eq!(left(&place), ["rein"]);
}

#[test]
fn a_build_that_doesnt_match_its_checksum_replaces_nothing() {
    let files = release();
    let sums = std::fs::read_to_string(files.join("SHA256SUMS")).unwrap();
    std::fs::write(files.join("SHA256SUMS"), format!("{}{}", "0".repeat(64), &sums[64..])).unwrap();
    let releases = Releases::serve(files, LATEST);
    let place = folder("place");
    std::fs::write(place.join("rein"), "the one that works").unwrap();
    let output = install(&releases, &place, None);
    assert!(!output.status.success() && said(&output).contains("doesn't match its checksum"), "{}", said(&output));
    assert_eq!(std::fs::read_to_string(place.join("rein")).unwrap(), "the one that works");
    assert_eq!(left(&place), ["rein"]);
}

#[test]
fn a_release_that_doesnt_list_the_build_replaces_nothing() {
    let files = release();
    std::fs::write(files.join("SHA256SUMS"), "").unwrap();
    let releases = Releases::serve(files, LATEST);
    let place = folder("place");
    let output = install(&releases, &place, None);
    assert!(!output.status.success() && said(&output).contains("doesn't list"), "{}", said(&output));
    assert!(left(&place).is_empty());
}

#[test]
fn a_build_that_cant_run_here_replaces_nothing() {
    let target = this_target();
    let files = release_of(&target, |dir| program(&dir.join("rein"), "echo version GLIBC_2.99 not found >&2\nexit 1"));
    let releases = Releases::serve(files, LATEST);
    let place = folder("place");
    std::fs::write(place.join("rein"), "the one that works").unwrap();
    let output = install(&releases, &place, None);
    let text = said(&output);
    assert!(!output.status.success() && text.contains("this build can't run here") && text.contains("GLIBC_2.99"), "{text}");
    assert_eq!(std::fs::read_to_string(place.join("rein")).unwrap(), "the one that works");
    assert_eq!(left(&place), ["rein"]);
}

#[test]
fn each_computer_gets_its_own_build_and_any_other_is_told() {
    let cases = [
        ("Darwin", "arm64", "aarch64-apple-darwin"),
        ("Darwin", "x86_64", "x86_64-apple-darwin"),
        ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
        ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
    ];
    for (system, chip, target) in cases {
        let releases = Releases::serve(folder("empty"), LATEST);
        let bin = folder("bin");
        program(&bin.join("uname"), &format!("case $1 in -s) echo {system};; *) echo {chip};; esac"));
        // Not under Rosetta, whatever this computer is.
        program(&bin.join("sysctl"), "echo 0");
        let output = install(&releases, &folder("place"), Some(&bin));
        let wanted = format!("/releases/download/v{VERSION}/rein-{VERSION}-{target}.tar.gz");
        assert!(releases.asked.lock().unwrap().contains(&wanted), "{target}: {:?} {}", releases.asked.lock().unwrap(), said(&output));
    }
    let refused = [("FreeBSD", "x86_64", "no build for FreeBSD"), ("MINGW64_NT", "x86_64", "use WSL"), ("Linux", "riscv64", "no build for riscv64")];
    for (system, chip, says) in refused {
        let releases = Releases::serve(folder("empty"), LATEST);
        let bin = folder("bin");
        program(&bin.join("uname"), &format!("case $1 in -s) echo {system};; *) echo {chip};; esac"));
        let output = install(&releases, &folder("place"), Some(&bin));
        assert!(!output.status.success() && said(&output).contains(says), "{}", said(&output));
    }
}

#[test]
fn a_mac_under_rosetta_gets_the_build_for_its_chip() {
    let releases = Releases::serve(folder("empty"), LATEST);
    let bin = folder("bin");
    program(&bin.join("uname"), "case $1 in -s) echo Darwin;; *) echo x86_64;; esac");
    program(&bin.join("sysctl"), "echo 1");
    install(&releases, &folder("place"), Some(&bin));
    assert!(releases.asked.lock().unwrap().iter().any(|path| path.ends_with("aarch64-apple-darwin.tar.gz")), "{:?}", releases.asked.lock().unwrap());
}

#[test]
fn with_no_release_yet_it_says_so() {
    let releases = Releases::serve(folder("empty"), "/releases");
    let place = folder("place");
    let output = install(&releases, &place, None);
    assert!(!output.status.success() && said(&output).contains("there's no release yet"), "{}", said(&output));
    assert!(left(&place).is_empty());
}

#[test]
fn at_a_terminal_it_shows_a_bar_and_colour_and_adds_the_folder_once_told_to() {
    let releases = Releases::serve(release(), LATEST);
    let (place, home) = (folder("place"), folder("home"));
    let zshrc = home.join(".zshrc");
    std::fs::write(&zshrc, "alias ll='ls -l'").unwrap();
    let run = in_a_terminal(&releases, &place, &home, Some("y\r"), |_| {});
    assert!(run.ok, "{}", run.text);
    assert!(run.text.contains("100.0%"), "the bar: {}", run.text);
    assert!(run.text.contains(&format!("\x1b[1;32mInstalled\x1b[0m rein {} in {}", env!("CARGO_PKG_VERSION"), place.display())), "{}", run.text);
    assert!(run.text.contains(&format!("Add it in {}? {QUESTION}", zshrc.display())), "{}", run.text);
    assert!(run.text.contains("Open a new terminal"), "{}", run.text);
    let text = std::fs::read_to_string(&zshrc).unwrap();
    assert!(text.starts_with("alias ll='ls -l'\n"), "what was there stays, on its own line: {text}");
    assert!(text.ends_with(&format!("{}\n", line_for(&place))), "{text}");
    // A shell that reads the file finds rein.
    let found = Command::new("sh").arg("-c").arg(format!(". '{}' && command -v rein", zshrc.display())).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&found.stdout).trim(), place.join("rein").display().to_string(), "{}", said(&found));
    // Run again from a terminal that hasn't read it yet: nothing to ask, and it's there once.
    let again = in_a_terminal(&releases, &place, &home, None, |_| {});
    assert!(again.ok && !again.text.contains(QUESTION) && again.text.contains("Open a new terminal"), "{}", again.text);
    assert_eq!(std::fs::read_to_string(&zshrc).unwrap().matches(&line_for(&place)).count(), 1);
}

#[test]
fn enter_or_yes_adds_the_folder_and_anything_else_changes_nothing() {
    let releases = Releases::serve(release(), LATEST);
    for answer in ["\r", "Y\r", "yes\r"] {
        let (place, home) = (folder("place"), folder("home"));
        let run = in_a_terminal(&releases, &place, &home, Some(answer), |_| {});
        assert!(run.ok, "{answer:?}: {}", run.text);
        assert!(std::fs::read_to_string(home.join(".zshrc")).unwrap().contains(&line_for(&place)), "{answer:?}: {}", run.text);
    }
    // Control-D, the last, is nobody answering.
    for answer in ["n\r", "no\r", "later\r", "\x04"] {
        let (place, home) = (folder("place"), folder("home"));
        let run = in_a_terminal(&releases, &place, &home, Some(answer), |_| {});
        assert!(run.ok, "{answer:?}: {}", run.text);
        assert!(left(&home).is_empty(), "{answer:?}: {:?}", left(&home));
        assert!(run.text.contains(&line_for(&place)), "says what to add itself: {}", run.text);
    }
}

#[test]
fn each_shell_gets_the_file_it_reads() {
    let cases: [(&str, &str, &[&str], &str); 6] = [
        ("Linux", "/usr/bin/zsh", &[], ".zshrc"),
        ("Linux", "/bin/bash", &[".bash_profile"], ".bashrc"),
        ("Darwin", "/bin/bash", &[], ".bash_profile"),
        ("Darwin", "/bin/bash", &[".bash_profile", ".profile"], ".bash_profile"),
        ("Darwin", "/bin/bash", &[".bash_login", ".profile"], ".bash_login"),
        ("Darwin", "/bin/bash", &[".profile"], ".profile"),
    ];
    for (system, shell, there, wanted) in cases {
        let suffix = if system == "Darwin" { "apple-darwin" } else { "unknown-linux-gnu" };
        let files = release_of(&format!("aarch64-{suffix}"), |dir| program(&dir.join("rein"), &format!("echo rein {VERSION}")));
        let releases = Releases::serve(files, LATEST);
        let (place, home, bin) = (folder("place"), folder("home"), folder("bin"));
        program(&bin.join("uname"), &format!("case $1 in -s) echo {system};; *) echo aarch64;; esac"));
        program(&bin.join("sysctl"), "echo 0");
        for name in there {
            std::fs::write(home.join(name), "").unwrap();
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let run = in_a_terminal(&releases, &place, &home, Some("\r"), |command| {
            command.env("SHELL", shell).env("PATH", path);
        });
        assert!(run.ok, "{system} {shell}: {}", run.text);
        let text = std::fs::read_to_string(home.join(wanted)).unwrap_or_default();
        assert!(text.contains(&line_for(&place)), "{system} {shell} {there:?} wants {wanted}: {:?}\n{}", left(&home), run.text);
        for name in there.iter().filter(|name| *name != &wanted) {
            assert_eq!(std::fs::read_to_string(home.join(name)).unwrap(), "", "{name} stays as it was");
        }
    }
}

#[test]
fn fish_and_a_moved_zsh_are_written_where_they_look_in_their_own_words() {
    let releases = Releases::serve(release(), LATEST);
    let (place, home) = (folder("place"), folder("home"));
    let run = in_a_terminal(&releases, &place, &home, Some("\r"), |command| {
        command.env("SHELL", "/opt/homebrew/bin/fish");
    });
    assert!(run.ok, "{}", run.text);
    let text = std::fs::read_to_string(home.join(".config/fish/config.fish")).unwrap_or_default();
    assert!(text.contains(&format!("set -gx PATH \"{}\" $PATH", place.display())), "{text}\n{}", run.text);

    let (place, home, moved) = (folder("place"), folder("home"), folder("zdotdir"));
    let run = in_a_terminal(&releases, &place, &home, Some("\r"), |command| {
        command.env("ZDOTDIR", &moved);
    });
    assert!(run.ok, "{}", run.text);
    assert!(std::fs::read_to_string(moved.join(".zshrc")).unwrap_or_default().contains(&line_for(&place)), "{}", run.text);
}

#[test]
fn a_shell_it_doesnt_know_is_shown_the_line_and_asked_nothing() {
    let releases = Releases::serve(release(), LATEST);
    let (place, home) = (folder("place"), folder("home"));
    let run = in_a_terminal(&releases, &place, &home, None, |command| {
        command.env("SHELL", "/usr/bin/nu");
    });
    assert!(run.ok && !run.text.contains(QUESTION), "{}", run.text);
    assert!(run.text.contains("your shell startup file") && run.text.contains(&line_for(&place)), "{}", run.text);
    assert!(left(&home).is_empty(), "{:?}", left(&home));
}

#[test]
fn with_no_shell_named_it_still_finishes() {
    let releases = Releases::serve(release(), LATEST);
    let (place, home) = (folder("place"), folder("home"));
    // macOS's sh fills in your login shell, and dash leaves it empty, so it
    // may or may not ask.
    let run = in_a_terminal(&releases, &place, &home, Some("n\r"), |command| {
        command.env_remove("SHELL");
    });
    assert!(run.ok && run.text.contains(&line_for(&place)), "{}", run.text);
    assert!(left(&home).is_empty(), "{:?}", left(&home));
}

#[test]
fn a_folder_that_could_run_something_is_never_offered_to_a_startup_file() {
    let releases = Releases::serve(release(), LATEST);
    let (place, home) = (folder("place").join("$(touch pwned)"), folder("home"));
    let run = in_a_terminal(&releases, &place, &home, None, |_| {});
    assert!(run.ok && !run.text.contains(QUESTION), "{}", run.text);
    assert!(left(&home).is_empty(), "{:?}", left(&home));
}

#[test]
fn a_relative_folder_goes_in_as_a_whole_path() {
    let releases = Releases::serve(release(), LATEST);
    let (work, home) = (folder("work"), folder("home"));
    let run = in_a_terminal(&releases, Path::new("bin/"), &home, Some("\r"), |command| {
        command.current_dir(&work);
    });
    assert!(run.ok, "{}", run.text);
    let place = work.canonicalize().unwrap().join("bin");
    assert!(std::fs::read_to_string(home.join(".zshrc")).unwrap_or_default().contains(&line_for(&place)), "{}", run.text);
}

#[test]
fn with_nobody_to_answer_it_asks_nothing() {
    let releases = Releases::serve(release(), LATEST);
    // Going to a log, from a terminal that could answer.
    let (place, home) = (folder("place"), folder("home"));
    let run = in_a_terminal(&releases, &place, &home, None, |command| {
        command.stdout(Stdio::null());
    });
    assert!(run.ok, "{}", run.text);
    assert!(left(&home).is_empty(), "{:?}", left(&home));
    // A screen, but no terminal of its own to read from.
    let (place, home) = (folder("place"), folder("home"));
    let run = run_on_screen(&releases, &place, &home, None, false, |_| {});
    assert!(run.ok && !run.text.contains(QUESTION), "{}", run.text);
    assert!(run.text.contains(&line_for(&place)), "{}", run.text);
    assert!(left(&home).is_empty(), "{:?}", left(&home));
}

#[test]
fn a_log_stays_plain() {
    let releases = Releases::serve(release(), LATEST);
    let output = install(&releases, &folder("place"), None);
    let text = said(&output);
    assert!(output.status.success() && text.contains("Installed rein"), "{text}");
    assert!(!text.contains('\x1b') && !text.contains("100.0%") && !text.contains(QUESTION), "{text}");
}

#[test]
fn colour_goes_with_no_color_or_a_terminal_that_cant_show_it() {
    let releases = Releases::serve(release(), LATEST);
    let run = in_a_terminal(&releases, &folder("place"), &folder("home"), Some("n\r"), |command| {
        command.env("NO_COLOR", "1");
    });
    assert!(run.ok && !run.text.contains('\x1b') && run.text.contains("100.0%"), "{}", run.text);
    let run = in_a_terminal(&releases, &folder("place"), &folder("home"), Some("n\r"), |command| {
        command.env("TERM", "dumb");
    });
    assert!(run.ok && !run.text.contains('\x1b'), "{}", run.text);
}

#[test]
fn a_failure_at_a_terminal_starts_in_red_and_in_a_log_it_doesnt() {
    let releases = Releases::serve(folder("empty"), "/releases");
    let run = in_a_terminal(&releases, &folder("place"), &folder("home"), None, |_| {});
    assert!(!run.ok && run.text.contains("\x1b[1;31mrein:\x1b[0m there's no release yet"), "{}", run.text);
    let output = install(&releases, &folder("place"), None);
    assert_eq!(said(&output), "rein: there's no release yet\n");
}

#[test]
fn a_startup_file_it_cant_write_leaves_the_install_done_and_shows_the_line() {
    let releases = Releases::serve(release(), LATEST);
    let (place, home) = (folder("place"), folder("home"));
    // A folder in the way of the file, which stops even a process that
    // can write anywhere, as a test's may.
    std::fs::create_dir(home.join(".zshrc")).unwrap();
    let run = in_a_terminal(&releases, &place, &home, Some("\r"), |_| {});
    assert!(run.ok && place.join("rein").exists(), "{}", run.text);
    assert!(run.text.contains("Can't write to") && run.text.contains(&line_for(&place)), "{}", run.text);
}
