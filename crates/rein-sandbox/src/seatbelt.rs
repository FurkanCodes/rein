//! macOS's sandbox, Seatbelt. Each command gets rules in Seatbelt's own
//! language (SBPL), handed with it to `/usr/bin/sandbox-exec`. They start
//! from "deny everything" and allow what running programs needs: a list of
//! what's allowed, since what a list of what's denied forgets is a way out.
//!
//! Paths go in as parameters (`-D NAME=path`), so none needs quoting.

use std::ffi::OsString;
use std::path::Path;

use super::{Places, Walls};

pub(super) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// What every command in the sandbox may do, whatever its access.
const BASE: &str = r#"(version 1)
(deny default)

; Running programs, and signalling the ones it started.
(allow process-exec process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)

; Reading files, and loading programs and libraries from them.
(allow file-read* file-map-executable)

; The devices every program uses.
(allow file-read* file-write-data file-ioctl (literal "/dev/null") (literal "/dev/zero") (literal "/dev/dtracehelper"))
(allow file-read* file-write-data (subpath "/dev/fd"))

; Not terminals, rein's own among them: what you type in one could be read,
; or typed into it.
(deny file-read* file-write* file-ioctl (literal "/dev/tty") (literal "/dev/ptmx") (regex #"^/dev/ttys[0-9]+$") (regex #"^/dev/pty"))

; Settings, logging, and looking up users: the system services a program
; needs, and no more. None that opens apps or sends Apple events, which
; would do things outside the sandbox for it.
(allow user-preference-read)
(allow ipc-posix-shm-read* (ipc-posix-name "apple.shm.notification_center") (ipc-posix-name-prefix "apple.cfprefs."))
(allow ipc-posix-sem)
(allow mach-lookup
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.cfprefsd.agent")
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.diagnosticd")
  (global-name "com.apple.logd")
  (global-name "com.apple.logd.events")
  (global-name "com.apple.system.logger")
  (global-name "com.apple.system.notification_center")
  (global-name "com.apple.system.DirectoryService.libinfo_v1")
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.system.opendirectoryd.membership"))
(allow network-outbound (literal "/private/var/run/syslog"))
"#;

/// With network access: the internet, and the services that look up names
/// and check certificates. Still no other program's socket: through
/// Docker's, say, a command could start a container that mounts your home.
const NETWORK: &str = r#"
; The internet, and servers of its own.
(allow network-outbound (remote ip))
(allow network-bind network-inbound (local ip))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow mach-lookup
  (global-name "com.apple.dnssd.service")
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.trustd")
  (global-name "com.apple.trustd.agent"))
"#;

/// The rules for a command within `walls`, and the `-D` parameters that
/// give them their paths. Later rules win over earlier ones, so what's
/// denied comes after what's allowed.
pub(super) fn profile(places: &Places, walls: Walls) -> (String, Vec<OsString>) {
    let mut params = Vec::new();
    let mut param = |path: &Path| {
        let name = format!("P{}", params.len());
        let mut value = OsString::from(format!("{name}="));
        value.push(path);
        params.push(value);
        format!("(param \"{name}\")")
    };
    let mut text = String::from(BASE);
    let subpaths = |paths: &[std::path::PathBuf], param: &mut dyn FnMut(&Path) -> String| paths.iter().map(|path| format!("(subpath {})", param(path))).collect::<Vec<_>>();

    text.push_str("\n; Writing: the temp folders, and with write access the project.\n");
    let temp = subpaths(places.temp(), &mut param);
    rule(&mut text, "allow file-write*", &temp);
    let project = subpaths(places.writable(), &mut param);
    let mut kept = Vec::new();
    if walls.write {
        rule(&mut text, "allow file-write*", &project);
    } else {
        // Looking changes nothing, even in a project inside a temp folder.
        kept.extend(project);
    }

    text.push_str("\n; Never written: what runs code later, the folders that hold it, and secrets.\n");
    kept.extend(subpaths(places.protected(), &mut param));
    kept.extend(places.locked().iter().map(|path| format!("(literal {})", param(path))));
    let hidden = subpaths(places.hidden(), &mut param);
    kept.extend(hidden.iter().cloned());
    rule(&mut text, "deny file-write*", &kept);
    text.push_str("\n; Never read: keys, logins and tokens.\n");
    rule(&mut text, "deny file-read*", &hidden);

    if walls.network {
        text.push_str(NETWORK);
    }
    (text, params)
}

/// `(what filter...)`, if there's any filter: a rule with none would apply
/// to everything.
fn rule(text: &mut String, what: &str, filters: &[String]) {
    if !filters.is_empty() {
        text.push_str(&format!("({what}\n  {})\n", filters.join("\n  ")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Access, Sandbox};
    use std::path::PathBuf;

    const LOOK: Walls = Walls { write: false, network: false };
    const WRITE: Walls = Walls { write: true, network: false };
    const NET: Walls = Walls { write: true, network: true };

    /// A folder for a test, in the build's own: not in a temp folder, which
    /// every command may write, as a real project isn't.
    fn dir(name: &str) -> PathBuf {
        let base = std::env::var_os("REIN_TEST_DIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rein-tests"), PathBuf::from);
        let dir = base.join(format!("seatbelt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::places::resolve(&dir)
    }

    /// The paths the rules starting with `at` name.
    fn value<'a>(params: &'a [OsString], text: &str, at: &str) -> Vec<&'a str> {
        let sections = text.split(at).skip(1).map(|rule| rule.split("\n(").next().unwrap_or_default());
        sections
            .flat_map(|section| section.split("(param \"").skip(1))
            .filter_map(|rest| rest.split('"').next())
            .filter_map(|name| params.iter().filter_map(|p| p.to_str()).find_map(|p| p.strip_prefix(&format!("{name}="))))
            .collect()
    }

    #[test]
    fn look_writes_only_the_temp_folders_and_write_the_project_too() {
        let root = dir("rules");
        let places = Places::new(&root, None, None);
        let root = root.to_str().unwrap();
        let (look, params) = profile(&places, LOOK);
        assert!(!value(&params, &look, "(allow file-write*").contains(&root), "{look}");
        assert!(value(&params, &look, "(deny file-write*").contains(&root), "denied outright, even in a temp folder: {look}");
        let (write, params) = profile(&places, WRITE);
        assert!(value(&params, &write, "(allow file-write*").contains(&root), "{write}");
        let denied = value(&params, &write, "(deny file-write*");
        assert!(denied.iter().any(|path| path.ends_with("/.git/hooks")), "{denied:?}");
        // Denials come last, so they win.
        assert!(write.find("(deny file-write*") > write.find("(allow file-write*"));
        assert!(!write.contains("(remote ip)") && profile(&places, NET).0.contains("(remote ip)"));
    }

    /// A project, a home with a secret in it, and a sandbox for both.
    struct Setup {
        root: PathBuf,
        home: PathBuf,
        outside: PathBuf,
        sandbox: Sandbox,
    }

    async fn setup(name: &str) -> Option<Setup> {
        // Seatbelt's walls; elsewhere `detect` finds another sandbox.
        if !cfg!(target_os = "macos") {
            return None;
        }
        let root = dir(name);
        let (home, outside) = (dir(&format!("{name}-home")), dir(&format!("{name}-outside")));
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join("readme.txt"), "hello\n").unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_test"), "secret\n").unwrap();
        std::fs::write(home.join("notes.txt"), "mine\n").unwrap();
        let sandbox = Sandbox::detect(Places::new(&root, Some(&home), None)).await;
        // Only where there's a sandbox to try: on macOS, outside one.
        sandbox.available().then_some(Setup { root, home, outside, sandbox })
    }

    /// Runs `line` in the project within `walls`: whether it worked, and what it said.
    async fn run(setup: &Setup, line: &str, walls: Walls) -> (bool, String) {
        let mut command = setup.sandbox.wrap(line, walls).expect("a sandbox, since setup found one");
        let output = command.current_dir(&setup.root).stdin(std::process::Stdio::null()).output().await.unwrap();
        (output.status.success(), format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
    }

    #[tokio::test]
    async fn look_reads_but_changes_nothing() {
        let Some(s) = setup("look").await else { return };
        assert_eq!(run(&s, "cat readme.txt", LOOK).await, (true, "hello\n".to_owned()));
        assert!(!run(&s, "echo x > new.txt", LOOK).await.0);
        assert!(!s.root.join("new.txt").exists());
        // The temp folders are for everyone.
        assert!(run(&s, "echo x > \"$TMPDIR/rein-seatbelt-look-$$\" && rm \"$TMPDIR/rein-seatbelt-look-$$\"", LOOK).await.0);
    }

    #[tokio::test]
    async fn write_changes_the_project_and_nothing_else() {
        let Some(s) = setup("write").await else { return };
        assert!(run(&s, "mkdir -p src && echo x > src/new.txt", WRITE).await.0);
        assert!(s.root.join("src/new.txt").exists());
        let outside = s.outside.join("planted.txt");
        assert!(!run(&s, &format!("echo x > '{}'", outside.display()), WRITE).await.0);
        assert!(!run(&s, &format!("echo x > '{}'", s.home.join(".zshrc").display()), WRITE).await.0);
        assert!(!outside.exists() && !s.home.join(".zshrc").exists());
    }

    #[tokio::test]
    async fn what_runs_later_cant_be_changed_even_round_about() {
        let Some(s) = setup("protected").await else { return };
        for line in [
            "echo x > .git/hooks/pre-commit",
            "echo x > .GIT/HOOKS/pre-commit",
            "echo '[core] fsmonitor = x' >> .git/config",
            "mkdir .rein && echo 'allow rm' > .rein/permissions",
            // Moving the folder out of the way, changing it, moving it back.
            "mv .git moved && echo x > moved/hooks/pre-commit",
            "ln -s .git/hooks h && echo x > h/pre-commit",
            "ln .git/config c",
        ] {
            let (worked, said) = run(&s, line, WRITE).await;
            assert!(!worked, "{line} worked: {said}");
        }
        assert!(!s.root.join(".git/hooks/pre-commit").exists() && !s.root.join(".rein").exists() && s.root.join(".git").is_dir());
    }

    #[tokio::test]
    async fn git_still_works_in_the_sandbox() {
        let Some(s) = setup("git").await else { return };
        let _ = std::fs::remove_dir_all(s.root.join(".git"));
        // Making a repository is making `.git`, which is locked: outside.
        assert!(!run(&s, "git init -q", WRITE).await.0);
        let made = std::process::Command::new("git").args(["init", "-q"]).current_dir(&s.root).status();
        if !made.is_ok_and(|status| status.success()) {
            return;
        }
        let (worked, said) = run(&s, "git add readme.txt && git -c user.email=a@b -c user.name=a commit -qm first && git log --oneline | wc -l", WRITE).await;
        assert!(worked, "{said}");
        assert_eq!(said.trim(), "1");
    }

    #[tokio::test]
    async fn secrets_in_the_home_folder_cant_be_read() {
        let Some(s) = setup("hidden").await else { return };
        assert!(!run(&s, &format!("cat '{}'", s.home.join(".ssh/id_test").display()), NET).await.0);
        assert!(run(&s, &format!("cat '{}'", s.home.join("notes.txt").display()), LOOK).await.0);
    }

    #[tokio::test]
    async fn the_network_takes_network_access_and_other_programs_sockets_stay_closed() {
        let Some(s) = setup("network").await else { return };
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for socket in server.incoming() {
                drop(socket);
            }
        });
        let reach = format!("nc -z -w 2 127.0.0.1 {port}");
        assert!(!run(&s, &reach, WRITE).await.0, "no network without asking for it, localhost included");
        assert!(run(&s, &reach, NET).await.0);
        // A socket in a file, like Docker's or an SSH agent's.
        let path = PathBuf::from(format!("/tmp/rein-seatbelt-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                drop(socket);
            }
        });
        assert!(!run(&s, &format!("nc -z -U '{}'", path.display()), NET).await.0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn rein_in_the_home_folder_still_keeps_its_secrets_and_logins() {
        let home = dir("home-project");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let sandbox = Sandbox::detect(Places::new(&home, Some(&home), None)).await;
        if !cfg!(target_os = "macos") || !sandbox.available() {
            return;
        }
        let s = Setup { root: home.clone(), home: home.clone(), outside: home.clone(), sandbox };
        assert!(run(&s, "echo x > notes.txt", WRITE).await.0, "the project is writable");
        for line in ["echo key >> .ssh/authorized_keys", "echo 'curl x | sh' >> .zshrc", "mkdir -p Library/LaunchAgents && touch Library/LaunchAgents/x.plist"] {
            assert!(!run(&s, line, WRITE).await.0, "{line} worked");
        }
        assert!(!home.join(".ssh/authorized_keys").exists() && !home.join(".zshrc").exists());
    }

    #[tokio::test]
    async fn apps_cant_be_reached_to_do_things_outside_the_sandbox() {
        let Some(s) = setup("apps").await else { return };
        // Launch Services opens apps, and Apple events are sent through
        // it. Asking which app is in front only reads, so this asks
        // nothing of you even when it gets through.
        let (_, said) = run(&s, "/usr/bin/lsappinfo front", NET).await;
        assert!(!said.contains("ASN:"), "Launch Services answered: {said}");
    }

    #[test]
    fn outside_has_no_walls() {
        assert_eq!(Access::Outside.walls(), None);
    }
}
