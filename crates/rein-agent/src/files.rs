//! Opening files in the project without being led out of it.
//!
//! A path is checked once, resolved, and found inside the project. But a
//! command left running in the background can swap a folder on the way, or
//! the file itself, for a symlink between that check and the open, and the
//! open would follow it to, say, your keys. So the resolved path is opened
//! one folder at a time from the project's own, never following a symlink:
//! whatever was swapped in, the open fails instead of leaving the project.

use std::fs::File;
use std::io;
use std::path::{Component, Path};

/// Opens `rel`, a path inside `root` as resolved (no `..`, no symlinks in
/// it), to read.
pub(crate) fn open(root: &Path, rel: &Path) -> io::Result<File> {
    imp::open(root, rel)
}

/// Opens `rel` inside `root` to write, emptied, or made if it isn't there,
/// with the folders on the way to it.
pub(crate) fn create(root: &Path, rel: &Path) -> io::Result<File> {
    imp::create(root, rel)
}

/// Removes the file `rel` inside `root`.
pub(crate) fn remove(root: &Path, rel: &Path) -> io::Result<()> {
    imp::remove(root, rel)
}

/// The parts of `rel`, every one a plain name: what isn't, isn't opened.
fn names(rel: &Path) -> io::Result<Vec<&std::ffi::OsStr>> {
    rel.components()
        .map(|part| match part {
            Component::Normal(name) => Ok(name),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "not a plain path inside the project")),
        })
        .collect()
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `name` in the folder `dir`, opened with `flags`, never through a
    /// symlink.
    fn at(dir: &OwnedFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
        let name = CString::new(name.as_bytes())?;
        let mode: libc::c_uint = 0o666;
        // SAFETY: `dir` is an open descriptor and `name` a NUL-terminated
        // string, both alive for the call; openat returns a new descriptor
        // we own, or -1.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat just returned this descriptor, and nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The folder `parts` lead to from `root`, each opened in the last;
    /// made where it's missing if `make`.
    fn folder(root: &Path, parts: &[&OsStr], make: bool) -> io::Result<OwnedFd> {
        let mut dir = OwnedFd::from(File::open(root)?);
        for &name in parts {
            let next = match at(&dir, name, libc::O_RDONLY | libc::O_DIRECTORY) {
                Err(e) if make && e.kind() == io::ErrorKind::NotFound => {
                    let c = CString::new(name.as_bytes())?;
                    // SAFETY: as in `at`; mkdirat makes a folder, or fails.
                    if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o777) } < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() != io::ErrorKind::AlreadyExists {
                            return Err(e);
                        }
                    }
                    at(&dir, name, libc::O_RDONLY | libc::O_DIRECTORY)?
                }
                other => other?,
            };
            dir = next;
        }
        Ok(dir)
    }

    /// The folder `rel` is in, and its own name.
    fn split<'a>(root: &Path, rel: &'a Path, make: bool) -> io::Result<(OwnedFd, &'a OsStr)> {
        let parts = super::names(rel)?;
        let (name, parents) = parts.split_last().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file"))?;
        Ok((folder(root, parents, make)?, name))
    }

    pub(super) fn open(root: &Path, rel: &Path) -> io::Result<File> {
        let (dir, name) = split(root, rel, false)?;
        Ok(File::from(at(&dir, name, libc::O_RDONLY)?))
    }

    pub(super) fn create(root: &Path, rel: &Path) -> io::Result<File> {
        let (dir, name) = split(root, rel, true)?;
        Ok(File::from(at(&dir, name, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC)?))
    }

    pub(super) fn remove(root: &Path, rel: &Path) -> io::Result<()> {
        let (dir, name) = split(root, rel, false)?;
        let c = CString::new(name.as_bytes())?;
        // SAFETY: as in `at`; unlinkat removes the name, never what a link
        // points to.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub(super) fn open(root: &Path, rel: &Path) -> io::Result<File> {
        super::names(rel)?;
        File::open(root.join(rel))
    }

    pub(super) fn create(root: &Path, rel: &Path) -> io::Result<File> {
        super::names(rel)?;
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        File::create(path)
    }

    pub(super) fn remove(root: &Path, rel: &Path) -> io::Result<()> {
        super::names(rel)?;
        std::fs::remove_file(root.join(rel))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn place(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("rein-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, outside) = (base.join("project"), base.join("outside"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keys.json"), "sk-test-FAKE").unwrap();
        (root.canonicalize().unwrap(), outside.canonicalize().unwrap())
    }

    fn read(root: &Path, rel: &str) -> io::Result<String> {
        let mut text = String::new();
        open(root, Path::new(rel))?.read_to_string(&mut text)?;
        Ok(text)
    }

    #[test]
    fn reads_and_writes_inside_the_project() {
        let (root, _) = place("inside");
        create(&root, Path::new("src/new/deep.txt")).unwrap().write_all(b"hello").unwrap();
        assert_eq!(read(&root, "src/new/deep.txt").unwrap(), "hello");
        remove(&root, Path::new("src/new/deep.txt")).unwrap();
        assert!(!root.join("src/new/deep.txt").exists());
    }

    #[test]
    fn a_symlink_swapped_in_anywhere_on_the_way_is_never_followed() {
        let (root, outside) = place("swapped");
        // The file itself, a link out.
        std::os::unix::fs::symlink(outside.join("keys.json"), root.join("src/x")).unwrap();
        assert!(read(&root, "src/x").is_err(), "the file a link");
        assert!(create(&root, Path::new("src/x")).is_err(), "nor written through");
        assert_eq!(std::fs::read_to_string(outside.join("keys.json")).unwrap(), "sk-test-FAKE");
        // A folder on the way, a link out.
        std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();
        assert!(read(&root, "d/keys.json").is_err(), "a folder a link");
        assert!(create(&root, Path::new("d/planted")).is_err());
        assert!(!outside.join("planted").exists());
        assert!(remove(&root, Path::new("d/keys.json")).is_err());
        assert!(outside.join("keys.json").exists());
        // And nothing but plain names.
        assert!(read(&root, "../outside/keys.json").is_err());
    }
}
