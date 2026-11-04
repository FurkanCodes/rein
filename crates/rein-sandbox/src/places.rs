//! Where a sandboxed command's walls go, worked out once when rein starts:
//! the project and what in it is protected, git's own folders, the temp
//! folders, and what it may read of your home folder.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// What in a project runs code later without anyone choosing to run it,
/// or says what may run. A command in the sandbox can't change these, and
/// rein's own file tools ask first, even in edits mode.
const PROTECTED: &[&str] = &[
    // rein's own rules.
    ".rein",
    // With `objects` and `refs` beside it, a `HEAD` makes the project a git
    // folder of its own, whose config git would read and run from.
    "HEAD",
    // direnv runs it when you go into the folder.
    ".envrc",
    // Editors run tasks and settings from these when the folder opens.
    ".vscode",
    ".idea",
    ".zed",
    ".devcontainer",
    // Other agents' hooks and servers.
    ".claude",
    ".codex",
    ".cursor",
    ".gemini",
    ".mcp.json",
    // Git hook managers: what these say runs on your next commit.
    ".husky",
    ".pre-commit-config.yaml",
    "lefthook.yml",
    ".lefthook.yml",
];

/// In a git folder: the hooks git runs, and its settings, which can name
/// more to run (`core.fsmonitor`, `core.hooksPath`, aliases, filters).
const IN_GIT: &[&str] = &["hooks", "config", "config.worktree"];

/// In your home folder, what a command in the sandbox may read besides the
/// project, git's folders and the temp folders: the toolchains, package
/// caches and settings that builds and everyday tools need. The rest of
/// your home folder is out of its reach, since what's secret there can't
/// all be listed: keys exported in shell settings, histories, `.env`
/// files, other apps' logins. What [`HIDDEN`] names stays hidden in these.
const READABLE: &[&str] = &[
    // Rust.
    ".cargo",
    ".rustup",
    // JavaScript.
    ".nvm",
    ".npm",
    ".bun",
    ".deno",
    ".volta",
    ".yarn",
    "Library/pnpm",
    ".local/share/pnpm",
    // Python: user installs, and what pyenv, pipx and uv install.
    ".pyenv",
    ".local/bin",
    ".local/lib",
    ".local/pipx",
    ".local/share/uv",
    "Library/Python",
    // Others, and the version managers for many.
    "go",
    ".rbenv",
    ".gem",
    ".gradle",
    ".m2",
    ".sdkman",
    ".asdf",
    ".local/share/mise",
    // Caches.
    ".cache",
    // Skills, whose scripts a skill has commands run: read, never changed.
    ".agents/skills",
    ".claude/skills",
    // Git's and Go's own settings, and the text encoding macOS programs
    // read at start.
    ".gitconfig",
    ".config/git",
    ".config/go",
    "Library/Application Support/go",
    ".CFUserTextEncoding",
];

/// In your home folder: keys, logins and tokens. A command in the sandbox
/// can neither read nor change these, even inside what it may read
/// (`.cargo`, or a project that is your home folder).
const HIDDEN: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".config/gh",
    ".config/github-copilot",
    ".npmrc",
    ".pypirc",
    ".gem/credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".gradle/gradle.properties",
    ".m2/settings.xml",
    ".m2/settings-security.xml",
    ".local/share/uv/credentials",
    ".cache/huggingface/token",
    ".cache/huggingface/stored_tokens",
    ".config/rein",
    ".claude",
    ".claude.json",
    ".codex",
    ".gemini",
    ".password-store",
    ".local/share/keyrings",
    ".env",
    ".zsh_history",
    ".bash_history",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".cache/mozilla",
    ".cache/google-chrome",
    ".cache/chromium",
    ".cache/BraveSoftware",
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Arc",
];

/// Variables that move something [`READABLE`] or [`HIDDEN`] names, maybe
/// out of your home folder, and what each stands for there: wherever one
/// points, what's inside is readable or hidden as it would be at home.
/// Some take a list (`KUBECONFIG`, `GOPATH`).
const MOVED: &[(&str, &str)] = &[
    ("XDG_CONFIG_HOME", ".config"),
    ("XDG_CACHE_HOME", ".cache"),
    ("XDG_DATA_HOME", ".local/share"),
    ("CARGO_HOME", ".cargo"),
    ("RUSTUP_HOME", ".rustup"),
    ("NVM_DIR", ".nvm"),
    ("BUN_INSTALL", ".bun"),
    ("PYENV_ROOT", ".pyenv"),
    ("GOPATH", "go"),
    ("GRADLE_USER_HOME", ".gradle"),
    ("HF_HOME", ".cache/huggingface"),
    ("CLAUDE_CONFIG_DIR", ".claude"),
    ("CODEX_HOME", ".codex"),
    ("GNUPGHOME", ".gnupg"),
    ("DOCKER_CONFIG", ".docker"),
    ("GH_CONFIG_DIR", ".config/gh"),
    ("CLOUDSDK_CONFIG", ".config/gcloud"),
    ("AZURE_CONFIG_DIR", ".azure"),
    ("PASSWORD_STORE_DIR", ".password-store"),
    ("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials"),
    ("AWS_CONFIG_FILE", ".aws/config"),
    ("KUBECONFIG", ".kube/config"),
    ("NPM_CONFIG_USERCONFIG", ".npmrc"),
    ("npm_config_userconfig", ".npmrc"),
];

/// How many of git's settings files rein follows `include`s through.
const MAX_GIT_FILES: usize = 32;

/// Where rein reads its environment from: a variable's value by name.
type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// In your home folder: what your shell, git and your login run. Out of
/// a command's reach anyway, unless the project is your home folder.
const RUN_AT_LOGIN: &[&str] =
    &[".zshrc", ".zshenv", ".zprofile", ".zlogin", ".bashrc", ".bash_profile", ".bash_login", ".profile", ".config/fish", ".gitconfig", ".config/git", "Library/LaunchAgents"];

/// How deep rein looks for submodules' git folders, and how many it takes.
const MODULE_DEPTH: usize = 4;
const MAX_MODULES: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct Places {
    /// The project, resolved.
    root: PathBuf,
    /// What `write` access may change: the project, and git's own folders
    /// when they're elsewhere (a worktree's, or a submodule's), since
    /// `git commit` writes there.
    writable: Vec<PathBuf>,
    /// Never written, nor anything in them.
    protected: Vec<PathBuf>,
    /// Never written as such: the folders that hold protected paths, so
    /// that renaming one can't move what's protected out of the way, and
    /// a missing `.git` can't be made one that points elsewhere.
    locked: Vec<PathBuf>,
    /// Temp folders every command may write.
    temp: Vec<PathBuf>,
    /// Folders of this command's own, which it may always read and write,
    /// but no other command may: an MCP server's package cache.
    own: Vec<PathBuf>,
    /// Never read or written, wherever they are.
    hidden: Vec<PathBuf>,
    /// Your home folder: none of it is read but what `readable` names.
    home: Option<PathBuf>,
    /// In your home folder, what may be read all the same, but for what's
    /// hidden in it: the project, git's folders, the temp folders, and
    /// toolchains, caches and settings.
    readable: Vec<PathBuf>,
}

impl Places {
    /// For the project at `root`, with your home folder at `home`, and
    /// rein's own (keys, history, sessions) at `rein`. The environment says
    /// where toolchains and secrets are when they're not where they usually
    /// are, and which folders in your home hold programs (`PATH`).
    pub fn new(root: &Path, home: Option<&Path>, rein: Option<&Path>) -> Places {
        Places::with(root, home, rein, &crate::secrets::var_os)
    }

    /// The same, with the environment from `env`.
    pub(crate) fn with(root: &Path, home: Option<&Path>, rein: Option<&Path>, env: Env) -> Places {
        let root = resolve(root);
        let mut places = Places { root: root.clone(), writable: vec![root.clone()], ..Places::default() };
        let mut gits = git_dirs(&root);
        // Writable first: what's locked is worked out within them. The
        // widest first, so a worktree's own git folder counts as inside the
        // repository's, not a place of its own.
        gits.sort_by_key(|dir| dir.components().count());
        for dir in &gits {
            if !places.writable.iter().any(|w| dir.starts_with(w)) {
                places.writable.push(dir.clone());
            }
        }
        for name in PROTECTED {
            places.protect(&root.join(name));
        }
        // `.git` is protected even before there is one.
        for dir in std::iter::once(root.join(".git")).chain(gits.iter().cloned()) {
            for name in IN_GIT {
                places.protect(&dir.join(name));
            }
            for module in submodules(&dir) {
                for name in IN_GIT {
                    places.protect(&module.join(name));
                }
            }
        }
        places.locked.push(root.join(".git"));
        for hooks in hooks_paths(&root, &gits, home, env) {
            places.protect(&hooks);
        }
        // Not the root folder as a home: walling it off would leave nothing.
        let home = home.map(resolve).filter(|home| home.parent().is_some());
        places.hidden.extend(HIDDEN.iter().flat_map(|name| locations(home.as_deref(), name, env)));
        if let Some(rein) = rein {
            places.hidden.push(resolve(rein));
        }
        places.temp = temp_dirs();
        if let Some(home) = home {
            places.protected.extend(RUN_AT_LOGIN.iter().map(|name| resolve(&home.join(name))));
            places.readable = readable(&places, &home, env);
            places.home = Some(home);
        }
        places.hidden.sort();
        places.hidden.dedup();
        places.protected.dedup();
        places.locked.sort();
        places.locked.dedup();
        places
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn writable(&self) -> &[PathBuf] {
        &self.writable
    }

    pub(crate) fn protected(&self) -> &[PathBuf] {
        &self.protected
    }

    pub(crate) fn locked(&self) -> &[PathBuf] {
        &self.locked
    }

    pub(crate) fn temp(&self) -> &[PathBuf] {
        &self.temp
    }

    pub(crate) fn own(&self) -> &[PathBuf] {
        &self.own
    }

    /// The same places, with `dir` one of the command's own (see `own`),
    /// unless it's inside what's hidden, which nothing may read.
    pub(crate) fn with_own(mut self, dir: &Path) -> Places {
        let dir = resolve(dir);
        if !self.hidden.iter().any(|hidden| lower(&dir).starts_with(lower(hidden))) {
            self.own.push(dir);
        }
        self
    }

    pub(crate) fn hidden(&self) -> &[PathBuf] {
        &self.hidden
    }

    pub(crate) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub(crate) fn readable(&self) -> &[PathBuf] {
        &self.readable
    }

    /// Whether `path`, relative to the project as the model gives it, is
    /// one rein's file tools must ask about: protected, or a folder that
    /// holds what is. Letter case doesn't count, as it doesn't on macOS:
    /// `.REIN/permissions` is `.rein/permissions` there. A path with `..`
    /// in it is protected too, rather than worked out.
    pub fn protects(&self, path: &str) -> bool {
        if self.root.as_os_str().is_empty() {
            return false;
        }
        if Path::new(path).components().any(|c| c == Component::ParentDir) {
            return true;
        }
        let full = lower(&resolve(&self.root.join(path)));
        self.protected.iter().any(|p| full.starts_with(lower(p))) || self.locked.iter().any(|l| full == lower(l))
    }

    /// Whether `path`, absolute, is out of a sandboxed command's reach:
    /// what it can't read, it's not for rein's own file tools to read for
    /// the model either. That's what's hidden, and what's in your home
    /// folder but not readable there; a folder that only leads to a
    /// readable place (the project's, say) is out of reach too, as its
    /// list of files is. Symlinks are followed as far as the path exists.
    /// Letter case doesn't count for what's hidden, as on macOS; for the
    /// rest it counts as the system counts it, so this never says yes to
    /// what the sandbox would refuse. On Linux a command can list any
    /// folder, hidden or not, since Landlock lets it list `/` only by
    /// letting it list all that's in it; this says no to such a folder all
    /// the same, as on macOS, where it can't.
    pub fn hides(&self, path: &Path) -> bool {
        let full = resolve(path);
        let hidden = self.hidden.iter().any(|hidden| lower(&full).starts_with(lower(hidden)));
        let walled = self.home.as_ref().is_some_and(|home| {
            let full = as_compared(&full);
            full.starts_with(as_compared(home)) && !self.readable.iter().any(|place| full.starts_with(as_compared(place)))
        });
        hidden || walled
    }

    /// Protects `path`, and locks the folders between it and the writable
    /// place it's in.
    fn protect(&mut self, path: &Path) {
        let path = resolve(path);
        let within = self.writable.iter().filter(|w| path.starts_with(w)).max_by_key(|w| w.components().count()).cloned();
        if let Some(within) = within {
            let mut folder = path.parent();
            while let Some(dir) = folder.filter(|dir| dir.starts_with(&within) && *dir != within) {
                self.locked.push(dir.to_path_buf());
                folder = dir.parent();
            }
        }
        if !self.protected.contains(&path) {
            self.protected.push(path);
        }
    }
}

/// `path` with symlinks followed as far as it exists, and the rest as
/// written: so a path that doesn't exist yet still compares with one that
/// does. As given, if even that can't be done.
pub(crate) fn resolve(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut missing = Vec::new();
    while existing.symlink_metadata().is_err() {
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else { return path.to_path_buf() };
        missing.push(name);
        existing = parent;
    }
    match existing.canonicalize() {
        Ok(resolved) => missing.iter().rev().fold(resolved, |dir, name| dir.join(name)),
        Err(_) => path.to_path_buf(),
    }
}

fn lower(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// `path` as this system's files compare it: letter case doesn't count on
/// macOS, and does elsewhere.
fn as_compared(path: &Path) -> PathBuf {
    if cfg!(target_os = "macos") { lower(path) } else { path.to_path_buf() }
}

/// Where `name`, as it would be in your home folder, is: there, and where
/// a variable in [`MOVED`] puts it. Resolved, so each compares with what
/// the sandbox sees.
fn locations(home: Option<&Path>, name: &str, env: Env) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = home.map(|home| home.join(name)).into_iter().collect();
    for (variable, stands_for) in MOVED {
        let Some(value) = env(variable) else { continue };
        for moved in std::env::split_paths(&value).filter(|path| path.is_absolute()) {
            match Path::new(name).strip_prefix(stands_for) {
                Ok(rest) if rest.as_os_str().is_empty() => found.push(moved),
                Ok(rest) => found.push(moved.join(rest)),
                // What the variable points to is part of `name`: `.aws`'s
                // credentials, wherever they're kept.
                Err(_) if Path::new(stands_for).starts_with(name) => found.push(moved),
                Err(_) => {}
            }
        }
    }
    found.iter().map(|path| resolve(path)).collect()
}

/// What a command may read in `home`, besides what `places` makes it
/// write: toolchains and caches wherever they are, the folders on `PATH`,
/// and the files git's settings there name. None is the home folder itself
/// or holds it, and none is inside what's hidden.
fn readable(places: &Places, home: &Path, env: Env) -> Vec<PathBuf> {
    let mut readable: Vec<PathBuf> = READABLE.iter().flat_map(|name| locations(Some(home), name, env)).collect();
    if let Some(path) = env("PATH") {
        readable.extend(std::env::split_paths(&path).filter(|dir| dir.is_absolute()).map(|dir| resolve(&dir)));
    }
    readable.extend(git_files(home, env));
    readable.retain(|place| !home.starts_with(place));
    // The project may be your home folder, or hold it.
    readable.push(places.root.clone());
    readable.extend(places.writable.iter().chain(&places.temp).cloned());
    readable.retain(|place| place.starts_with(home) && !places.hidden.iter().any(|hidden| place.starts_with(hidden)));
    readable.sort();
    readable.dedup();
    readable
}

/// The files git reads from your home folder: its settings, what they
/// include, and the ignore and attributes files they name. Git stops when
/// it can't read one, so a command needs them all. Absolute, or from `~/`.
fn git_files(home: &Path, env: Env) -> Vec<PathBuf> {
    let config = env("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    let mut files = vec![home.join(".gitconfig"), config.join("git/config")];
    files.extend(env("GIT_CONFIG_GLOBAL").map(PathBuf::from));
    let mut named = Vec::new();
    let mut next = 0;
    while let Some(file) = files.get(next).cloned() {
        next += 1;
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let folder = file.parent().unwrap_or(home);
        for value in config_values(&text, &["include", "includeif"], "path") {
            let path = from_home(&value, home).unwrap_or_else(|| folder.join(&value));
            if !files.contains(&path) && files.len() < MAX_GIT_FILES {
                files.push(path);
            }
        }
        let others = [config_values(&text, &["core"], "excludesfile"), config_values(&text, &["core"], "attributesfile")];
        named.extend(others.iter().flatten().filter_map(|value| from_home(value, home).or_else(|| Path::new(value).is_absolute().then(|| PathBuf::from(value)))));
    }
    files.extend(named);
    files.iter().map(|file| resolve(file)).collect()
}

/// `value` in `home`, if it starts `~/`.
fn from_home(value: &str, home: &Path) -> Option<PathBuf> {
    value.strip_prefix("~/").map(|rest| home.join(rest))
}

/// The project's git folders: `.git`, or the one a `.git` file points to
/// (a worktree's, a submodule's), and the folder shared by all of a
/// repository's worktrees.
fn git_dirs(root: &Path) -> Vec<PathBuf> {
    let dot = root.join(".git");
    let git = if dot.is_dir() {
        dot
    } else {
        let Ok(text) = std::fs::read_to_string(&dot) else { return Vec::new() };
        let Some(target) = text.lines().find_map(|line| line.strip_prefix("gitdir:")) else { return Vec::new() };
        resolve(&root.join(target.trim()))
    };
    let mut dirs = vec![git.clone()];
    if let Ok(common) = std::fs::read_to_string(git.join("commondir")) {
        let common = resolve(&git.join(common.trim()));
        if common != git {
            dirs.push(common);
        }
    }
    dirs
}

/// Submodules' git folders inside `git`'s `modules`: each holds a `HEAD`.
fn submodules(git: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut look = vec![(git.join("modules"), 0)];
    while let Some((dir, depth)) = look.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || found.len() >= MAX_MODULES {
                continue;
            }
            if path.join("HEAD").is_file() {
                look.push((path.join("modules"), depth + 1));
                found.push(path);
            } else if depth < MODULE_DEPTH {
                // A submodule named `a/b` lives in `modules/a/b`.
                look.push((path, depth + 1));
            }
        }
    }
    found
}

/// Where `core.hooksPath` sends git for hooks, by the project's settings
/// and yours, and what yours include. A relative one is relative to the
/// project, from any of them.
fn hooks_paths(root: &Path, gits: &[PathBuf], home: Option<&Path>, env: Env) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = gits.iter().flat_map(|git| [git.join("config"), git.join("config.worktree")]).collect();
    if let Some(home) = home {
        files.extend(git_files(home, env));
    }
    files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| config_values(&text, &["core"], "hookspath"))
        .map(|value| home.and_then(|home| from_home(&value, home)).unwrap_or_else(|| root.join(&value)))
        .collect()
}

/// The values of `key` in a git config file's sections named one of
/// `sections`, in lower case: `[includeIf "gitdir:~/work/"]` is named
/// `includeif`. Keys don't count letter case either, as in git, and a
/// value may follow its section's name on the same line.
fn config_values(text: &str, sections: &[&str], key: &str) -> Vec<String> {
    let mut inside = false;
    let mut found = Vec::new();
    for line in text.lines().map(str::trim) {
        let mut line = line;
        if let Some(section) = line.strip_prefix('[') {
            let (header, rest) = section.split_once(']').unwrap_or((section, ""));
            let name = header.split(|c: char| c.is_whitespace() || c == '"' || c == '.').next().unwrap_or_default();
            inside = sections.iter().any(|wanted| name.eq_ignore_ascii_case(wanted));
            line = rest.trim();
        }
        if !inside {
            continue;
        }
        if let Some((name, value)) = line.split_once('=')
            && name.trim().eq_ignore_ascii_case(key)
        {
            let value = value.split(['#', ';']).next().unwrap_or_default().trim().trim_matches('"');
            if !value.is_empty() {
                found.push(value.to_owned());
            }
        }
    }
    found
}

/// The temp folders: the system's, and this user's own.
fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["/tmp", "/var/tmp"].iter().map(PathBuf::from).collect();
    dirs.extend(std::env::var_os("TMPDIR").map(PathBuf::from));
    #[cfg(target_os = "macos")]
    dirs.extend([libc::_CS_DARWIN_USER_TEMP_DIR, libc::_CS_DARWIN_USER_CACHE_DIR].into_iter().filter_map(confstr_path));
    let mut dirs: Vec<PathBuf> = dirs.iter().filter(|dir| dir.is_dir()).map(|dir| resolve(dir)).collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// A path the system names with `confstr`, like this user's temp folder.
#[cfg(target_os = "macos")]
fn confstr_path(name: libc::c_int) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; 1024];
    // SAFETY: confstr writes at most `buf.len()` bytes, its NUL included,
    // into `buf`, and returns the length it needed.
    let needed = unsafe { libc::confstr(name, buf.as_mut_ptr().cast(), buf.len()) };
    if needed == 0 || needed > buf.len() {
        return None;
    }
    buf.truncate(needed - 1);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-places-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        resolve(&dir)
    }

    #[test]
    fn protects_what_runs_later_and_the_folders_that_hold_it() {
        let root = project("protect");
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let places = Places::new(&root, None, None);
        for path in [".git/hooks/pre-commit", ".git/config", ".rein/permissions", ".vscode/tasks.json", ".envrc", "HEAD", ".git"] {
            assert!(places.protects(path), "{path}");
        }
        // Letter case doesn't get round it, nor `..`.
        for path in [".GIT/HOOKS/pre-commit", ".Rein/permissions", "src/../.git/config"] {
            assert!(places.protects(path), "{path}");
        }
        for path in ["src/main.rs", ".git/objects/ab/cdef", ".git/index", "HEADER.md", "notes/.git-like"] {
            assert!(!places.protects(path), "{path}");
        }
        // `.git` itself can't be renamed or replaced, though git writes in it.
        assert!(places.locked().contains(&root.join(".git")));
    }

    #[test]
    fn follows_a_symlink_to_what_it_protects() {
        let root = project("link");
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(root.join(".git/hooks"), root.join("h")).unwrap();
        let places = Places::new(&root, None, None);
        assert!(places.protects("h/pre-commit"));
    }

    #[test]
    fn a_worktrees_git_folder_is_writable_but_its_hooks_arent() {
        let root = project("worktree");
        let main = project("worktree-main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/wt").display())).unwrap();
        let places = Places::new(&root, None, None);
        assert!(places.writable().contains(&main.join(".git")), "{:?}", places.writable());
        assert!(places.protected().contains(&main.join(".git/hooks")));
        assert!(places.protected().contains(&main.join(".git/worktrees/wt/config.worktree")));
        // The folders between the shared git folder and this worktree's.
        assert!(places.locked().contains(&main.join(".git/worktrees/wt")));
    }

    #[test]
    fn protects_where_hooks_path_points_and_submodules_hooks() {
        let root = project("hookspath");
        std::fs::create_dir_all(root.join(".git/modules/vendor/lib")).unwrap();
        std::fs::write(root.join(".git/modules/vendor/lib/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n\thooksPath = .githooks  # ours\n[alias]\n\thooksPath = no\n").unwrap();
        let places = Places::new(&root, None, None);
        assert!(places.protects(".githooks/pre-push"));
        assert!(places.protects(".git/modules/vendor/lib/hooks/post-checkout"));
        assert!(places.protects(".git/modules/vendor/lib/config"));
        assert!(!places.protects(".git/modules/vendor/lib/objects/aa"));
    }

    #[test]
    fn reads_hooks_path_only_from_core() {
        let text = "[core]\nhooksPath = \"x y\"\n[user]\nhooksPath = z\n[Core]\nHOOKSPATH=w ; why\n[core] hooksPath = v\n";
        assert_eq!(config_values(text, &["core"], "hookspath"), ["x y", "w", "v"]);
        let text = "[include]\n\tpath = ~/.gitconfig.local\n[includeIf \"gitdir:~/work/\"]\n\tpath = work.inc\n[user]\n\tpath = no\n";
        assert_eq!(config_values(text, &["include", "includeif"], "path"), ["~/.gitconfig.local", "work.inc"]);
    }

    #[test]
    fn hides_secrets_and_reins_own_folder() {
        let root = project("hidden");
        let home = project("hidden-home");
        let places = Places::new(&root, Some(&home), Some(&home.join("rein-home")));
        for name in [".ssh", ".aws", ".config/gh", "Library/Keychains", "rein-home"] {
            assert!(places.hidden().contains(&home.join(name)), "{name}");
        }
        assert!(places.protected().contains(&home.join(".zshrc")));
    }

    #[test]
    fn says_what_it_hides_whatever_the_case_or_the_way_there() {
        let root = project("hides");
        let home = project("hides-home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::os::unix::fs::symlink(home.join(".ssh"), root.join("keys")).unwrap();
        let places = Places::new(&root, Some(&home), Some(&home.join("rein-home")));
        assert!(places.hides(&home.join(".ssh/id_ed25519")));
        assert!(places.hides(&home.join(".SSH/id_ed25519")), "letter case doesn't count");
        assert!(places.hides(&root.join("keys/id_ed25519")), "nor a symlink on the way");
        assert!(places.hides(&home.join("rein-home/keys.json")));
        assert!(!places.hides(&root.join("src/main.rs")));
        assert!(!places.hides(&home.join(".cargo/credentials.d/x")), "a name that only starts the same isn't hidden");
    }

    /// An environment with only `vars` in it.
    fn env(vars: &[(&str, PathBuf)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars.iter().map(|(name, value)| ((*name).to_owned(), value.clone().into_os_string())).collect();
        move |name| vars.iter().find(|(n, _)| n == name).map(|(_, value)| value.clone())
    }

    #[test]
    fn of_the_home_folder_only_the_project_and_what_tools_need_is_read() {
        let home = project("readable-home");
        let inside = home.join("work/app");
        std::fs::create_dir_all(&inside).unwrap();
        let path = std::env::join_paths([home.join(".opencode/bin"), home.clone(), PathBuf::from("/usr/bin")]).unwrap();
        let places = Places::with(&inside, Some(&home), None, &env(&[("PATH", PathBuf::from(path))]));
        for walled in [".zshrc", ".zshenv", ".bash_history", ".env", "notes.txt", ".config/github-copilot/apps.json", "Library/Application Support/App/login", "work", "work/other"]
        {
            assert!(places.hides(&home.join(walled)), "{walled}");
        }
        for readable in [".cargo/config.toml", ".rustup/settings.toml", ".gitconfig", ".config/git/config", ".opencode/bin/opencode", "work/app/src/main.rs"] {
            assert!(!places.hides(&home.join(readable)), "{readable}");
        }
        assert!(places.hides(&home.join(".cargo/credentials.toml")), "what's hidden stays hidden in what's readable");
        assert!(!places.readable().contains(&home), "not by being on PATH: {:?}", places.readable());
        assert!(!places.hides(Path::new("/usr/bin/env")), "outside the home folder, as before");
        // A project that is the home folder reads it all, but what's hidden.
        let places = Places::with(&home, Some(&home), None, &env(&[]));
        assert!(!places.hides(&home.join("notes.txt")) && places.hides(&home.join(".ssh/id_ed25519")));
    }

    #[test]
    fn what_a_variable_moves_is_hidden_or_readable_wherever_it_goes() {
        let root = project("moved");
        let home = project("moved-home");
        let away = project("moved-away");
        let kube = std::env::join_paths([away.join("kube/one"), away.join("kube/two")]).unwrap();
        let vars = [
            ("CLAUDE_CONFIG_DIR", away.join("claude")),
            ("XDG_CONFIG_HOME", home.join("cfg")),
            ("CARGO_HOME", home.join("tools/cargo")),
            ("AWS_SHARED_CREDENTIALS_FILE", away.join("aws-credentials")),
            ("KUBECONFIG", PathBuf::from(kube)),
            ("GNUPGHOME", away.join("gnupg")),
        ];
        let places = Places::with(&root, Some(&home), None, &env(&vars));
        for hidden in [
            away.join("claude/.credentials.json"),
            away.join("aws-credentials"),
            away.join("kube/one"),
            away.join("kube/two"),
            away.join("gnupg/x"),
            home.join("cfg/gh/hosts.yml"),
            home.join("cfg/git/credentials"),
            home.join("tools/cargo/credentials.toml"),
        ] {
            assert!(places.hides(&hidden), "{}", hidden.display());
        }
        for readable in [home.join("cfg/git/config"), home.join("tools/cargo/config.toml"), away.join("notes.txt")] {
            assert!(!places.hides(&readable), "{}", readable.display());
        }
        assert!(places.hides(&home.join(".config/gh/hosts.yml")), "and where it usually is");
    }

    #[test]
    fn what_git_reads_from_the_home_folder_is_readable() {
        let root = project("git-files");
        let home = project("git-files-home");
        std::fs::write(home.join(".gitconfig"), "[include]\n\tpath = ~/.gitconfig.local\n[core]\n\texcludesFile = ~/.gitignore_global\n").unwrap();
        std::fs::write(home.join(".gitconfig.local"), "[includeIf \"gitdir:~/work/\"]\n\tpath = work/.gitconfig\n[core]\n\thooksPath = ~/hooks\n").unwrap();
        let places = Places::with(&root, Some(&home), None, &env(&[]));
        for name in [".gitconfig.local", ".gitignore_global", "work/.gitconfig"] {
            assert!(!places.hides(&home.join(name)), "{name}");
        }
        assert!(places.hides(&home.join("work/notes.txt")));
        // An included file's hooksPath is where git runs hooks from too.
        assert!(places.protected().contains(&home.join("hooks")), "{:?}", places.protected());
    }
}
