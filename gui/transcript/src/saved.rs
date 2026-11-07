//! Conversations saved on disk, as `session/list` gives them. They're the
//! same ones the terminal keeps, so what you started there can go on here.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub id: String,
    pub cwd: String,
    /// What it started with, if it says.
    pub title: Option<String>,
    /// When it was last used, as the agent wrote it.
    pub updated: Option<String>,
}

impl Saved {
    /// The conversations in a `session/list`'s `result`. One that names no
    /// conversation or no folder can't be opened, so it isn't listed.
    pub fn listed(result: &Value) -> Vec<Saved> {
        let Some(sessions) = result["sessions"].as_array() else { return Vec::new() };
        sessions
            .iter()
            .filter_map(|session| {
                Some(Saved {
                    id: session["sessionId"].as_str()?.to_owned(),
                    cwd: session["cwd"].as_str()?.to_owned(),
                    title: session["title"].as_str().map(str::to_owned),
                    updated: session["updatedAt"].as_str().map(str::to_owned),
                })
            })
            .collect()
    }

    /// The name of the folder it was held in.
    pub fn folder(&self) -> &str {
        folder_name(&self.cwd)
    }

    /// Its day, from the agent's stamp.
    pub fn day(&self) -> Option<&str> {
        self.updated.as_deref().and_then(|stamp| stamp.get(..10))
    }

    /// How long ago it was last used, at `now` (seconds since 1970), from
    /// the agent's stamp: `2026-11-07T10:00:00Z`. A stamp from later than
    /// now is now.
    pub fn age(&self, now: u64) -> Option<std::time::Duration> {
        let then = seconds_of(self.updated.as_deref()?)?;
        Some(std::time::Duration::from_secs(now.saturating_sub(then)))
    }
}

/// The seconds since 1970 that a UTC stamp `YYYY-MM-DDTHH:MM:SSZ` names.
fn seconds_of(stamp: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<u64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days from 1970 to the civil date, as Howard Hinnant's algorithm has it.
    let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
    let era = y / 400;
    let of_era = y - era * 400;
    let of_year = (153 * m + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let days = (era * 146_097 + of_cycle).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// The last part of the path `cwd`.
pub fn folder_name(cwd: &str) -> &str {
    let trimmed = cwd.trim_end_matches('/');
    trimmed.rsplit('/').next().filter(|name| !name.is_empty()).unwrap_or(cwd)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_list_is_read_and_what_cant_be_opened_is_left_out() {
        let result = json!({ "sessions": [
            { "sessionId": "abc", "cwd": "/work/rein", "title": "fix the tests", "updatedAt": "2026-11-07T10:00:00Z" },
            { "sessionId": "no-title", "cwd": "/work/other", "title": null },
            { "cwd": "/work/no-id" },
            { "sessionId": "no-folder" },
        ], "nextCursor": "50" });
        let saved = Saved::listed(&result);
        assert_eq!(saved.iter().map(|saved| saved.id.as_str()).collect::<Vec<_>>(), ["abc", "no-title"]);
        assert_eq!(saved[0].title.as_deref(), Some("fix the tests"));
        assert_eq!(saved[0].folder(), "rein");
        assert_eq!(saved[0].day(), Some("2026-11-07"));
        assert_eq!((saved[1].title.as_deref(), saved[1].day()), (None, None));
        assert!(Saved::listed(&json!({})).is_empty());
    }

    #[test]
    fn an_age_comes_from_the_stamp_and_what_isnt_one_has_none() {
        let at = |stamp: &str| Saved { id: String::new(), cwd: String::new(), title: None, updated: Some(stamp.to_owned()) };
        // 2026-11-07T10:00:00Z, by `date -u -j -f %Y-%m-%dT%H:%M:%S 2026-11-07T10:00:00 +%s`.
        let now = 1_794_045_600;
        assert_eq!(seconds_of("2026-11-07T10:00:00Z"), Some(now));
        assert_eq!(seconds_of("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(seconds_of("2024-02-29T00:00:00Z"), Some(1_709_164_800), "a leap day");
        assert_eq!(at("2026-11-07T09:58:00Z").age(now), Some(std::time::Duration::from_secs(120)));
        assert_eq!(at("2026-11-07T10:05:00Z").age(now), Some(std::time::Duration::ZERO), "from later than now is now");
        assert_eq!(at("yesterday").age(now), None);
        assert_eq!(seconds_of("2026-13-07T10:00:00Z"), None);
    }

    #[test]
    fn a_folders_name_is_its_last_part() {
        assert_eq!(folder_name("/work/rein"), "rein");
        assert_eq!(folder_name("/work/rein/"), "rein");
        assert_eq!(folder_name("/"), "/");
        assert_eq!(folder_name(""), "");
    }
}
