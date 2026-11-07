//! What a turn or a whole conversation changed: each file as it was before,
//! against how it is now, which is what undoing it would see. So what you
//! review is what Undo puts back, and the two can't disagree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use similar::{Algorithm, ChangeTag, TextDiff};

use super::Seen;
use super::copies::{self, Kept};

/// A file longer than this shows no lines: comparing it takes a while, and
/// nobody reads the result.
pub(super) const LONGEST: u64 = 1 << 20;
/// How long two files are compared for. Past this the lines are still right,
/// but fewer of them are found to be the same.
const COMPARING: Duration = Duration::from_secs(1);
/// The most patch a client is sent in one answer.
const MOST_PATCH: usize = 8 << 20;
/// How many unchanged lines show around a change.
const AROUND: usize = 3;

/// How far back to look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// The last turn that changed anything.
    LastTurn,
    Conversation,
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
    /// It isn't text.
    Binary,
    TooBig,
    /// No copy of how it was is left, or it changed as it was read.
    NotKept,
}

/// What's happened to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub became: Became,
    pub added: usize,
    pub removed: usize,
    pub unshown: Option<Unshown>,
    /// Changed since rein last did, by you or another program: undoing
    /// leaves it as it is.
    pub since: bool,
    /// Its lines as a unified patch, empty where they don't show.
    pub patch: String,
}

/// What a span changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// The last turn that changed anything, to undo.
    pub turn: Option<usize>,
    pub files: Vec<FileChange>,
}

impl Changes {
    /// How many files changed, and the lines added and removed in them.
    pub fn totals(&self) -> (usize, usize, usize) {
        let lines = |count: fn(&FileChange) -> usize| self.files.iter().map(count).sum();
        (self.files.len(), lines(|file| file.added), lines(|file| file.removed))
    }
}

/// How a file was, as far as rein can say without reading it.
pub(super) enum Was {
    Gone,
    TooBig,
    NotKept,
    Kept(Kept),
}

/// A file to compare.
pub(super) struct ToCompare {
    pub(super) path: String,
    pub(super) was: Was,
    pub(super) now: Option<Seen>,
    /// Changed since rein last did.
    pub(super) since: bool,
}

/// What's left to do to say what a span changed. It holds nothing of
/// [`super::Undo`], so comparing takes no lock: a turn that's running goes on,
/// and only one file is in memory at a time.
#[derive(Default)]
pub(crate) struct Comparing {
    pub(super) root: PathBuf,
    pub(super) turn: Option<usize>,
    pub(super) files: Vec<ToCompare>,
}

impl Comparing {
    /// Reads and compares each file, with its patch if `lines`: how much
    /// changed takes comparing them, and nothing more.
    pub(crate) fn run(self, lines: bool) -> Changes {
        let Comparing { root, turn, files } = self;
        let sides = files.into_iter().map(|file| {
            let is = file.now.as_ref().map_or(Side::Gone, |now| now_side(&root, &file.path, now));
            (file.path, was_side(file.was), is, file.since)
        });
        Changes { turn, files: within(sides, lines, MOST_PATCH) }
    }
}

/// `rel` as it is now, read never through a link swapped in for it.
fn now_side(root: &Path, rel: &str, now: &Seen) -> Side {
    if now.len > LONGEST {
        return Side::TooBig;
    }
    // Changed since it was looked at, it's still how it is now.
    copies::read(root, rel, now).or_else(|_| copies::read_now(root, rel, LONGEST)).map_or(Side::NotKept, Side::Bytes)
}

fn was_side(was: Was) -> Side {
    match was {
        Was::Gone => Side::Gone,
        Was::TooBig => Side::TooBig,
        Was::NotKept => Side::NotKept,
        Was::Kept(kept) => kept.bytes().map_or(Side::NotKept, Side::Bytes),
    }
}

/// One side of a comparison.
enum Side {
    /// There's no such file.
    Gone,
    Bytes(Vec<u8>),
    TooBig,
    NotKept,
}

/// Compares what `sides` holds, one at a time as it comes: each path, how it
/// was, how it is, and whether it's changed since rein did. Files that came
/// back as they were drop out, and past `room` for patches the rest show no
/// lines.
fn within(sides: impl IntoIterator<Item = (String, Side, Side, bool)>, lines: bool, mut room: usize) -> Vec<FileChange> {
    let mut files = Vec::new();
    for (path, was, is, since) in sides {
        let Some(mut file) = compare(path, was, is, lines) else { continue };
        file.since = since;
        match room.checked_sub(file.patch.len()) {
            Some(left) => room = left,
            None => {
                file.patch.clear();
                file.unshown = Some(Unshown::TooBig);
            }
        }
        files.push(file);
    }
    files
}

/// `path` as it was against how it is, if it changed.
fn compare(path: String, was: Side, is: Side, lines: bool) -> Option<FileChange> {
    let became = match (&was, &is) {
        (Side::Gone, Side::Gone) => return None,
        (Side::Gone, _) => Became::Made,
        (_, Side::Gone) => Became::Removed,
        _ => Became::Changed,
    };
    let unshown = |why| Some(FileChange { path: path.clone(), became, added: 0, removed: 0, unshown: Some(why), since: false, patch: String::new() });
    let (old, new) = match (was, is) {
        (Side::TooBig, _) | (_, Side::TooBig) => return unshown(Unshown::TooBig),
        (Side::NotKept, _) | (_, Side::NotKept) => return unshown(Unshown::NotKept),
        (Side::Bytes(old), Side::Bytes(new)) if old == new => return None,
        (Side::Bytes(old), Side::Bytes(new)) => (old, new),
        (Side::Bytes(old), Side::Gone) => (old, Vec::new()),
        (Side::Gone, Side::Bytes(new)) => (Vec::new(), new),
        (Side::Gone, Side::Gone) => return None,
    };
    let (Some(old), Some(new)) = (text(&old), text(&new)) else { return unshown(Unshown::Binary) };
    let diff = TextDiff::configure().algorithm(Algorithm::Patience).timeout(COMPARING).diff_lines(old, new);
    let count = |tag: ChangeTag| diff.iter_all_changes().filter(|change| change.tag() == tag).count();
    let (added, removed) = (count(ChangeTag::Insert), count(ChangeTag::Delete));
    let patch = if lines { format!("{}{}", header(&path, became), diff.unified_diff().context_radius(AROUND)) } else { String::new() };
    Some(FileChange { path, became, added, removed, unshown: None, since: false, patch })
}

/// `bytes`, if they're text.
fn text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok().filter(|text| !text.contains('\0'))
}

/// How a patch of `path` starts, as Git writes it.
fn header(path: &str, became: Became) -> String {
    let (mode, old, new) = match became {
        Became::Changed => (String::new(), format!("a/{path}"), format!("b/{path}")),
        Became::Made => ("new file mode 100644\n".to_owned(), "/dev/null".to_owned(), format!("b/{path}")),
        Became::Removed => ("deleted file mode 100644\n".to_owned(), format!("a/{path}"), "/dev/null".to_owned()),
    };
    format!("diff --git a/{path} b/{path}\n{mode}--- {old}\n+++ {new}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(text: &str) -> Side {
        Side::Bytes(text.as_bytes().to_vec())
    }

    #[test]
    fn past_the_room_for_patches_the_rest_say_so_and_still_count() {
        let sides = |name: &str| (name.to_owned(), side("one\n"), side("two\n"), false);
        let room = compare("a".to_owned(), side("one\n"), side("two\n"), true).expect("it changed").patch.len();
        let files = within(vec![sides("a"), sides("b"), sides("c")], true, room + 1);
        assert!(!files[0].patch.is_empty() && files[0].unshown.is_none());
        assert!(files[1].patch.is_empty() && files[1].unshown == Some(Unshown::TooBig), "{:?}", files[1]);
        assert_eq!((files[1].added, files[1].removed), (1, 1), "how much changed is still known");
    }

    #[test]
    fn valid_text_with_a_nul_in_it_isnt_text() {
        let compared = compare("a".to_owned(), side("one\n"), side("t\0wo\n"), true).expect("it changed");
        assert_eq!(compared.unshown, Some(Unshown::Binary));
    }

    #[test]
    fn a_file_grown_too_long_shows_no_lines_whichever_side_is_long() {
        assert_eq!(compare("a".to_owned(), side("one\n"), Side::TooBig, true).and_then(|file| file.unshown), Some(Unshown::TooBig));
        assert_eq!(compare("a".to_owned(), Side::TooBig, side("one\n"), true).and_then(|file| file.unshown), Some(Unshown::TooBig));
    }

    #[test]
    fn a_made_file_with_nothing_in_it_is_still_made() {
        let made = compare("a".to_owned(), Side::Gone, side(""), true).expect("it's there now");
        assert_eq!((made.became, made.added), (Became::Made, 0));
        assert!(made.patch.contains("new file mode"), "{}", made.patch);
    }

    #[test]
    fn how_much_changed_is_known_without_a_patch() {
        let counted = compare("a".to_owned(), side("one\n"), side("two\nthree\n"), false).expect("it changed");
        assert_eq!((counted.added, counted.removed, counted.patch.as_str()), (2, 1, ""));
    }
}
