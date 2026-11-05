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

; What other programs run with, their arguments and environments, only for
; what this command started. The kernel hands out another's
; (`KERN_PROCARGS2`, checked as `kern.procargs2.<pid>`) unless both of these
; say no: the sysctl isn't among the names below, and `process-info` is
; denied in so many words, since the default doesn't count for it there.
; launchd goes by it too, and won't print your session's `launchctl setenv`
; values to a command denied it.
(deny process-info*)
(allow process-info* (target same-sandbox))

; What the system is and runs on, by name, and the process table (ids and
; names, as `kern.proc.*` gives them, not arguments or environments).
(allow sysctl-read
  (sysctl-name-prefix "hw.")
  (sysctl-name-prefix "machdep.")
  (sysctl-name-prefix "sysctl.")
  (sysctl-name-prefix "kern.proc.")
  (sysctl-name-prefix "net.routetable.")
  (sysctl-name "kern.argmax")
  (sysctl-name "kern.bootargs")
  (sysctl-name "kern.boottime")
  (sysctl-name "kern.hostname")
  (sysctl-name "kern.hv_vmm_present")
  (sysctl-name "kern.iossupportversion")
  (sysctl-name "kern.maxfiles")
  (sysctl-name "kern.maxfilesperproc")
  (sysctl-name "kern.maxproc")
  (sysctl-name "kern.ngroups")
  (sysctl-name "kern.osproductversion")
  (sysctl-name "kern.osproductversioncompat")
  (sysctl-name "kern.osrelease")
  (sysctl-name "kern.osrevision")
  (sysctl-name "kern.ostype")
  (sysctl-name "kern.osvariant_status")
  (sysctl-name "kern.osversion")
  (sysctl-name "kern.secure_kernel")
  (sysctl-name "kern.singleuser")
  (sysctl-name "kern.usrstack64")
  (sysctl-name "kern.version")
  (sysctl-name "kern.willshutdown")
  (sysctl-name "security.mac.lockdown_mode_state")
  (sysctl-name "vm.loadavg"))

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

    text.push_str("\n; Writing: the temp folders and its own, and with write access the project.\n");
    let mut temp = subpaths(places.temp(), &mut param);
    temp.extend(subpaths(places.own(), &mut param));
    rule(&mut text, "allow file-write*", &temp);
    text.push_str("\n; Tools' records, whatever the walls: as files, never links or folders.\n");
    let recorded: Vec<String> = places.recorded().iter().map(|path| format!("(literal {})", param(path))).collect();
    if !recorded.is_empty() {
        rule(&mut text, "allow file-write*", &[format!("(require-all (require-any {}) (vnode-type REGULAR-FILE))", recorded.join(" "))]);
    }
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
    if let Some(home) = places.home() {
        text.push_str("\n; Not read: your home folder, but for the project, toolchains, caches and settings,\n; and what leads to them, as a path, not a list of what's in it.\n");
        rule(&mut text, "deny file-read*", &[format!("(subpath {})", param(home))]);
        let readable: Vec<std::path::PathBuf> = places.readable().iter().chain(places.own()).cloned().collect();
        let ways: Vec<String> = leading(&readable, home).iter().map(|path| format!("(literal {})", param(path))).collect();
        rule(&mut text, "allow file-read-metadata", &ways);
        rule(&mut text, "allow file-read*", &subpaths(&readable, &mut param));
    }
    text.push_str("\n; Never read: keys, logins and tokens, wherever they are.\n");
    rule(&mut text, "deny file-read*", &hidden);
    text.push_str("\n; But skills kept among them, which hold none: read, and the way to them.\n");
    if let Some(home) = places.home() {
        let ways: Vec<String> = leading(places.unhidden(), home).iter().map(|path| format!("(literal {})", param(path))).collect();
        rule(&mut text, "allow file-read-metadata", &ways);
    }
    rule(&mut text, "allow file-read*", &subpaths(places.unhidden(), &mut param));

    if walls.network {
        text.push_str(NETWORK);
    }
    (text, params)
}

/// The folders from `home` down to each of `places`, not counting the
/// places themselves: what a path to them goes through.
fn leading(places: &[std::path::PathBuf], home: &Path) -> Vec<std::path::PathBuf> {
    let mut ways: Vec<std::path::PathBuf> = places.iter().flat_map(|place| place.ancestors().skip(1).take_while(|dir| dir.starts_with(home)).map(Path::to_path_buf)).collect();
    ways.sort();
    ways.dedup();
    ways
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

    /// A profile that names syscalls crashed the Mac once: past the number
    /// the kernel takes, its syscall mask panics macOS (`mask size (75)
    /// exceeds maximum (70)`), and a list that's fine on one macOS can be
    /// past it on an older one. So no profile of rein's names any, whatever
    /// the walls and wherever the project is. This only reads the rules: it
    /// never runs them.
    #[test]
    fn no_profile_ever_names_a_syscall() {
        let root = dir("syscalls");
        for places in [Places::new(&root, None, None), Places::new(&root, Some(&root), Some(&root.join(".rein")))] {
            for write in [false, true] {
                for network in [false, true] {
                    let (text, _) = profile(&places, Walls { write, network });
                    assert!(!text.to_lowercase().contains("syscall"), "a profile names a syscall:\n{text}");
                }
            }
        }
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

    #[test]
    fn the_home_folder_is_walled_off_but_what_tools_need_and_secrets_stay_hidden() {
        let root = dir("home-rules");
        let home = dir("home-rules-home");
        let places = Places::new(&root, Some(&home), None);
        let (text, params) = profile(&places, LOOK);
        let home_text = home.to_str().unwrap();
        let walled =
            text.find(&format!("(deny file-read*\n  (subpath (param \"P{}\"))", params.iter().position(|p| p.to_str().unwrap().ends_with(&format!("={home_text}"))).unwrap()));
        let readable = walled.and_then(|at| text[at..].find("(allow file-read*").map(|found| at + found));
        let hidden = text.rfind("(deny file-read*");
        let skills = text.rfind("(allow file-read*");
        assert!(walled.is_some() && walled < readable && readable < hidden && hidden < skills, "skills, in what's hidden, read last: {text}");
        assert!(value(&params, &text, "(allow file-read*").last().is_some_and(|path| path.ends_with("/.claude/plugins/cache")), "{text}");
        assert!(value(&params, &text, "(allow file-read*\n").iter().any(|path| path.ends_with("/.cargo")), "{text}");
        assert!(value(&params, &text, "(allow file-read-metadata").contains(&home_text), "the way to what's readable: {text}");
        // Other processes' details: denied in so many words, and no sysctl
        // by a name that isn't listed.
        assert!(text.find("(deny process-info*)") < text.find("(allow process-info* (target same-sandbox))"));
        assert!(!text.contains("(allow sysctl-read)") && !text.contains("\"kern.procargs") && !text.contains("(sysctl-name-prefix \"kern.\")"));
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

    /// rtk keeps its stats in your home folder: every command may add to
    /// them, whatever its walls, but only as files, and never its settings,
    /// which say what rtk does outside the walls too. Walled off, `rtk
    /// gain` failed, and every rtk command said its hook wasn't installed.
    #[tokio::test]
    async fn a_tools_records_are_added_to_as_files_never_links_nor_its_settings() {
        let Some(s) = setup("records").await else { return };
        let rtk = s.home.join("Library/Application Support/rtk");
        std::fs::create_dir_all(&rtk).unwrap();
        for (name, text) in [("history.db", "stats\n"), ("config.toml", "[tee]\n"), ("filters.toml", ""), ("trusted_filters.json", "{}")] {
            std::fs::write(rtk.join(name), text).unwrap();
        }
        let zshrc = s.home.join(".zshrc");
        std::fs::write(&zshrc, "mine\n").unwrap();
        let at = |name: &str| format!("'{}'", rtk.join(name).display());
        for line in [
            format!("echo more >> {}", at("history.db")),
            format!("echo wal > {}", at("history.db-wal")),
            format!(": > {}", at(".hook_warn_last")),
            format!("cat {}", at("config.toml")),
        ] {
            let (worked, said) = run(&s, &line, LOOK).await;
            assert!(worked, "{line}: {said}");
        }
        let moved = "\"$TMPDIR/rein-seatbelt-records-$$\"";
        for line in [
            format!("echo x > {}", at("config.toml")),
            format!("echo x > {}", at("filters.toml")),
            format!("echo x > {}", at("trusted_filters.json")),
            format!("echo x > {}", at("other.txt")),
            format!("mkdir {}", at("tee")),
            // A link in place of the stats, made there or moved in.
            format!("rm -f {wal} && ln -s '{}' {wal}", zshrc.display(), wal = at("history.db-wal")),
            format!("ln -s '{}' {moved} && mv {moved} {}; worked=$?; rm -f {moved}; exit $worked", zshrc.display(), at("history.db-journal")),
        ] {
            let (worked, said) = run(&s, &line, LOOK).await;
            assert!(!worked, "{line} worked: {said}");
        }
        assert!(std::fs::read_to_string(rtk.join("history.db")).unwrap().ends_with("more\n"));
        assert_eq!(std::fs::read_to_string(rtk.join("config.toml")).unwrap(), "[tee]\n");
        assert!(!rtk.join("history.db-wal").exists() && !rtk.join("history.db-journal").exists() && !rtk.join("tee").exists());
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), "mine\n");
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

    /// Claude Code keeps its skills, and the plugins you install, among its
    /// logins and settings, all hidden: a skill's script is read, and
    /// nothing else there, nor through a link from a skill.
    #[tokio::test]
    async fn skills_in_a_hidden_folder_are_read_and_nothing_else_there() {
        let Some(s) = setup("skills").await else { return };
        let claude = s.home.join(".claude");
        for (file, text) in [
            ("skills/graphs/run.sh", "echo drawn\n"),
            ("plugins/cache/market/kit/1.0.0/skills/lint/SKILL.md", "lint\n"),
            (".credentials.json", "FAKE-token\n"),
            ("plugins/data/kit/token", "FAKE-data\n"),
            ("settings.json", "{}\n"),
        ] {
            std::fs::create_dir_all(claude.join(file).parent().unwrap()).unwrap();
            std::fs::write(claude.join(file), text).unwrap();
        }
        std::os::unix::fs::symlink(claude.join(".credentials.json"), claude.join("skills/graphs/leak")).unwrap();
        let (worked, said) = run(&s, &format!("sh '{}'", claude.join("skills/graphs/run.sh").display()), LOOK).await;
        assert!(worked && said.trim() == "drawn", "{said}");
        assert!(run(&s, &format!("cat '{}'", claude.join("plugins/cache/market/kit/1.0.0/skills/lint/SKILL.md").display()), LOOK).await.0);
        for secret in [".credentials.json", "plugins/data/kit/token", "settings.json", "skills/graphs/leak"] {
            let (worked, said) = run(&s, &format!("cat '{}'", claude.join(secret).display()), NET).await;
            assert!(!worked && !said.contains("FAKE"), "{secret}: {said}");
        }
        assert!(!run(&s, &format!("ls '{}'", claude.display()), LOOK).await.0, "nor what's in the folder");
        assert!(!run(&s, &format!("echo x > '{}'", claude.join("skills/graphs/run.sh").display()), WRITE).await.0, "read, never changed");
        let places = s.sandbox.places();
        assert!(
            !places.hides(&claude.join("skills/graphs/run.sh")) && places.hides(&claude.join(".credentials.json")) && places.hides(&claude.join("skills/graphs/leak")),
            "rein's own tools agree"
        );
    }

    #[tokio::test]
    async fn the_home_folder_cant_be_read_but_for_what_tools_need() {
        let Some(s) = setup("hidden").await else { return };
        std::fs::create_dir_all(s.home.join(".cargo")).unwrap();
        std::fs::write(s.home.join(".cargo/config.toml"), "[build]\n").unwrap();
        std::fs::write(s.home.join(".cargo/credentials.toml"), "FAKE-token\n").unwrap();
        std::fs::write(s.home.join(".zshenv"), "export FAKE_KEY=FAKE-key\n").unwrap();
        for secret in [".ssh/id_test", "notes.txt", ".zshenv", ".cargo/credentials.toml"] {
            assert!(!run(&s, &format!("cat '{}'", s.home.join(secret).display()), NET).await.0, "{secret}");
        }
        assert!(!run(&s, &format!("ls '{}'", s.home.display()), NET).await.0, "nor what's in it");
        assert!(run(&s, &format!("cat '{}'", s.home.join(".cargo/config.toml").display()), LOOK).await.0);
        std::fs::write(s.outside.join("notes.txt"), "theirs\n").unwrap();
        assert!(run(&s, &format!("cat '{}'", s.outside.join("notes.txt").display()), LOOK).await.0, "outside it, as before");
    }

    #[tokio::test]
    async fn a_project_in_the_home_folder_is_reached_through_it() {
        let home = dir("deep-home");
        let root = home.join("work/app");
        std::fs::create_dir_all(&root).unwrap();
        let sandbox = Sandbox::detect(Places::new(&root, Some(&home), None)).await;
        if !cfg!(target_os = "macos") || !sandbox.available() {
            return;
        }
        let s = Setup { root: root.clone(), home: home.clone(), outside: home.clone(), sandbox };
        // npm, cargo and Python's venv look at the folders on the way
        // (`stat`), and stop if they can't.
        for folder in [&home, &home.join("work")] {
            let (worked, said) = run(&s, &format!("/usr/bin/stat '{}'", folder.display()), LOOK).await;
            assert!(worked, "{}: {said}", folder.display());
        }
        assert!(!run(&s, &format!("ls '{}'", home.join("work").display()), LOOK).await.0, "though not what's in them");
    }

    /// The shell line that runs this test binary for only the ignored test
    /// `name`, with `vars` set.
    #[cfg(target_os = "macos")]
    fn this_binary(vars: &str, name: &str) -> String {
        format!("{vars} '{}' --exact seatbelt::tests::{name} --ignored --nocapture --test-threads 1", std::env::current_exe().unwrap().display())
    }

    // `KERN_PROCARGS2` is macOS's; Linux's `/proc` has its own test.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn other_processes_environments_are_out_of_reach() {
        let Some(s) = setup("processes").await else { return };
        // Not one of Apple's programs, which hide their environment anyway:
        // this test binary, waiting.
        let mut other = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "seatbelt::tests::wait_for_a_test", "--ignored", "--test-threads", "1"])
            .env("REIN_TEST_WAIT", "1")
            .env("REIN_TEST_SECRET", "FAKE-hunter2")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let read = this_binary(&format!("REIN_TEST_READ_PID={}", other.id()), "read_another_environment");
        // Read until it's there, from outside: the probe works, and the
        // process has started.
        let wait = format!("for i in $(seq 100); do out=$({read}); case $out in *FAKE-hunter2*) echo \"$out\"; break;; esac; sleep 0.1; done");
        let outside = std::process::Command::new("/bin/sh").args(["-c", &wait]).output().unwrap();
        let outside = String::from_utf8_lossy(&outside.stdout).into_owned();
        let (_, inside) = run(&s, &read, NET).await;
        let _ = other.kill();
        let _ = other.wait();
        assert!(outside.contains("FAKE-hunter2"), "the probe reads it from outside: {outside}");
        assert!(!inside.contains("FAKE-hunter2"), "{inside}");
        // What it started itself, it may still look at.
        let start = this_binary("REIN_TEST_WAIT=1 REIN_TEST_SECRET=FAKE-own", "wait_for_a_test");
        let read = this_binary("REIN_TEST_READ_PID=$child", "read_another_environment");
        let own =
            format!("{start} >/dev/null & child=$!; for i in $(seq 100); do out=$({read}); case $out in *FAKE-own*) echo \"$out\"; break;; esac; sleep 0.1; done; kill $child");
        let (_, own) = run(&s, &own, NET).await;
        assert!(own.contains("FAKE-own"), "{own}");
        // launchd's copy of `launchctl setenv` values, where there's a
        // session to print. Only read: nothing is set.
        // SAFETY: getuid can't fail, and touches no memory of ours.
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let printed = std::process::Command::new("/bin/launchctl").args(["print", &domain]).output().is_ok_and(|o| o.status.success());
        if printed {
            assert!(!run(&s, &format!("/bin/launchctl print {domain} >/dev/null"), NET).await.0, "launchctl printed the session's environment");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "run by other_processes_environments_are_out_of_reach, as the process it reads"]
    fn wait_for_a_test() {
        if std::env::var_os("REIN_TEST_WAIT").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "run by other_processes_environments_are_out_of_reach, inside the sandbox and out"]
    fn read_another_environment() {
        let Some(pid) = std::env::var("REIN_TEST_READ_PID").ok().and_then(|pid| pid.parse::<i32>().ok()) else { return };
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: asks only for the size; `mib` holds three ints.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            println!("refused: {}", std::io::Error::last_os_error());
            return;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` holds `size` bytes, as the kernel was told.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            println!("refused: {}", std::io::Error::last_os_error());
            return;
        }
        buf.truncate(size);
        println!("{}", String::from_utf8_lossy(&buf).replace('\0', "\n"));
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
