//! Where a sandboxed command's walls go, worked out once when rein starts:
//! the project and what in it is protected, git's own folders, the temp
//! folders, and what in your home folder is hidden.

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

/// In your home folder: keys, logins and tokens. A command in the sandbox
/// can neither read nor change these.
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
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/rein",
    ".claude",
    ".claude.json",
    ".codex",
    ".password-store",
    ".local/share/keyrings",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Arc",
];

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
    /// Never read or written.
    hidden: Vec<PathBuf>,
}

impl Places {
    /// For the project at `root`, with your home folder at `home`, and
    /// rein's own (keys, history, sessions) at `rein`.
    pub fn new(root: &Path, home: Option<&Path>, rein: Option<&Path>) -> Places {
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
        for hooks in hooks_paths(&root, &gits, home) {
            places.protect(&hooks);
        }
        if let Some(home) = home {
            let home = resolve(home);
            places.hidden.extend(HIDDEN.iter().map(|name| resolve(&home.join(name))));
            places.protected.extend(RUN_AT_LOGIN.iter().map(|name| resolve(&home.join(name))));
        }
        if let Some(rein) = rein {
            places.hidden.push(resolve(rein));
        }
        places.temp = temp_dirs();
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

    pub(crate) fn hidden(&self) -> &[PathBuf] {
        &self.hidden
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
    /// the model either. Symlinks are followed as far as the path exists,
    /// and letter case doesn't count, as on macOS.
    pub fn hides(&self, path: &Path) -> bool {
        let full = lower(&resolve(path));
        self.hidden.iter().any(|hidden| full.starts_with(lower(hidden)))
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
/// and yours. A relative one is relative to the project, from any of them.
fn hooks_paths(root: &Path, gits: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = gits.iter().flat_map(|git| [git.join("config"), git.join("config.worktree")]).collect();
    if let Some(home) = home {
        files.push(home.join(".gitconfig"));
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        files.push(xdg.join("git/config"));
    }
    files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| core_hooks_path(&text))
        .map(|value| match (value.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => root.join(value),
        })
        .collect()
}

/// The `hooksPath` values in a git config file's `[core]` sections.
fn core_hooks_path(text: &str) -> Vec<String> {
    let mut in_core = false;
    let mut found = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(section) = line.strip_prefix('[') {
            in_core = section.trim_end_matches(']').trim().eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("hookspath")
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
        assert_eq!(core_hooks_path("[core]\nhooksPath = \"x y\"\n[user]\nhooksPath = z\n[Core]\nHOOKSPATH=w ; why\n"), ["x y", "w"]);
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
        assert!(!places.hides(&home.join(".sshd_config")), "a name that only starts the same isn't hidden");
    }
}
