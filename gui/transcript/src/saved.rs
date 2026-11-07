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
    fn a_folders_name_is_its_last_part() {
        assert_eq!(folder_name("/work/rein"), "rein");
        assert_eq!(folder_name("/work/rein/"), "rein");
        assert_eq!(folder_name("/"), "/");
        assert_eq!(folder_name(""), "");
    }
}
