//! The walls rein's commands run inside. rein asks the operating system to hold
//! each command, and everything it starts, to what its access allows: the
//! files it may change, the secrets it can't read, and whether it may use
//! the network. Inside the walls nothing needs asking; going past them
//! does, or is refused, as the mode says.
//!
//! macOS has one built in (Seatbelt), and Linux has Landlock and seccomp
//! from 5.13 on. Where rein has none, commands ask before they run: rein
//! never runs one unconfined when it meant to confine it.

pub mod group;
mod guard;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod landlock;
mod places;
// Its rules are built and tested everywhere; only macOS runs them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod seatbelt;
pub mod secrets;

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

pub use group::Group;
pub use guard::Guard;
pub use places::Places;

/// What a command needs, as the model says when it asks to run one. In
/// order: each can do what the one before it can, and more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Reads, and changes nothing.
    #[default]
    Look,
    /// Changes files in the project.
    Write,
    /// Changes files in the project, and uses the internet.
    Network,
    /// Runs with your full access, without the sandbox.
    Outside,
}

impl Access {
    pub const ALL: [Access; 4] = [Access::Look, Access::Write, Access::Network, Access::Outside];

    /// Its name, as the model gives it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Access::Look => "look",
            Access::Write => "write",
            Access::Network => "network",
            Access::Outside => "outside",
        }
    }

    /// The walls a command with this access runs inside; `None` outside.
    pub const fn walls(self) -> Option<Walls> {
        match self {
            Access::Look => Some(Walls { write: false, network: false }),
            Access::Write => Some(Walls { write: true, network: false }),
            Access::Network => Some(Walls { write: true, network: true }),
            Access::Outside => None,
        }
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a command in the sandbox may reach besides reading files that
/// aren't hidden and writing the temp folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Walls {
    /// It may change files in the project, but not protected ones.
    pub write: bool,
    /// It may use the internet, though not other programs' sockets.
    pub network: bool,
}

impl Walls {
    /// What a command that failed within these walls is told, so the model
    /// knows when to ask for more: the walls may be why.
    pub const fn note(self) -> &'static str {
        match (self.write, self.network) {
            (false, _) => {
                "[It ran in the sandbox with look access: it couldn't change files, use the network or read \
                 secrets in the home folder. If that's why it failed, run it again with the access it needs.]"
            }
            (true, false) => {
                "[It ran in the sandbox with write access: it could change files in the project but nowhere else, \
                 and couldn't use the network or read secrets in the home folder. If that's why it failed, \
                 run it again with network or outside access, and say why.]"
            }
            (true, true) => {
                "[It ran in the sandbox with network access: it could change files only in the project, and \
                 couldn't reach other programs on this computer or read secrets in the home folder. If that's \
                 why it failed, run it again with outside access, and say why.]"
            }
        }
    }
}

/// How a step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// In the sandbox, within these walls.
    Sandboxed(Walls),
    /// Without the operating system's sandbox: a command with your full
    /// access, or one of rein's own tools, which keep to the project
    /// themselves.
    Unsandboxed,
}

/// What rein knows about walling commands in, found once when it starts:
/// how it can on this computer, if it can, and where the walls go.
#[derive(Debug, Default)]
pub struct Sandbox {
    how: How,
    places: Places,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum How {
    /// macOS's, through `sandbox-exec`.
    #[cfg(target_os = "macos")]
    Seatbelt,
    /// Linux's, at this Landlock version.
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    Landlock(u32),
    /// Said to be there, for tests that decide by it; it runs nothing.
    #[cfg(feature = "test-support")]
    Assumed,
    Missing(Missing),
}

impl Default for How {
    fn default() -> How {
        How::Missing(Missing::NotHere)
    }
}

/// Why there's no sandbox. Whether you want one is yours to say apart
/// from this: it's about what this computer has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// rein has no way to sandbox commands on this system yet.
    NotHere,
    /// There is one, but it didn't start, and this is what it said. rein
    /// may itself be running in a sandbox, which can't hold another.
    Failed(String),
}

/// Why a command meant for the sandbox didn't run.
pub const NO_SANDBOX: &str = "there's no sandbox here to run it in";

/// How long the sandbox may take to show it works when rein starts.
const PROBE: Duration = Duration::from_secs(5);

impl Sandbox {
    /// This computer's sandbox, with the walls at `places`. rein tries it
    /// once: a sandbox that can't start counts as none.
    pub async fn detect(places: Places) -> Sandbox {
        let how = match Sandbox::this_system() {
            Ok(how) => how,
            Err(why) => return Sandbox::missing(places, why),
        };
        let mut sandbox = Sandbox { how, places };
        sandbox.how = sandbox.probe().await;
        sandbox
    }

    /// The sandbox this system has, untried.
    fn this_system() -> Result<How, Missing> {
        #[cfg(target_os = "macos")]
        if std::path::Path::new(seatbelt::SANDBOX_EXEC).exists() {
            return Ok(How::Seatbelt);
        }
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
        return landlock::version().map(How::Landlock).map_err(Missing::Failed);
        #[allow(unreachable_code)]
        Err(Missing::NotHere)
    }

    /// Runs `true` within the tightest walls: whether that works is whether
    /// the sandbox does, rules and all.
    async fn probe(&self) -> How {
        let mut probe = match self.wrap("true", Walls { write: false, network: false }) {
            Ok(probe) => probe,
            Err(e) => return How::Missing(Missing::Failed(e)),
        };
        probe.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        match tokio::time::timeout(PROBE, probe.output()).await {
            Ok(Ok(output)) if output.status.success() => self.how.clone(),
            Ok(Ok(output)) => How::Missing(Missing::Failed(String::from_utf8_lossy(&output.stderr).trim().to_owned())),
            Ok(Err(e)) => How::Missing(Missing::Failed(e.to_string())),
            Err(_) => How::Missing(Missing::Failed(format!("it took longer than {PROBE:?} to start"))),
        }
    }

    /// No sandbox, because of `why`; the walls still say what's protected.
    pub fn missing(places: Places, why: Missing) -> Sandbox {
        Sandbox { how: How::Missing(why), places }
    }

    /// One that says it's there, without trying: for deciding what needs
    /// asking, in tests that run nothing in it. Whatever is run in it is
    /// still confined, or refused.
    #[cfg(feature = "test-support")]
    pub fn assume(places: Places) -> Sandbox {
        Sandbox { how: How::Assumed, places }
    }

    /// Whether commands can be sandboxed.
    pub fn available(&self) -> bool {
        self.missing_why().is_none()
    }

    /// Why there's no sandbox, when there's none.
    pub fn missing_why(&self) -> Option<&Missing> {
        match &self.how {
            How::Missing(why) => Some(why),
            _ => None,
        }
    }

    /// What holds commands in when it's on, in a few words: for `/status`.
    pub fn describe(&self) -> String {
        match &self.how {
            #[cfg(target_os = "macos")]
            How::Seatbelt => "macOS Seatbelt".to_owned(),
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            How::Landlock(version) => format!("Linux Landlock (version {version}) and seccomp; what runs code later is put back after each command"),
            #[cfg(feature = "test-support")]
            How::Assumed => "assumed, for a test".to_owned(),
            How::Missing(Missing::NotHere) => "none on this system".to_owned(),
            How::Missing(Missing::Failed(said)) => format!("it didn't start ({said})"),
        }
    }

    pub fn places(&self) -> &Places {
        &self.places
    }

    /// The same sandbox, with `own` a folder of its own: what runs in it
    /// may always read and write there, whatever its walls. For an MCP
    /// server's package cache, which `npx` and `uvx` fill as it starts,
    /// and which nothing else may write: whatever's put there, the server
    /// runs.
    pub fn with_own(&self, own: &std::path::Path) -> Sandbox {
        Sandbox { how: self.how.clone(), places: self.places.clone().with_own(own) }
    }

    /// The command `line`, run by `sh` within `walls`, with an environment
    /// that leaves keys and tokens out; what's wrong if it can't be.
    pub fn wrap(&self, line: &str, walls: Walls) -> Result<tokio::process::Command, String> {
        match self.how {
            #[cfg(target_os = "macos")]
            How::Seatbelt => {
                let (profile, params) = seatbelt::profile(&self.places, walls);
                let mut command = tokio::process::Command::new(seatbelt::SANDBOX_EXEC);
                command.arg("-p").arg(profile);
                for param in params {
                    command.arg("-D").arg(param);
                }
                command.arg("/bin/sh").arg("-c").arg(line);
                command.env_clear().envs(passed());
                Ok(command)
            }
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            How::Landlock(version) => {
                use std::os::fd::AsRawFd;
                let ruleset = landlock::ruleset(&self.places, walls, version).map_err(|e| format!("can't set up the sandbox: {e}"))?;
                let filter = landlock::filter(walls);
                let mut command = tokio::process::Command::new("/bin/sh");
                command.arg("-c").arg(line);
                command.env_clear().envs(passed());
                // SAFETY: `confine` makes only system calls, on the ruleset
                // and filter made here, which the closure keeps alive: safe
                // between fork and exec.
                unsafe {
                    command.pre_exec(move || landlock::confine(ruleset.as_raw_fd(), &filter));
                }
                Ok(command)
            }
            #[cfg(feature = "test-support")]
            How::Assumed => Err(NO_SANDBOX.to_owned()),
            How::Missing(_) => Err(NO_SANDBOX.to_owned()),
        }
    }

    /// A guard over what's protected in what a command within `walls` may
    /// write (the project, and tools' records), where this sandbox can't
    /// keep the command from it itself; `None` where it can. What's wrong
    /// if what's protected is too much to guard.
    pub fn guard(&self, walls: Walls) -> Result<Option<Guard>, String> {
        match self.how {
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            How::Landlock(_) => Guard::new(&self.places, walls).map(Some),
            _ => {
                let _ = walls;
                Ok(None)
            }
        }
    }
}

/// Shared by the tools, which run commands in it, and the permissions,
/// which decide what needs asking by it.
pub type Shared = Arc<Sandbox>;

/// rein's environment as a command in the sandbox gets it: without what's
/// secret, or was taken out as secret.
fn passed() -> impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os().filter(|(name, value)| !secrets::withheld(&name.to_string_lossy(), value))
}

/// Parts of a variable's name that say it holds a secret wherever they are
/// in it, even run together with another word (`APIKEY`, `AUTHTOKEN`).
const SECRET_PARTS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "PASSPHRASE", "CREDENTIAL", "ASKPASS", "WEBHOOK", "COOKIE"];

/// Words that say so only as a whole word between `_`s, being short:
/// inside other words they mean something else (`PATH`, `AUTHOR`,
/// `COMPASS`). `PWD` alone is the working folder, and `SESSION` is a
/// secret only as a login (`BW_SESSION`, `OP_SESSION_me`), not when it
/// names one ([`SESSION_FACTS`]).
const SECRET_WORDS: &[&str] = &["PAT", "PASS", "PWD", "AUTH", "CREDS", "DSN", "SESSION"];

/// What follows `SESSION` in the names of variables that say which session
/// this is rather than log in to one: `TERM_SESSION_ID`, `XDG_SESSION_TYPE`,
/// `DBUS_SESSION_BUS_ADDRESS`.
const SESSION_FACTS: &[&str] = &["ID", "TYPE", "CLASS", "DESKTOP", "BUS"];

/// Whether a variable in rein's environment, `name` at `value`, is passed
/// to a command in the sandbox. Not a key, token or password, which goes
/// by its name ([`SECRET_PARTS`], [`SECRET_WORDS`]) or, whatever its name,
/// a value with a URL that has a password in it (`scheme://user:pass@host`,
/// as in `DATABASE_URL`). Nor what reaches an SSH or GPG agent, which would
/// sign or log in for it.
pub(crate) fn passes(name: &str, value: &std::ffi::OsStr) -> bool {
    let upper = name.to_ascii_uppercase();
    let words: Vec<&str> = upper.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let part = SECRET_PARTS.iter().any(|part| upper.contains(part));
    let word = words.iter().enumerate().any(|(i, word)| match *word {
        "PWD" => words.len() > 1,
        "SESSION" => !words.get(i + 1).is_some_and(|next| SESSION_FACTS.contains(next)),
        word => SECRET_WORDS.contains(&word),
    });
    let agent = matches!(upper.as_str(), "SSH_AUTH_SOCK" | "GPG_AGENT_INFO");
    !part && !word && !agent && !has_password(&value.to_string_lossy())
}

/// Whether the variable `name`, at `value`, holds a key, token or
/// password, as its name or value says (see [`passes`]): what rein hides
/// wherever it shows up.
pub fn looks_secret(name: &str, value: &str) -> bool {
    !passes(name, std::ffi::OsStr::new(value))
}

/// Whether `text` has a URL in it with a password: `postgres://me:pw@host`.
fn has_password(text: &str) -> bool {
    text.match_indices("://").any(|(at, _)| {
        let rest = &text[at + 3..];
        let authority = &rest[..rest.find(['/', '?', '#', ' ', ',', ';']).unwrap_or(rest.len())];
        authority.rsplit_once('@').is_some_and(|(user, _)| user.split_once(':').is_some_and(|(_, password)| !password.is_empty()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A folder for a test, outside the temp folders, which every command
    /// may read and write, as a real home folder or project isn't.
    fn dir(name: &str) -> PathBuf {
        let base = std::env::var_os("REIN_TEST_DIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/rein-tests"), PathBuf::from);
        let dir = base.join(format!("lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        places::resolve(&dir)
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Many commands at once, in every kind of access, again and again:
    /// every one that should work does, and the walls hold under load. A
    /// sandbox profile once crashed macOS, so this is how to see one holds
    /// up before it ships. Opt in, with `REIN_STRESS_RUNS` commands (600 by
    /// default), `REIN_STRESS_AT_ONCE` at a time (24):
    /// `cargo test -p rein-sandbox -- --ignored --nocapture many_commands`.
    #[tokio::test]
    #[ignore = "a stress test: hundreds of sandboxed commands, run when the sandbox changes"]
    async fn many_commands_at_once_all_run_and_stay_walled_in() {
        let number = |name: &str, default: usize| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let (runs, at_once) = (number("REIN_STRESS_RUNS", 600), number("REIN_STRESS_AT_ONCE", 24).max(1));
        let (root, home) = (dir("stress"), dir("stress-home"));
        write(&home.join(".ssh/id_test"), "FAKE-stress-secret\n");
        write(&root.join("readme.txt"), "hello\n");
        let sandbox = std::sync::Arc::new(Sandbox::detect(Places::new(&root, Some(&home), None)).await);
        if !sandbox.available() {
            return;
        }
        let (look, write_, network) = (Walls { write: false, network: false }, Walls { write: true, network: false }, Walls { write: true, network: true });
        let secret = home.join(".ssh/id_test").display().to_string();
        // What each command is, the walls it gets, and whether it should work.
        let mixes: Vec<(String, Walls, bool)> = vec![
            ("echo ok".into(), look, true),
            ("uname -a >/dev/null && cat readme.txt >/dev/null && ls / >/dev/null && echo ok".into(), look, true),
            ("(sysctl -n hw.ncpu 2>/dev/null || nproc) >/dev/null && echo ok".into(), look, true),
            ("if command -v git >/dev/null; then git --version >/dev/null || exit 3; fi; echo ok".into(), look, true),
            ("if command -v python3 >/dev/null; then python3 -I -c 'print(1)' >/dev/null || exit 3; fi; echo ok".into(), look, true),
            ("for i in 1 2 3 4 5 6 7 8; do (true) & done; wait; echo ok".into(), look, true),
            ("echo x > \"written-$$\" && rm \"written-$$\" && echo ok".into(), write_, true),
            ("echo x > \"written-$$\" && rm \"written-$$\" && echo ok".into(), network, true),
            // The walls, under load.
            ("echo x > \"look-wrote-$$\"".into(), look, false),
            (format!("cat '{secret}'"), network, false),
        ];
        let started = std::time::Instant::now();
        let (mut done, mut failures) = (0usize, Vec::new());
        while done < runs {
            let mut batch = tokio::task::JoinSet::new();
            for n in done..(done + at_once).min(runs) {
                let (line, walls, works) = mixes[n % mixes.len()].clone();
                let mut command = sandbox.wrap(&line, walls).expect("a sandbox, since one was found");
                command.current_dir(&root).stdin(std::process::Stdio::null());
                batch.spawn(async move {
                    let output = command.output().await;
                    (n, line, works, output)
                });
            }
            while let Some(joined) = batch.join_next().await {
                let (n, line, works, output) = joined.expect("the task ran");
                done += 1;
                let said = match &output {
                    Ok(out) => format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
                    Err(e) => format!("didn't start: {e}"),
                };
                let worked = output.as_ref().is_ok_and(|out| out.status.success()) && said.trim() == "ok";
                let ran = output.is_ok();
                if worked != works || !ran || said.contains("FAKE-stress-secret") {
                    failures.push(format!("#{n} {line}: {said}"));
                }
            }
        }
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().filter_map(Result::ok).map(|e| e.file_name()).filter(|name| name != "readme.txt").collect();
        println!("{runs} sandboxed commands, {at_once} at a time, in {:.1?}", started.elapsed());
        assert!(failures.is_empty(), "{} of {runs} went wrong:\n{}", failures.len(), failures.join("\n"));
        assert!(left.is_empty(), "left in the project: {left:?}");
    }

    /// rein's own file tools refuse what `hides` says is hidden, so it must
    /// be exactly what a command in the sandbox can't read.
    #[tokio::test]
    async fn what_hides_says_is_what_the_sandbox_cant_read() {
        let (root, home, away) = (dir("tie"), dir("tie-home"), dir("tie-away"));
        let files = [
            home.join(".zshrc"),
            home.join("notes.txt"),
            home.join(".cargo/config.toml"),
            home.join(".cargo/credentials.toml"),
            home.join(".ssh/id_test"),
            home.join(".config/git/config"),
            home.join(".config/git/credentials"),
            home.join(".config/other/token"),
            home.join("bin/tool"),
            home.join("work/notes.txt"),
            away.join("claude/.credentials.json"),
            away.join("notes.txt"),
            root.join("readme.txt"),
        ];
        for file in &files {
            write(file, "FAKE\n");
        }
        std::os::unix::fs::symlink(home.join(".zshrc"), root.join("link")).unwrap();
        let path = std::env::join_paths([home.join("bin")]).unwrap();
        let claude = away.join("claude").into_os_string();
        let env = move |name: &str| match name {
            "PATH" => Some(path.clone()),
            "CLAUDE_CONFIG_DIR" => Some(claude.clone()),
            _ => None,
        };
        let places = Places::with(&root, Some(&home), None, &env);
        let sandbox = Sandbox::detect(places.clone()).await;
        if !sandbox.available() {
            return;
        }
        let mut paths: Vec<PathBuf> = files.to_vec();
        paths.extend([root.join("link"), home.clone(), home.join(".cargo"), home.join(".config"), home.join("work"), away.clone(), root.clone()]);
        if cfg!(target_os = "macos") {
            paths.extend([home.join(".ZSHRC"), home.join(".CARGO/config.toml"), home.join(".SSH/id_test")]);
        }
        let mut hidden = 0;
        for path in &paths {
            let line = if path.is_dir() { format!("ls '{}'", path.display()) } else { format!("cat '{}'", path.display()) };
            let mut command = sandbox.wrap(&line, Walls { write: false, network: false }).unwrap();
            let status =
                command.current_dir(&root).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().await.unwrap();
            if path.is_dir() && cfg!(target_os = "linux") {
                // Landlock lists every folder (see `hides`); what's in their
                // files is what it keeps.
                assert!(status.success(), "{}", path.display());
                continue;
            }
            assert_eq!(places.hides(path), !status.success(), "{}", path.display());
            hidden += usize::from(!status.success());
        }
        // Both ways: some it can read, and some it can't.
        assert!(hidden > 0 && hidden < paths.len());
    }

    /// An MCP server's package cache is its own: it writes it even with
    /// look access, where nothing else in your home folder is readable, and
    /// no other command may write it.
    #[tokio::test]
    async fn a_folder_of_its_own_is_written_whatever_the_walls_and_by_nothing_else() {
        let (root, home) = (dir("own"), dir("own-home"));
        let (own, other) = (home.join("place/own"), home.join("place/other"));
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let sandbox = Sandbox::detect(Places::new(&root, Some(&home), None)).await;
        if !sandbox.available() {
            return;
        }
        let run = |sandbox: &Sandbox, line: String| {
            let mut command = sandbox.wrap(&line, Walls { write: false, network: false }).unwrap();
            command.current_dir(&root).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            async move { command.status().await.unwrap().success() }
        };
        let mine = sandbox.with_own(&own);
        let (own, other) = (own.display(), other.display());
        assert!(run(&mine, format!("echo x > '{own}/f' && mkdir -p '{own}/a/b' && cat '{own}/f' && ls '{own}'")).await, "its own, read and written");
        assert!(!run(&mine, format!("echo x > '{other}/f'")).await, "not another folder beside it");
        assert!(!run(&sandbox, format!("echo x > '{own}/g'")).await, "not for a command it isn't its own");
        assert!(!run(&sandbox, format!("cat '{own}/f'")).await, "nor read by one: it's in the home folder");
    }

    #[test]
    fn each_access_reaches_further_than_the_one_before() {
        let walls: Vec<Option<Walls>> = Access::ALL.iter().map(|a| a.walls()).collect();
        assert_eq!(walls, [Some(Walls { write: false, network: false }), Some(Walls { write: true, network: false }), Some(Walls { write: true, network: true }), None]);
        assert!(Access::Look < Access::Write && Access::Network < Access::Outside);
        // A misspelling is an error the model is told about.
        assert!(serde_json::from_str::<Access>("\"net\"").is_err());
        assert_eq!(serde_json::from_str::<Access>("\"network\"").unwrap(), Access::Network);
    }

    #[test]
    fn keys_and_tokens_stay_out_of_a_sandboxed_command() {
        let plain = std::ffi::OsStr::new("x");
        for name in [
            "OPENAI_API_KEY",
            "anthropic_api_key",
            "GITHUB_TOKEN",
            "NGROK_AUTHTOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "PGPASSWORD",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "DB_PASS",
            "MYSQL_PWD",
            "GITHUB_PAT",
            "BW_SESSION",
            "OP_SESSION_my_team",
            "GPG_PASSPHRASE",
            "SLACK_WEBHOOK_URL",
            "NPM_CONFIG__AUTH",
            "AWS_CREDS",
            "SENTRY_DSN",
        ] {
            assert!(!passes(name, plain), "{name}");
        }
        for name in [
            "PATH",
            "HOME",
            "LANG",
            "TERM",
            "CARGO_HOME",
            "GIT_AUTHOR_NAME",
            "TMPDIR",
            "PWD",
            "OLDPWD",
            "TERM_SESSION_ID",
            "XDG_SESSION_TYPE",
            "DBUS_SESSION_BUS_ADDRESS",
            "SECURITYSESSIONID",
            "XAUTHORITY",
            "DATABASE_URL",
        ] {
            assert!(passes(name, plain), "{name}");
        }
    }

    #[test]
    fn a_url_with_a_password_stays_out_whatever_its_name() {
        let passes = |value: &str| passes("DATABASE_URL", std::ffi::OsStr::new(value));
        for value in ["postgres://me:FAKE-pw@localhost:5432/db", "http://user:FAKE@proxy:8080", "redis://:FAKE@cache", "a=1 mysql://u:FAKE@h/x"] {
            assert!(!passes(value), "{value}");
        }
        for value in ["postgres://localhost/db", "ssh://git@github.com:22/me/repo", "https://example.com/a@b:c", "user:pw@host", "https://me:@host", "plain"] {
            assert!(passes(value), "{value}");
        }
    }
}
