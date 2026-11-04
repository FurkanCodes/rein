//! Reading and writing rein's local files, including credentials.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Error, Result};

/// How long a window waits for another that holds a lock to let it go.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// `$REIN_HOME` if set, otherwise `~/.config/rein`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("REIN_HOME") {
        return Some(dir.into());
    }
    std::env::home_dir().map(|home| home.join(".config").join("rein"))
}

/// Takes the lock at `path` that every rein window shares, waiting a while
/// for a window that holds it; `busy` is what's said if it doesn't let go.
/// It's let go when the file is dropped, or when its process ends however
/// it ends.
pub(crate) async fn lock_file(path: &Path, busy: &str) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Auth(busy.to_owned())),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Reads a JSON file, or returns `None` if it doesn't exist. One that isn't
/// as rein wrote it says where, never what's there: it may be a key.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| Error::Unreadable { path: path.to_owned(), line: e.line(), column: e.column() }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(value)?)
}

/// Writes a file only the current user can read (0600 on Unix, in a 0700
/// directory). It writes a temporary file and renames it over the target, so
/// a crash never leaves a half-written file behind.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        private_dir(dir)?;
    }
    // A name of its own for each write, so two rein windows saving the same
    // file at once can't rename each other's half-written one into place.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), WRITES.fetch_add(1, Ordering::Relaxed)));
    // The mode only applies to new files, so never reuse a leftover one.
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all()).and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// Makes `dir` a folder only you can open (0700), whether it's made now or
/// was there: one made by hand or by another program may be 0755, and
/// `DirBuilder`'s mode only applies to the folders it makes.
fn private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        if fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| std::io::Error::new(e.kind(), format!("can't make {} private: {e}", dir.display())))?;
        }
    }
    #[cfg(not(unix))]
    builder.create(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_file_says_where_but_never_whats_in_it() {
        let dir = std::env::temp_dir().join(format!("rein-store-damaged-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        for (text, line, column) in
            [("\"sk-test-FAKE-bare-string\"", 1, 26), ("{\n  \"openrouter\": \"sk-or-v1-FAKE\",\n  \"anthropic\": sk-ant-FAKE\n}", 3, 16), ("{\"anthropic\": \"sk-ant-FAKE", 1, 26)]
        {
            fs::write(&path, text).unwrap();
            let e = read_json::<std::collections::BTreeMap<String, String>>(&path).unwrap_err();
            let shown = e.to_string();
            assert!(matches!(&e, Error::Unreadable { line: l, column: c, .. } if (*l, *c) == (line, column)), "{e:?}");
            assert!(shown.starts_with(&path.display().to_string()) && shown.contains(&format!("line {line}, column {column}")), "{shown}");
            assert!(!shown.contains("FAKE") && !shown.contains("response"), "{shown}");
            assert!(!format!("{e:?}").contains("FAKE"), "{e:?}");
            assert!(std::error::Error::source(&e).is_none(), "the cause would quote it");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writers_at_the_same_time_never_leave_a_half_written_file() {
        let dir = std::env::temp_dir().join(format!("rein-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("keys.json");
        let writers: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..40 {
                        write_private(&path, format!("{{\"writer\":{n},\"write\":{i},\"pad\":\"{}\"}}", "x".repeat(4096)).as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved["writer"].is_u64(), "{saved}");
        let left: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["keys.json"], "no temporary files left");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_others_can_open_is_made_private_when_written_to() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rein-store-open-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        write_json_private(&dir.join("keys.json"), &serde_json::json!({ "openrouter": "sk-or-v1-FAKE" })).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700, "only you can open it now");
        assert_eq!(fs::metadata(dir.join("keys.json")).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }
}
