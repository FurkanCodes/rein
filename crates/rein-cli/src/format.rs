//! Small things said the same way everywhere: sizes, counts, paths,
//! times and email addresses.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The first line of `text`, at most `max` characters.
pub(crate) fn shorten(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= max && !text.trim_end().contains('\n') {
        return line.to_owned();
    }
    let cut: String = line.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

/// `path` relative to the working folder (`../AGENTS.md`), so it reads
/// shorter; absolute when they only share the filesystem root.
pub(crate) fn shown_path(path: &Path, cwd: &Path) -> String {
    for (up, folder) in cwd.ancestors().enumerate() {
        if folder.parent().is_none() {
            break;
        }
        if let Ok(rest) = path.strip_prefix(folder) {
            return format!("{}{}", "../".repeat(up), rest.display());
        }
    }
    path.display().to_string()
}

/// A moment in the computer's local time zone.
pub(crate) struct LocalTime {
    year: i32,
    month: u32,
    day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    pub(crate) second: u32,
}

impl LocalTime {
    pub(crate) fn date(&self) -> String {
        format!("{}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

pub(crate) fn local_time(time: SystemTime) -> LocalTime {
    let seconds = time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value, and localtime_r only
    // writes into the one we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

/// What the requests cost on the wire: `412 KB as 128 KB (gzip, 3.2× fewer bytes)`.
pub(crate) fn traffic(t: rein_llm::Traffic) -> String {
    use rein_llm::Compression;
    if t.json == 0 {
        return "nothing yet".to_owned();
    }
    match t.compression {
        Compression::Gzip if t.sent < t.json => format!("{} as {} (gzip, {:.1}× fewer bytes)", bytes(t.json), bytes(t.sent), t.json as f64 / t.sent as f64),
        Compression::Gzip => format!("{} (too little to compress yet)", bytes(t.sent)),
        Compression::Off => format!("{}, uncompressed: compress is off in the settings", bytes(t.sent)),
        Compression::Refused => format!("{}, uncompressed: the server doesn't take compressed requests", bytes(t.sent)),
    }
}

/// `950 bytes`, `412 KB`, `3.1 MB`.
pub(crate) fn bytes(n: u64) -> String {
    match n {
        0..1000 => format!("{n} bytes"),
        1000..1_000_000 => format!("{} KB", n.div_ceil(1000)),
        _ => format!("{:.1} MB", n as f64 / 1e6),
    }
}

/// `12345` → `12k`: token counts read at a glance.
pub(crate) fn thousands(n: u64) -> String {
    if n < 1000 { n.to_string() } else { format!("{}k", (n + 500) / 1000) }
}

/// `someone@example.com` -> `so•••@example.com`: enough to recognize your own
/// account, safe to show on a screen recording or stream.
pub(crate) fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((name, domain)) => format!("{}•••@{domain}", name.chars().take(2).collect::<String>()),
        None => "•••".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_emails() {
        assert_eq!(mask_email("someone@example.com"), "so•••@example.com");
        assert_eq!(mask_email("a@example.com"), "a•••@example.com");
        assert_eq!(mask_email("not-an-email"), "•••");
    }

    #[test]
    fn says_how_many_bytes() {
        assert_eq!(bytes(950), "950 bytes");
        assert_eq!(bytes(412_300), "413 KB");
        assert_eq!(bytes(3_140_000), "3.1 MB");
    }
}
