//! The walls rein's commands run inside. rein asks the operating system to hold
//! each command, and everything it starts, to what its access allows: the
//! files it may change, the secrets it can't read, and whether it may use
//! the network. Inside the walls nothing needs asking; going past them
//! does, or is refused, as the mode says.
//!
//! macOS has one built in (Seatbelt), and Linux has Landlock and seccomp
//! from 5.13 on. Where rein has none, commands ask before they run: rein
//! never runs one unconfined when it meant to confine it.

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

/// Why there's no sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// `sandbox = off` in the settings.
    TurnedOff,
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

    /// What holds commands in, in a few words: for `/status`.
    pub fn describe(&self) -> String {
        match &self.how {
            #[cfg(target_os = "macos")]
            How::Seatbelt => "on: macOS Seatbelt".to_owned(),
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            How::Landlock(version) => format!("on: Linux Landlock (version {version}) and seccomp; what runs code later is put back after each command"),
            #[cfg(feature = "test-support")]
            How::Assumed => "assumed, for a test".to_owned(),
            How::Missing(Missing::TurnedOff) => "off in the settings: commands ask before they run".to_owned(),
            How::Missing(Missing::NotHere) => "none on this system: commands ask before they run".to_owned(),
            How::Missing(Missing::Failed(said)) => format!("didn't start ({said}): commands ask before they run"),
        }
    }

    pub fn places(&self) -> &Places {
        &self.places
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
                command.env_clear().envs(std::env::vars_os().filter(|(name, _)| passes(&name.to_string_lossy())));
                Ok(command)
            }
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            How::Landlock(version) => {
                use std::os::fd::AsRawFd;
                let ruleset = landlock::ruleset(&self.places, walls, version).map_err(|e| format!("can't set up the sandbox: {e}"))?;
                let filter = landlock::filter(walls);
                let mut command = tokio::process::Command::new("/bin/sh");
                command.arg("-c").arg(line);
                command.env_clear().envs(std::env::vars_os().filter(|(name, _)| passes(&name.to_string_lossy())));
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

    /// A guard over what's protected in the project, for a command within
    /// `walls`, where this sandbox can't keep the command from it itself;
    /// `None` where it can, or the command can't write. What's wrong if
    /// what's protected is too much to guard.
    pub fn guard(&self, walls: Walls) -> Result<Option<Guard>, String> {
        if !walls.write {
            return Ok(None);
        }
        match self.how {
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
            How::Landlock(_) => Guard::new(&self.places).map(Some),
            _ => Ok(None),
        }
    }
}

/// Shared by the tools, which run commands in it, and the permissions,
/// which decide what needs asking by it.
pub type Shared = Arc<Sandbox>;

/// Whether a variable in rein's environment is passed to a command in the
/// sandbox. Keys, tokens and passwords aren't, by name, nor what reaches
/// an SSH or GPG agent, which would sign or log in for it.
pub(crate) fn passes(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let secret = ["KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "CREDENTIAL", "ASKPASS"].iter().any(|word| upper.contains(word));
    !secret && !matches!(upper.as_str(), "SSH_AUTH_SOCK" | "GPG_AGENT_INFO")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for name in ["OPENAI_API_KEY", "anthropic_api_key", "GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "PGPASSWORD", "GOOGLE_APPLICATION_CREDENTIALS", "SSH_AUTH_SOCK", "GIT_ASKPASS"]
        {
            assert!(!passes(name), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "TERM", "CARGO_HOME", "GIT_AUTHOR_NAME", "TMPDIR"] {
            assert!(passes(name), "{name}");
        }
    }
}
