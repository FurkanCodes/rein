//! What an agent says it changed, read here once, and what you say back
//! about it. A comment is yours until you send it, and goes as your words.

use serde_json::Value;

/// How far back to look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Turn,
    Conversation,
}

impl Span {
    /// What an agent calls it.
    pub const fn id(self) -> &'static str {
        match self {
            Span::Turn => "turn",
            Span::Conversation => "conversation",
        }
    }
}

/// What's become of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Became {
    Changed,
    Made,
    Removed,
}

/// Why a file's lines don't show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unshown {
    Binary,
    TooBig,
    NotKept,
}

impl Unshown {
    /// Said where its lines would be.
    pub const fn says(self) -> &'static str {
        match self {
            Unshown::Binary => "This file isn't text, so it has no lines to show.",
            Unshown::TooBig => "This file is too long to show here.",
            Unshown::NotKept => "rein has no copy of how this file was.",
        }
    }
}

/// A file a span changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub became: Became,
    pub added: u64,
    pub removed: u64,
    pub unshown: Option<Unshown>,
    /// You or another program changed it since: undoing leaves it alone.
    pub since: bool,
    /// Its lines as a unified patch, if they were asked for.
    pub patch: Option<String>,
}

/// One side of a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// As it was: a line taken out is only here.
    Old,
    /// As it is.
    New,
}

impl ChangedFile {
    fn of(file: &Value) -> Option<ChangedFile> {
        let became = match file["became"].as_str()? {
            "changed" => Became::Changed,
            "made" => Became::Made,
            "removed" => Became::Removed,
            _ => return None,
        };
        let unshown = match file["unshown"].as_str() {
            Some("binary") => Some(Unshown::Binary),
            Some("too_big") => Some(Unshown::TooBig),
            Some("not_kept") => Some(Unshown::NotKept),
            _ => None,
        };
        Some(ChangedFile {
            path: file["path"].as_str()?.to_owned(),
            became,
            added: file["added"].as_u64().unwrap_or(0),
            removed: file["removed"].as_u64().unwrap_or(0),
            unshown,
            since: file["since"] == true,
            patch: file["patch"].as_str().map(str::to_owned),
        })
    }

    /// The word beside its name, if it needs one.
    pub const fn tag(&self) -> &'static str {
        match (self.since, self.became) {
            (true, _) => "changed since",
            (false, Became::Made) => "made",
            (false, Became::Removed) => "removed",
            (false, Became::Changed) => "",
        }
    }

    /// What line `number` of `side` says, as far as the patch shows it.
    pub fn line(&self, side: Side, number: usize) -> Option<&str> {
        let (mut old, mut new) = (0, 0);
        let mut body = false;
        for line in self.patch.as_deref()?.lines() {
            if let Some(hunk) = line.strip_prefix("@@ -") {
                (old, new) = hunk_start(hunk)?;
                body = true;
                continue;
            }
            if !body {
                continue;
            }
            let (mark, text) = line.split_at_checked(1)?;
            let (on_old, on_new) = (mark == " " || mark == "-", mark == " " || mark == "+");
            old = old.saturating_add(usize::from(on_old));
            new = new.saturating_add(usize::from(on_new));
            let here = match side {
                Side::Old => on_old.then_some(old),
                Side::New => on_new.then_some(new),
            };
            if here == Some(number) {
                return Some(text);
            }
        }
        None
    }
}

/// The lines before a hunk on each side, from what follows `@@ -`.
fn hunk_start(hunk: &str) -> Option<(usize, usize)> {
    let (old, rest) = hunk.split_once(" +")?;
    let new = rest.split(" @@").next()?;
    let start = |range: &str| range.split(',').next()?.parse::<usize>().ok().map(|line| line.saturating_sub(1));
    Some((start(old)?, start(new)?))
}

/// What a span changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// The last turn that changed anything, to undo.
    pub turn: Option<usize>,
    pub files: Vec<ChangedFile>,
}

impl Changes {
    pub fn of(said: &Value) -> Changes {
        let files = said["files"].as_array().map(Vec::as_slice).unwrap_or_default();
        Changes { turn: said["turn"].as_u64().and_then(|turn| usize::try_from(turn).ok()), files: files.iter().filter_map(ChangedFile::of).collect() }
    }

    /// How many files, and the lines added and removed in them.
    pub fn totals(&self) -> Totals {
        Totals { files: self.files.len(), added: self.files.iter().map(|file| file.added).sum(), removed: self.files.iter().map(|file| file.removed).sum() }
    }

    pub fn file(&self, path: &str) -> Option<&ChangedFile> {
        self.files.iter().find(|file| file.path == path)
    }
}

/// How much changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: usize,
    pub added: u64,
    pub removed: u64,
}

impl Totals {
    /// In words: `3 files`.
    pub fn files_said(self) -> String {
        if self.files == 1 { "1 file".to_owned() } else { format!("{} files", self.files) }
    }
}

/// What you said about some lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub path: String,
    pub side: Side,
    pub first: usize,
    pub last: usize,
    /// The line it's on, for the model to find it however the file moves.
    pub quote: Option<String>,
    pub text: String,
}

impl Comment {
    /// Where it is, in words.
    pub fn place(&self) -> String {
        let removed = if self.side == Side::Old { "removed " } else { "" };
        if self.first == self.last { format!("{}, {removed}line {}", self.path, self.last) } else { format!("{}, {removed}lines {} to {}", self.path, self.first, self.last) }
    }
}

/// `comments` as one message. `None` for none: there's nothing to say.
pub fn comments_said(comments: &[Comment]) -> Option<String> {
    if comments.is_empty() {
        return None;
    }
    let mut said = "Comments on what you changed:\n".to_owned();
    for comment in comments {
        said.push('\n');
        said.push_str(&comment.place());
        said.push('\n');
        // The blank line ends the quote: Markdown runs what follows into it.
        if let Some(quote) = &comment.quote {
            said.push_str(&format!("> {quote}\n\n"));
        }
        said.push_str(comment.text.trim());
        said.push('\n');
    }
    Some(said)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PATCH: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -10,3 +10,4 @@ fn a\n one\n-two\n+TWO\n+three\n four\n";

    fn file(patch: Option<&str>) -> ChangedFile {
        ChangedFile { path: "a.rs".to_owned(), became: Became::Changed, added: 2, removed: 1, unshown: None, since: false, patch: patch.map(str::to_owned) }
    }

    #[test]
    fn what_rein_says_changed_reads_into_its_types() {
        let said = json!({ "turn": 2, "files": [
            { "path": "a.rs", "became": "changed", "added": 2, "removed": 1, "unshown": null, "since": false, "patch": PATCH },
            { "path": "logo.png", "became": "made", "added": 0, "removed": 0, "unshown": "binary", "since": false },
            { "path": "b.rs", "became": "removed", "added": 0, "removed": 4, "unshown": null, "since": true },
            { "path": "odd", "became": "renamed" },
        ] });
        let changes = Changes::of(&said);
        assert_eq!(changes.turn, Some(2));
        assert_eq!(changes.files.len(), 3, "what isn't understood isn't guessed at");
        assert_eq!(changes.totals(), Totals { files: 3, added: 2, removed: 5 });
        assert_eq!(changes.file("logo.png").map(|file| (file.unshown, file.tag())), Some((Some(Unshown::Binary), "made")));
        assert_eq!(changes.file("b.rs").map(ChangedFile::tag), Some("changed since"), "what undoing leaves alone is said before what it was");
        assert_eq!(changes.file("a.rs").map(ChangedFile::tag), Some(""));
        assert_eq!(Changes::of(&json!({})), Changes::default());
    }

    #[test]
    fn a_line_is_found_on_its_own_side_of_the_patch() {
        let file = file(Some(PATCH));
        assert_eq!(file.line(Side::New, 10), Some("one"));
        assert_eq!(file.line(Side::Old, 11), Some("two"), "only on the side it was on");
        assert_eq!(file.line(Side::New, 11), Some("TWO"));
        assert_eq!(file.line(Side::New, 12), Some("three"));
        assert_eq!(file.line(Side::New, 13), Some("four"));
        assert_eq!(file.line(Side::Old, 12), Some("four"), "the same line, where it was");
        assert_eq!(file.line(Side::New, 14), None);
        assert_eq!(file.line(Side::New, 0), None, "no line is number 0, whatever a side skipped");
    }

    /// A line first in its hunk has nothing before it to match first, and
    /// its other side's count still stands on the line above the hunk.
    #[test]
    fn a_line_only_on_one_side_is_never_found_on_the_other() {
        let added = file(Some("@@ -5,1 +5,2 @@\n+new\n five\n"));
        assert_eq!(added.line(Side::Old, 4), None, "line 4 isn't in this patch, and what was added wasn't there");
        assert_eq!(added.line(Side::Old, 5), Some("five"));
        assert_eq!(added.line(Side::New, 5), Some("new"));
        let removed = file(Some("@@ -5,2 +5,1 @@\n-gone\n five\n"));
        assert_eq!(removed.line(Side::New, 4), None, "line 4 isn't in this patch, and what was removed isn't there now");
        assert_eq!(removed.line(Side::New, 5), Some("five"));
        assert_eq!(removed.line(Side::Old, 5), Some("gone"));
    }

    #[test]
    fn a_patch_nobody_wrote_finds_nothing_and_breaks_nothing() {
        assert_eq!(file(None).line(Side::New, 1), None);
        assert_eq!(file(Some("@@ -x +y @@\n one\n")).line(Side::New, 1), None);
        assert_eq!(file(Some("@@ -1 +1 @@\n\n")).line(Side::New, 1), None);
    }

    #[test]
    fn a_hunk_starting_at_nothing_doesnt_underflow() {
        assert_eq!(file(Some("@@ -0,0 +1,2 @@\n+one\n+two\n")).line(Side::New, 2), Some("two"));
    }

    #[test]
    fn comments_go_as_one_message_saying_where_each_is() {
        let on =
            |side, first, last, quote: Option<&str>, text: &str| Comment { path: "a.rs".to_owned(), side, first, last, quote: quote.map(str::to_owned), text: text.to_owned() };
        assert_eq!(comments_said(&[]), None);
        let said = comments_said(&[
            on(Side::New, 3, 3, Some("let x = 1;"), " Put this in a helper. "),
            on(Side::Old, 12, 12, Some("old"), "Keep this."),
            on(Side::New, 4, 6, None, "Too long."),
        ]);
        assert_eq!(
            said.as_deref(),
            Some(
                "Comments on what you changed:\n\na.rs, line 3\n> let x = 1;\n\nPut this in a helper.\n\na.rs, removed line 12\n> old\n\nKeep this.\n\na.rs, lines 4 to 6\nToo long.\n"
            )
        );
        assert!(!said.as_deref().unwrap_or_default().starts_with('/'), "it's your words, never a command");
    }
}
