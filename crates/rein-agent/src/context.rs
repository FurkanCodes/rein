//! What the model is told before the first message: where it's working,
//! today's date, and the project's own instructions from AGENTS.md files.
//!
//! It goes in as the conversation's first message rather than into the
//! system prompt, and it's saved with the conversation, so a continued
//! conversation starts with exactly the same text and keeps its cache.

use std::fs;
use std::path::{Path, PathBuf};

use rein_llm::Item;

/// Instruction files are cut off after this many bytes in total, like
/// Codex's default `project_doc_max_bytes`.
pub const MAX_BYTES: usize = 32 * 1024;

/// One instruction file and what's in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    pub path: PathBuf,
    pub text: String,
}

/// The instruction files that apply in `cwd`, most general first: the
/// user's own `personal` file, then one per folder from the project root
/// down to `cwd`. The root is the nearest folder holding `.git`; outside a
/// repository only `cwd` itself counts. In each folder AGENTS.md is read,
/// or CLAUDE.md if there's no AGENTS.md.
pub fn find(cwd: &Path, personal: Option<&Path>) -> Vec<Instructions> {
    let root = cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(cwd);
    let mut folders: Vec<&Path> = cwd.ancestors().take_while(|dir| *dir != root).collect();
    folders.push(root);
    folders.reverse();

    let mut paths: Vec<PathBuf> = personal.map(Path::to_owned).into_iter().collect();
    for folder in folders {
        let agents = folder.join("AGENTS.md");
        paths.push(if agents.is_file() { agents } else { folder.join("CLAUDE.md") });
    }

    let mut budget = MAX_BYTES;
    let mut found = Vec::new();
    for path in paths {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        if text.trim().is_empty() || budget == 0 {
            continue;
        }
        let text = cut(&text, budget);
        budget -= text.len();
        found.push(Instructions { path, text });
    }
    found
}

/// At most `max` bytes of `text`, cut at a character boundary.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Whether `item` is the first message, built by [`message`].
pub fn is_context(item: &Item) -> bool {
    matches!(item, Item::User(text) if text.starts_with("<environment>"))
}

/// The first message: the environment, then each instruction file. The
/// date has no time of day, so it changes at most once a day.
pub fn message(cwd: &Path, os: &str, date: &str, files: &[Instructions]) -> String {
    let mut text = format!("<environment>\nWorking directory: {}\nOperating system: {os}\nToday's date: {date}\n</environment>", cwd.display());
    for file in files {
        text.push_str(&format!("\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>", file.path.display(), file.text.trim_end()));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-context-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo/.git")).unwrap();
        fs::create_dir_all(dir.join("repo/app/src")).unwrap();
        dir
    }

    fn paths(found: &[Instructions]) -> Vec<String> {
        found.iter().map(|f| f.text.clone()).collect()
    }

    #[test]
    fn reads_from_the_project_root_down_to_the_working_folder() {
        let dir = tree("walk");
        fs::write(dir.join("AGENTS.md"), "above the repository").unwrap();
        fs::write(dir.join("repo/AGENTS.md"), "root").unwrap();
        fs::write(dir.join("repo/app/AGENTS.md"), "app").unwrap();
        fs::write(dir.join("me.md"), "mine").unwrap();
        let found = find(&dir.join("repo/app/src"), Some(&dir.join("me.md")));
        assert_eq!(paths(&found), ["mine", "root", "app"]);
        assert_eq!(found[1].path, dir.join("repo/AGENTS.md"));
    }

    #[test]
    fn falls_back_to_claude_md_per_folder() {
        let dir = tree("fallback");
        fs::write(dir.join("repo/CLAUDE.md"), "claude root").unwrap();
        fs::write(dir.join("repo/app/AGENTS.md"), "agents app").unwrap();
        fs::write(dir.join("repo/app/CLAUDE.md"), "claude app").unwrap();
        assert_eq!(paths(&find(&dir.join("repo/app"), None)), ["claude root", "agents app"]);
    }

    #[test]
    fn outside_a_repository_only_the_working_folder_counts() {
        let dir = tree("norepo");
        fs::create_dir_all(dir.join("plain/sub")).unwrap();
        fs::write(dir.join("plain/AGENTS.md"), "parent").unwrap();
        fs::write(dir.join("plain/sub/AGENTS.md"), "here").unwrap();
        assert_eq!(paths(&find(&dir.join("plain/sub"), None)), ["here"]);
    }

    #[test]
    fn stops_at_the_size_limit() {
        let dir = tree("limit");
        fs::write(dir.join("repo/AGENTS.md"), "é".repeat(MAX_BYTES)).unwrap();
        fs::write(dir.join("repo/app/AGENTS.md"), "never reached").unwrap();
        let found = find(&dir.join("repo/app"), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text.len(), MAX_BYTES);
    }

    #[test]
    fn builds_the_first_message() {
        let files = [Instructions { path: "/p/AGENTS.md".into(), text: "Use tabs.\n".into() }];
        assert_eq!(
            message(Path::new("/p"), "macos", "2026-11-01", &files),
            "<environment>\nWorking directory: /p\nOperating system: macos\nToday's date: 2026-11-01\n</environment>\n\n\
             <project_instructions path=\"/p/AGENTS.md\">\nUse tabs.\n</project_instructions>"
        );
    }
}
