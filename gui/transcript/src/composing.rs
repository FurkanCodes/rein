//! What's worked out as you write, the terminal's rules for its box: the
//! word the caret is at, what ↑ brings back, what goes with a message.

/// The word starting with `mark` that the caret is at the end of: where it
/// starts, and what's typed after the mark. `caret` is in bytes.
pub fn word_at(text: &str, caret: usize, mark: char) -> Option<(usize, &str)> {
    let before = text.get(..caret)?;
    if text[caret..].chars().next().is_some_and(|after| !after.is_whitespace()) {
        return None;
    }
    let start = before.rfind(char::is_whitespace).map_or(0, |at| at + before[at..].chars().next().map_or(1, char::len_utf8));
    before[start..].strip_prefix(mark).map(|typed| (start, typed))
}

/// What's put in place of an `@` word: with a space after a file, to go on
/// with the message, and none after a folder, to go on into it.
pub fn mention(path: &str) -> String {
    if path.ends_with('/') { format!("@{path}") } else { format!("@{path} ") }
}

/// What you sent before, walked with ↑ and ↓.
#[derive(Debug, Default)]
pub struct Recall {
    /// Oldest first, each message once.
    sent: Vec<String>,
    /// Which is in the box, and what it took the place of.
    at: Option<(usize, String)>,
}

impl Recall {
    /// What rein has kept of this folder. What was sent while it was on its
    /// way stays the newest.
    pub fn load(&mut self, kept: Vec<String>) {
        let newer = std::mem::replace(&mut self.sent, kept);
        for text in newer {
            self.keep(&text);
        }
    }

    /// You sent `text`.
    pub fn keep(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.sent.retain(|old| old != text);
        self.sent.push(text.to_owned());
        self.at = None;
    }

    /// The message before the one in the box, in place of `draft`.
    pub fn older(&mut self, draft: &str) -> Option<String> {
        let (next, draft) = match self.at.take() {
            Some((0, draft)) => {
                self.at = Some((0, draft));
                return None;
            }
            Some((at, draft)) => (at - 1, draft),
            None => (self.sent.len().checked_sub(1)?, draft.to_owned()),
        };
        self.at = Some((next, draft));
        self.sent.get(next).cloned()
    }

    /// The message after the one in the box, or what it took the place of.
    pub fn newer(&mut self) -> Option<String> {
        let (at, draft) = self.at.take()?;
        if at + 1 < self.sent.len() {
            self.at = Some((at + 1, draft));
            return self.sent.get(at + 1).cloned();
        }
        Some(draft)
    }

    /// `value` is in the box now: if it's not what ↑ put there, you've
    /// changed it, and it's yours.
    pub fn box_is(&mut self, value: &str) {
        if self.at.as_ref().is_some_and(|(at, _)| self.sent.get(*at).is_none_or(|shown| shown != value)) {
            self.at = None;
        }
    }

    /// Newest first, for finding one by its words.
    pub fn newest_first(&self) -> impl Iterator<Item = &str> {
        self.sent.iter().rev().map(String::as_str)
    }
}

/// `label`s still in `message`, in order: what goes with it. A label taken
/// out of the box takes its image with it, as in the terminal.
pub fn still_in<'a>(message: &str, labels: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    labels.filter(|label| message.contains(label)).collect()
}

/// What's shown of a line waiting for the end of a turn: a command's may
/// hold a key, and only rein can tell, so only its name.
pub fn waiting_shown(line: &str) -> &str {
    let first = line.lines().next().unwrap_or_default();
    if line.trim_start().starts_with('/') { first.split_whitespace().next().unwrap_or_default() } else { first }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_the_one_the_caret_is_at_the_end_of() {
        assert_eq!(word_at("@src", 4, '@'), Some((0, "src")));
        assert_eq!(word_at("read @src/ma please", 12, '@'), Some((5, "src/ma")));
        assert_eq!(word_at("read @src/ma please", 8, '@'), None, "in the middle of it isn't");
        assert_eq!(word_at("read me", 4, '@'), None);
        assert_eq!(word_at("a mail@home.com", 15, '@'), None, "an @ inside a word isn't one");
        assert_eq!(word_at("x\n@a", 4, '@'), Some((2, "a")), "a new line is space too");
        assert_eq!(word_at("é @a", 5, '@'), Some((3, "a")));
        assert_eq!(word_at("@", 1, '@'), Some((0, "")));
    }

    #[test]
    fn a_caret_that_isnt_on_a_character_finds_nothing() {
        assert_eq!(word_at("é", 1, '@'), None);
        assert_eq!(word_at("a", 9, '@'), None);
    }

    #[test]
    fn a_file_is_followed_by_a_space_and_a_folder_isnt() {
        assert_eq!(mention("src/main.rs"), "@src/main.rs ");
        assert_eq!(mention("src/"), "@src/");
    }

    fn recall() -> Recall {
        let mut recall = Recall::default();
        recall.load(vec!["first".to_owned(), "second".to_owned()]);
        recall
    }

    #[test]
    fn up_walks_back_and_down_comes_home_to_what_you_were_writing() {
        let mut recall = recall();
        assert_eq!(recall.older("draft").as_deref(), Some("second"));
        assert_eq!(recall.older("second").as_deref(), Some("first"));
        assert_eq!(recall.older("first"), None, "it stops at the oldest");
        assert_eq!(recall.newer().as_deref(), Some("second"));
        assert_eq!(recall.newer().as_deref(), Some("draft"));
        assert_eq!(recall.newer(), None, "and that's as far as it goes");
    }

    #[test]
    fn changing_what_was_brought_back_makes_it_yours() {
        let mut recall = recall();
        recall.older("draft");
        recall.box_is("second");
        assert_eq!(recall.older("second").as_deref(), Some("first"), "as it was put there");
        recall.box_is("first, edited");
        assert_eq!(recall.newer(), None);
        assert_eq!(recall.older("first, edited").as_deref(), Some("second"), "from the newest again");
    }

    #[test]
    fn a_message_is_kept_once_as_the_newest() {
        let mut recall = recall();
        recall.keep("first");
        assert_eq!(recall.newest_first().collect::<Vec<_>>(), ["first", "second"]);
        recall.keep("  ");
        assert_eq!(recall.newest_first().count(), 2);
    }

    #[test]
    fn what_was_sent_before_the_list_came_stays_the_newest() {
        let mut recall = Recall::default();
        recall.keep("just now");
        recall.load(vec!["older".to_owned(), "just now".to_owned()]);
        assert_eq!(recall.newest_first().collect::<Vec<_>>(), ["just now", "older"]);
    }

    #[test]
    fn with_nothing_sent_up_does_nothing() {
        assert_eq!(Recall::default().older("draft"), None);
        assert_eq!(Recall::default().newer(), None);
    }

    #[test]
    fn only_the_images_still_in_the_message_go_with_it() {
        let labels = ["[Image #1]", "[Image #2]"];
        assert_eq!(still_in("look at [Image #2] please", labels.into_iter()), ["[Image #2]"]);
    }

    #[test]
    fn a_waiting_command_shows_only_its_name() {
        assert_eq!(waiting_shown("/login groq a-secret-key"), "/login");
        assert_eq!(waiting_shown("words\nand more"), "words");
    }
}
