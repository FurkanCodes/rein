//! `@path` in a message: a file or folder the user points at, read along
//! with the message so the model starts with it instead of going to look.
//!
//! ```text
//! why does @src/parser.rs panic?        the whole file
//! look at @src/parser.rs:120-180        those lines (also #L120-L180)
//! what's in @src/ui/?                   the folder's files
//! ```
//!
//! A mention starts a word, so `me@example.com` isn't one; sentence
//! punctuation after it is left off. Whether it names something is for the
//! agent to find out: `@alice` is just a word unless there's a file called
//! that.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub path: String,
    /// The first line, counting from 1, and the last, if only some lines.
    pub lines: Option<(usize, Option<usize>)>,
}

impl Mention {
    /// It reads like a path, with a `/` or a `.` in it: one that doesn't
    /// exist is worth saying so about, where `@alice` is just a word.
    pub fn looks_like_a_path(&self) -> bool {
        self.path.contains(['/', '.'])
    }
}

/// The mentions in `text`, each once, in order.
pub fn find(text: &str) -> Vec<Mention> {
    let mut found: Vec<Mention> = Vec::new();
    for (at, _) in text.match_indices('@') {
        let starts_a_word = text[..at].chars().next_back().is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '"' | '\'' | '`'));
        if !starts_a_word {
            continue;
        }
        let rest = &text[at + 1..];
        let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ')' | ']' | ',' | ';' | '@')).unwrap_or(rest.len());
        let word = rest[..end].trim_end_matches(['.', ':', '!', '?']);
        let (path, lines) = split_lines(word);
        let path = path.trim_start_matches("./");
        if path.is_empty() {
            continue;
        }
        let mention = Mention { path: path.to_owned(), lines };
        if !found.contains(&mention) {
            found.push(mention);
        }
    }
    found
}

/// `src/a.rs:120-180` or `src/a.rs#L120-L180` → the path and the lines.
fn split_lines(word: &str) -> (&str, Option<(usize, Option<usize>)>) {
    let split = word.rsplit_once("#L").or_else(|| word.rsplit_once(':'));
    let Some((path, range)) = split else { return (word, None) };
    let (first, last) = match range.split_once('-') {
        Some((first, last)) => (first, Some(last.trim_start_matches('L'))),
        None => (range, None),
    };
    let Ok(first) = first.parse::<usize>() else { return (word, None) };
    let last = match last.map(str::parse::<usize>) {
        Some(Ok(last)) if last >= first => Some(last),
        Some(_) => return (word, None),
        None => None,
    };
    (path, Some((first.max(1), last)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(text: &str) -> Vec<String> {
        find(text).into_iter().map(|m| m.path).collect()
    }

    #[test]
    fn finds_what_a_message_points_at() {
        assert_eq!(paths("why does @src/parser.rs panic?"), ["src/parser.rs"]);
        assert_eq!(paths("compare @a.rs and @b.rs, then @a.rs again."), ["a.rs", "b.rs"], "each once; punctuation left off");
        assert_eq!(paths("(see @src/ui/) and \"@./README.md\""), ["src/ui/", "README.md"]);
        assert_eq!(paths("@Cargo.toml"), ["Cargo.toml"]);
        assert!(paths("mail me@example.com or @ alone").is_empty(), "an address isn't a mention, nor is a lone @");
    }

    #[test]
    fn reads_which_lines() {
        let one = |text| find(text).remove(0).lines;
        assert_eq!(one("@src/a.rs:120-180"), Some((120, Some(180))));
        assert_eq!(one("@src/a.rs#L120-L180"), Some((120, Some(180))));
        assert_eq!(one("@src/a.rs:42"), Some((42, None)));
        assert_eq!(one("@src/a.rs:"), None, "a colon at the end of a sentence");
        assert_eq!(find("@src/a.rs:9-3").remove(0).path, "src/a.rs:9-3", "a range backwards isn't one");
    }

    #[test]
    fn tells_a_path_from_a_word() {
        assert!(find("@src/a.rs").remove(0).looks_like_a_path());
        assert!(find("@Makefile/").remove(0).looks_like_a_path());
        assert!(!find("ask @alice").remove(0).looks_like_a_path());
    }
}
