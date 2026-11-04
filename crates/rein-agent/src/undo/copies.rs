//! Where the copies of files are kept: in memory, or in rein's folder for
//! the project, in a folder of each conversation's own. A conversation's
//! copies are only its own, so clearing them out never takes another's.
//! Clones, where the file system makes them, share what's in them with the
//! file and with each other, so a copy per conversation takes no more room.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::{Seen, Turn};
use crate::files;

/// The file that holds a conversation's turns, in its folder.
const TURNS_FILE: &str = "turns.json";
/// Another conversation's copies go once it hasn't been used for this long,
const UNUSED: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// or once this many have been used since.
const CONVERSATIONS: usize = 10;

pub(super) enum Copies {
    /// For rein's own file changes only, as long as rein runs.
    Memory(HashMap<String, Vec<u8>>),
    /// In `own`, the conversation's folder in rein's folder for the project.
    Disk { own: PathBuf },
}

impl Copies {
    pub(super) fn on_disk(&self) -> bool {
        matches!(self, Copies::Disk { .. })
    }

    /// Keeps a copy of each file in `files`, inside `root`, each found as
    /// its `Seen` says, on several threads at once when they're on disk:
    /// each with its copy's name, or none if it isn't what `Seen` says any
    /// more (it's being written), or can't be read.
    pub(super) fn keep(&mut self, root: &Path, files: Vec<(String, Seen)>) -> Vec<(String, Seen)> {
        match self {
            Copies::Memory(copies) => files
                .into_iter()
                .map(|(rel, mut seen)| {
                    let name = seen.name();
                    if let Ok(bytes) = read(root, &rel, &seen) {
                        copies.insert(name.clone(), bytes);
                        seen.copy = Some(name);
                    }
                    (rel, seen)
                })
                .collect(),
            Copies::Disk { own } => {
                if files.is_empty() || private_dir(own).is_err() {
                    return files;
                }
                let own = &*own;
                let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
                let chunk = files.len().div_ceil(threads);
                let mut files = files;
                std::thread::scope(|scope| {
                    for part in files.chunks_mut(chunk) {
                        scope.spawn(move || {
                            for (rel, seen) in part {
                                let name = seen.name();
                                seen.copy = keep_on_disk(own, root, rel, seen, &name).ok().map(|()| name);
                            }
                        });
                    }
                });
                files
            }
        }
    }

    /// What the copy `name` holds.
    pub(super) fn bytes(&self, name: &str) -> io::Result<Vec<u8>> {
        match self {
            Copies::Memory(copies) => copies.get(name).cloned().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "its copy is gone")),
            Copies::Disk { own } => std::fs::read(own.join(name)),
        }
    }

    /// Lets go of every copy but those `wanted`.
    pub(super) fn tidy(&mut self, wanted: &BTreeSet<&str>) {
        match self {
            Copies::Memory(copies) => copies.retain(|name, _| wanted.contains(name.as_str())),
            Copies::Disk { own } => {
                let Ok(entries) = std::fs::read_dir(&*own) else { return };
                for entry in entries.filter_map(Result::ok) {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Half-made ones too: only this rein writes here.
                    if name != TURNS_FILE && !wanted.contains(name.as_str()) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    /// The turns kept from before, if this conversation has been here.
    pub(super) fn turns(&self) -> Vec<Turn> {
        match self {
            Copies::Memory(_) => Vec::new(),
            // One that can't be read, say tampered with, is started over.
            Copies::Disk { own } => rein_llm::store::read_json(&own.join(TURNS_FILE)).ok().flatten().unwrap_or_default(),
        }
    }

    /// Keeps `turns` for when this conversation is continued.
    pub(super) fn save(&self, turns: &[Turn]) {
        if let Copies::Disk { own } = self {
            let _ = rein_llm::store::write_json_private(&own.join(TURNS_FILE), &turns);
        }
    }

    /// Takes away other conversations' copies, once they're a week unused
    /// or ten more have been used since.
    pub(super) fn sweep(&self) {
        let Copies::Disk { own } = self else { return };
        let Some(project) = own.parent() else { return };
        let Ok(entries) = std::fs::read_dir(project) else { return };
        let mut others: Vec<(SystemTime, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != *own && entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| {
                let used = std::fs::metadata(entry.path().join(TURNS_FILE)).or_else(|_| entry.metadata()).and_then(|meta| meta.modified());
                (used.unwrap_or(SystemTime::UNIX_EPOCH), entry.path())
            })
            .collect();
        others.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
        let now = SystemTime::now();
        for (at, (used, path)) in others.iter().enumerate() {
            if at >= CONVERSATIONS || now.duration_since(*used).is_ok_and(|since| since > UNUSED) {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}

/// `rel` inside `root`, opened never through a symlink, if it's still
/// the file `seen` says.
fn open(root: &Path, rel: &str, seen: &Seen) -> io::Result<File> {
    let file = files::open(root, Path::new(rel))?;
    unchanged(&file, seen)?;
    Ok(file)
}

/// What `rel` holds, if it's still the file `seen` says once it's read.
fn read(root: &Path, rel: &str, seen: &Seen) -> io::Result<Vec<u8>> {
    let mut file = open(root, rel, seen)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    unchanged(&file, seen)?;
    Ok(bytes)
}

fn unchanged(file: &File, seen: &Seen) -> io::Result<()> {
    if Seen::of(&file.metadata()?).is_some_and(|now| now.same(seen)) { Ok(()) } else { Err(io::Error::other("it changed while rein kept it")) }
}

/// The copy `name` of `rel`, in the conversation's folder (made already):
/// there already, or made.
fn keep_on_disk(own: &Path, root: &Path, rel: &str, seen: &Seen, name: &str) -> io::Result<()> {
    let mine = own.join(name);
    if mine.exists() {
        return Ok(());
    }
    let file = open(root, rel, seen)?;
    match clone(&file, &mine) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
        Ok(()) => {}
    }
    unchanged(&file, seen).inspect_err(|_| {
        let _ = std::fs::remove_file(&mine);
    })
}

/// `file` copied to `to`, a new file, whole or not at all: a clone, which
/// shares what's in it until either changes, where the file system makes
/// them.
#[cfg(target_os = "macos")]
fn clone(file: &File, to: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: `file` is open and `path` a NUL-terminated string, both alive
    // for the call; fclonefileat makes a new file at `path`, or fails.
    if unsafe { libc::fclonefileat(file.as_raw_fd(), libc::AT_FDCWD, path.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::AlreadyExists {
        return Err(e);
    }
    // Another volume, or one that can't clone: copied.
    copied(file, to)
}

/// `file` copied to `to`, a new file, whole or not at all. On Linux the
/// kernel copies it (`copy_file_range`), which Btrfs and XFS make a clone.
#[cfg(not(target_os = "macos"))]
fn clone(file: &File, to: &Path) -> io::Result<()> {
    copied(file, to)
}

/// `file` copied to a file of its own beside `to`, then named `to`: a half
/// copy is never there under the name.
fn copied(file: &File, to: &Path) -> io::Result<()> {
    if to.exists() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    static MADE: AtomicU64 = AtomicU64::new(0);
    let name = to.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let half = to.with_file_name(format!(".{name}.{}.{}", std::process::id(), MADE.fetch_add(1, Ordering::Relaxed)));
    let made = copy(file, &half).and_then(|()| std::fs::rename(&half, to));
    if made.is_err() {
        let _ = std::fs::remove_file(&half);
    }
    made
}

fn copy(mut file: &File, to: &Path) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(to)?;
    io::copy(&mut file, &mut out)?;
    Ok(())
}

/// `dir`, made if it isn't there, only you able to open it.
fn private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How a copy is made where nothing clones, as on Linux's ext4.
    #[test]
    fn a_plain_copy_is_whole_under_its_name_or_not_there() {
        let dir = std::env::temp_dir().join(format!("rein-undo-copies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "one\n").unwrap();
        let file = File::open(dir.join("a.rs")).unwrap();
        copied(&file, &dir.join("copy")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("copy")).unwrap(), "one\n");
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(dir.join("copy")).unwrap().permissions()) & 0o777, 0o600, "only yours");
        assert_eq!(copied(&file, &dir.join("copy")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "no half copy left: {names:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
