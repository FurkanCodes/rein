//! Reading and writing rein's local files, including credentials.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Result;

/// `$REIN_HOME` if set, otherwise `~/.config/rein`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("REIN_HOME") {
        return Some(dir.into());
    }
    std::env::home_dir().map(|home| home.join(".config").join("rein"))
}

/// Reads a JSON file, or returns `None` if it doesn't exist.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
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
