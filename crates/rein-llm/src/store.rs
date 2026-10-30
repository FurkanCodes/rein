//! Reading and writing rein's local files, including credentials.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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
    if let Some(dir) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    let tmp = path.with_extension("tmp");
    // The mode only applies to new files, so never reuse a leftover one.
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}
