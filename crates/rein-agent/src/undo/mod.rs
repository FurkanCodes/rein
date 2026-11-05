//! What rein changed in the project, turn by turn, so `/undo` can put it
//! back: what `write` and `edit` did, and what commands did to files.
//!
//! Before a command that can change files, rein looks at every file in the
//! project that the `.gitignore` files don't leave out, and keeps a copy of
//! each one that's new or changed since it last looked. After the command
//! it looks again, and what differs is what the command did. A file's size,
//! times and inode say whether it changed, so after the first look only
//! what changed is read. The copies are kept in rein's folder, readable only
//! by you, as clones where the file system makes them (APFS, Btrfs, XFS),
//! which take no room until the file changes.
//!
//! What's put back is files in the project, and only those: not what a
//! command did outside it, nor to anything but files (a database, a branch,
//! a package it installed). Ignored files, files over [`BIGGEST`] and
//! symlinks aren't kept. A file that changed since rein did is left as it
//! is, as that's someone else's work.

mod copies;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::search::Hides;
use copies::Copies;

/// A file bigger than this isn't kept.
pub const BIGGEST: u64 = 10 << 20;
/// With more files than this in the project, commands aren't followed.
pub const MOST_FILES: usize = 20_000;
/// Nor when the files to keep come to more than this.
pub const MOST_BYTES: u64 = 1 << 30;
/// How many turns a conversation keeps to undo.
const TURNS: usize = 30;

/// A file as rein last saw it, and the copy kept of it then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Seen {
    len: u64,
    /// When what's in it last changed, and when anything about it did, in
    /// seconds and nanoseconds: the second can't be set by a program.
    modified: (i64, i64),
    changed: (i64, i64),
    device: u64,
    inode: u64,
    mode: u32,
    /// Its copy's name, or none if it wasn't kept.
    copy: Option<String>,
}

impl Seen {
    /// The file `meta` is about, if it's a plain file, before it's kept.
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Option<Seen> {
        use std::os::unix::fs::MetadataExt;
        meta.is_file().then(|| Seen {
            len: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
            device: meta.dev(),
            inode: meta.ino(),
            mode: meta.mode(),
            copy: None,
        })
    }

    /// Nothing is followed where a file's inode and times can't be read.
    #[cfg(not(unix))]
    fn of(_: &std::fs::Metadata) -> Option<Seen> {
        None
    }

    /// The same file as it was, whatever was kept of it. A new mode is a
    /// new change time too.
    fn same(&self, other: &Seen) -> bool {
        (self.len, self.modified, self.changed, self.device, self.inode) == (other.len, other.modified, other.changed, other.device, other.inode)
    }

    /// What a copy of it as it is now is called: found again by that, by
    /// any conversation, as long as the file stays the same.
    fn name(&self) -> String {
        let (modified, changed) = (self.modified, self.changed);
        format!("{:x}-{:x}-{:x}-{:x}.{:x}-{:x}.{:x}", self.device, self.inode, self.len, modified.0, modified.1, changed.0, changed.1)
    }
}

/// What a turn did to one file: as it was before the turn, and as the turn
/// left it; `None` where it wasn't there.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Change {
    before: Option<Seen>,
    after: Option<Seen>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Turn {
    /// What started it, as said: the start of your message.
    said: String,
    /// When, in seconds since 1970.
    at: u64,
    changes: BTreeMap<String, Change>,
}

impl Turn {
    fn new(said: &str) -> Turn {
        let said = said.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
        let said = match said.char_indices().nth(60) {
            Some((at, _)) => format!("{}…", &said[..at]),
            None => said.to_owned(),
        };
        let at = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs());
        Turn { said, at, changes: BTreeMap::new() }
    }

    fn copies(&self) -> impl Iterator<Item = &str> {
        self.changes.values().flat_map(|change| [&change.before, &change.after]).flatten().filter_map(|seen| seen.copy.as_deref())
    }
}

/// What a step did to the project's files.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changed {
    pub changed: Vec<String>,
    pub made: Vec<String>,
    pub removed: Vec<String>,
    /// Of those changed or removed, the ones rein had no copy of: too big,
    /// or changing as it looked. `/undo` can't put them back.
    pub unkept: Vec<String>,
}

impl Changed {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.made.is_empty() && self.removed.is_empty()
    }
}

/// What a command did, as it's shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watched {
    Changed(Changed),
    /// What commands do isn't followed here, and this is why: said once.
    NotHere(Off),
}

/// Why what commands do isn't followed in this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// It has this many files, more than [`MOST_FILES`].
    Files(usize),
    /// Its files come to this many bytes, more than [`MOST_BYTES`].
    Bytes(u64),
}

impl std::fmt::Display for Off {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Off::Files(files) => write!(f, "the project has {files} files, more than the {MOST_FILES} rein keeps copies of"),
            Off::Bytes(bytes) => write!(f, "its files come to {} MB, more than the {} MB rein keeps copies of", bytes >> 20, MOST_BYTES >> 20),
        }
    }
}

/// A turn there is to undo, as `/undo` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Which it is, for [`Undo::plan`] and [`Undo::undo`].
    pub turn: usize,
    pub said: String,
    /// When it started, in seconds since 1970.
    pub at: u64,
    pub changed: usize,
    pub made: usize,
    pub removed: usize,
}

/// What undoing back to before a turn does, or did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Changed or removed since: back as they were.
    pub put_back: Vec<String>,
    /// Made since: taken away again.
    pub take_away: Vec<String>,
    /// Changed since rein last did, by you or another program: left as
    /// they are.
    pub left: Vec<String>,
    /// Never kept, being too big: they can't be put back.
    pub lost: Vec<String>,
    /// What couldn't be done, and why.
    pub failed: Vec<(String, String)>,
    /// What the first turn undone started with.
    pub said: String,
    /// How many turns it undoes.
    pub turns: usize,
}

impl Plan {
    /// Whether it does anything to a file.
    pub fn does_anything(&self) -> bool {
        !self.put_back.is_empty() || !self.take_away.is_empty()
    }
}

pub(crate) struct Undo {
    root: PathBuf,
    /// rein's folder for this project, if copies are kept on disk: each
    /// conversation's are in it, under its ID.
    folder: Option<PathBuf>,
    copies: Copies,
    /// Whether the turns kept on disk for this conversation have been read,
    /// and other conversations' old copies swept away.
    loaded: bool,
    /// Every file rein knows of, as it last saw it, with its copy.
    seen: BTreeMap<String, Seen>,
    /// Whether `seen` is the whole project, from a look at all of it.
    whole: bool,
    /// A command was looked before, and not after: it was stopped. What it
    /// did is found the next time rein looks.
    pending: bool,
    turns: Vec<Turn>,
    /// Why commands aren't followed here, once that's found.
    off: Option<Off>,
    /// The most files, and bytes, followed: [`MOST_FILES`] and [`MOST_BYTES`].
    most: (usize, u64),
}

impl Undo {
    /// For the project at `root`, copies kept in memory: only rein's own
    /// file changes are followed, until [`Undo::keep_in`] gives a folder.
    pub(crate) fn new(root: PathBuf) -> Undo {
        let copies = Copies::Memory(Default::default());
        Undo { root, folder: None, copies, loaded: true, seen: BTreeMap::new(), whole: false, pending: false, turns: Vec::new(), off: None, most: (MOST_FILES, MOST_BYTES) }
    }

    /// Keeps copies in `folder`, rein's folder for this project, from the
    /// next conversation [`Undo::open`]s.
    pub(crate) fn keep_in(&mut self, folder: PathBuf) {
        self.folder = Some(folder);
    }

    /// Follows the conversation `session`, with the turns it kept, if it's
    /// one continued: what's from the last one goes. Nothing is read until
    /// it's needed.
    pub(crate) fn open(&mut self, session: &str) {
        self.copies = match &self.folder {
            Some(folder) => Copies::Disk { own: folder.join(plain(session)) },
            None => Copies::Memory(Default::default()),
        };
        self.loaded = !self.copies.on_disk();
        (self.seen, self.whole, self.pending, self.turns, self.off) = (BTreeMap::new(), false, false, Vec::new(), None);
    }

    fn load(&mut self) {
        if !self.loaded {
            self.loaded = true;
            self.copies.sweep();
            self.turns = self.copies.turns();
        }
    }

    /// The project, resolved, as paths in it are.
    fn root(&self) -> PathBuf {
        self.root.canonicalize().unwrap_or_else(|_| self.root.clone())
    }

    /// A new turn, started by `said`. What a stopped command did is the
    /// last turn's; a turn that changed nothing isn't kept.
    pub(crate) fn turn(&mut self, said: &str, hides: Hides) {
        self.load();
        self.settle(hides);
        if self.turns.last().is_some_and(|turn| turn.changes.is_empty()) {
            self.turns.pop();
        }
        self.turns.push(Turn::new(said));
        // As many as there are kept, and the one just started.
        let extra = self.turns.len().saturating_sub(TURNS + 1);
        self.turns.drain(..extra);
        let wanted: BTreeSet<&str> = self.seen.values().filter_map(|seen| seen.copy.as_deref()).chain(self.turns.iter().flat_map(Turn::copies)).collect();
        self.copies.tidy(&wanted);
    }

    /// What was done since a look before that never had its look after.
    pub(crate) fn settle(&mut self, hides: Hides) {
        if self.pending {
            self.after_all(hides);
        }
    }

    /// A command in the background may still change files: the next look
    /// keeps what it changed since this one in the turn, as rein's doing.
    pub(crate) fn still_changing(&mut self) {
        self.pending = self.whole;
    }

    /// Before a command that may change files: the whole project as it is
    /// now, with a copy of what's new or changed since rein last looked.
    /// What changed in between wasn't rein's doing, so it isn't the turn's.
    /// Why commands aren't followed here, the first time that's found.
    pub(crate) fn before_all(&mut self, hides: Hides) -> Option<Off> {
        self.load();
        self.settle(hides);
        if !self.copies.on_disk() || self.off.is_some() {
            return None;
        }
        match self.look(hides) {
            Ok(found) => {
                self.seen = self.refresh(found);
                (self.whole, self.pending) = (true, true);
                None
            }
            Err(off) => {
                (self.off, self.whole) = (Some(off), false);
                Some(off)
            }
        }
    }

    /// After a command: what it did, kept in the turn.
    pub(crate) fn after_all(&mut self, hides: Hides) -> Changed {
        self.pending = false;
        if !self.whole {
            return Changed::default();
        }
        let found = match self.look(hides) {
            Ok(found) => found,
            Err(off) => {
                (self.off, self.whole) = (Some(off), false);
                return Changed::default();
            }
        };
        let fresh = self.refresh(found);
        let old = std::mem::replace(&mut self.seen, fresh);
        let paths: BTreeSet<String> = old.keys().chain(self.seen.keys()).cloned().collect();
        let mut changed = Changed::default();
        for path in paths {
            let (before, after) = (old.get(&path).cloned(), self.seen.get(&path).cloned());
            self.note(&path, before, after, &mut changed);
        }
        self.copies.save(&self.turns);
        changed
    }

    /// Before `write` or `edit` changes `rel`: the file as it is now, kept.
    pub(crate) fn before_one(&mut self, rel: &str) {
        self.load();
        match self.now(rel) {
            Some(seen) => {
                let seen = self.kept(rel, seen);
                self.seen.insert(rel.to_owned(), seen);
            }
            None => {
                self.seen.remove(rel);
            }
        }
    }

    /// After `write` or `edit` changed `rel`: what it did, kept in the turn.
    pub(crate) fn after_one(&mut self, rel: &str) {
        let before = self.seen.get(rel).cloned();
        let after = self.now(rel).map(|seen| self.kept(rel, seen));
        match &after {
            Some(seen) => self.seen.insert(rel.to_owned(), seen.clone()),
            None => self.seen.remove(rel),
        };
        if self.note(rel, before, after, &mut Changed::default()) {
            self.copies.save(&self.turns);
        }
    }

    /// Keeps in the turn that `path` went from `before` to `after`, and
    /// says so in `changed`; false if it didn't change.
    fn note(&mut self, path: &str, before: Option<Seen>, after: Option<Seen>, changed: &mut Changed) -> bool {
        let unkept = before.as_ref().is_some_and(|seen| seen.copy.is_none());
        match (&before, &after) {
            (Some(before), Some(after)) if before.same(after) => return false,
            (None, None) => return false,
            (Some(_), Some(_)) => changed.changed.push(path.to_owned()),
            (None, Some(_)) => changed.made.push(path.to_owned()),
            (Some(_), None) => changed.removed.push(path.to_owned()),
        }
        if unkept {
            changed.unkept.push(path.to_owned());
        }
        if self.turns.is_empty() {
            self.turns.push(Turn::new(""));
        }
        let turn = self.turns.last_mut().expect("a turn was just made if there was none");
        let change = turn.changes.entry(path.to_owned()).or_insert(Change { before, after: None });
        change.after = after;
        // Made and gone again within the turn: nothing to undo.
        if change.before.is_none() && change.after.is_none() {
            turn.changes.remove(path);
        }
        true
    }

    /// `rel` as it is now, if it's a plain file.
    fn now(&self, rel: &str) -> Option<Seen> {
        std::fs::symlink_metadata(self.root().join(rel)).ok().and_then(|meta| Seen::of(&meta))
    }

    /// `seen`, the file `rel` as it is now, with its copy, as [`Undo::refresh`].
    fn kept(&mut self, rel: &str, seen: Seen) -> Seen {
        let mut kept = self.refresh([(rel.to_owned(), seen)].into());
        kept.remove(rel).expect("refresh keeps every file it's given")
    }

    /// Every file in `found` with its copy: the one kept before if it hasn't
    /// changed, or a new one.
    fn refresh(&mut self, found: BTreeMap<String, Seen>) -> BTreeMap<String, Seen> {
        let mut ready = BTreeMap::new();
        let mut wanted = Vec::new();
        for (rel, mut seen) in found {
            match self.seen.get(&rel) {
                Some(old) if old.same(&seen) && (old.copy.is_some() || seen.len > BIGGEST) => seen.copy.clone_from(&old.copy),
                _ if seen.len > BIGGEST => {}
                _ => {
                    wanted.push((rel, seen));
                    continue;
                }
            }
            ready.insert(rel, seen);
        }
        let root = self.root();
        ready.extend(self.copies.keep(&root, wanted));
        ready
    }

    /// Every plain file in the project the `.gitignore` files don't leave
    /// out, as it is now, unless there are too many to keep.
    fn look(&self, hides: Hides) -> Result<BTreeMap<String, Seen>, Off> {
        let root = self.root();
        let files = crate::search::files(&root, hides);
        if files.len() > self.most.0 {
            return Err(Off::Files(files.len()));
        }
        let mut found = BTreeMap::new();
        let mut bytes: u64 = 0;
        for rel in files {
            let Some(seen) = std::fs::symlink_metadata(root.join(&rel)).ok().and_then(|meta| Seen::of(&meta)) else { continue };
            if seen.len <= BIGGEST {
                bytes = bytes.saturating_add(seen.len);
            }
            found.insert(rel, seen);
        }
        if bytes > self.most.1 {
            return Err(Off::Bytes(bytes));
        }
        Ok(found)
    }

    /// The turns there are to undo, the latest first.
    pub(crate) fn listed(&mut self, hides: Hides) -> Vec<Listed> {
        self.load();
        self.settle(hides);
        let count = |turn: &Turn, which: fn(&Change) -> bool| turn.changes.values().filter(|change| which(change)).count();
        let listed = self.turns.iter().enumerate().filter(|(_, turn)| !turn.changes.is_empty());
        let mut listed: Vec<Listed> = listed
            .map(|(at, turn)| Listed {
                turn: at,
                said: turn.said.clone(),
                at: turn.at,
                changed: count(turn, |change| change.before.is_some() && change.after.is_some()),
                made: count(turn, |change| change.before.is_none()),
                removed: count(turn, |change| change.after.is_none()),
            })
            .collect();
        listed.reverse();
        listed
    }

    /// What undoing back to before turn `from` would do now.
    pub(crate) fn plan(&mut self, from: usize, hides: Hides) -> Plan {
        self.load();
        self.settle(hides);
        let undone = self.turns.get(from..).unwrap_or_default();
        let mut plan = Plan { said: undone.first().map(|turn| turn.said.clone()).unwrap_or_default(), turns: undone.len(), ..Plan::default() };
        // Each file as it was before the first of them, and as the last left it.
        let mut changes: BTreeMap<&str, (Option<&Seen>, Option<&Seen>)> = BTreeMap::new();
        for turn in undone {
            for (path, change) in &turn.changes {
                changes.entry(path.as_str()).or_insert((change.before.as_ref(), None)).1 = change.after.as_ref();
            }
        }
        for (path, (before, after)) in changes {
            let now = self.now(path);
            let as_left = match (&now, after) {
                (None, None) => true,
                (Some(now), Some(after)) => now.same(after),
                _ => false,
            };
            match before {
                _ if !as_left => plan.left.push(path.to_owned()),
                None if now.is_some() => plan.take_away.push(path.to_owned()),
                None => {}
                Some(before) if before.copy.is_none() => plan.lost.push(path.to_owned()),
                Some(_) => plan.put_back.push(path.to_owned()),
            }
        }
        plan
    }

    /// Undoes turn `from` and every one after it, as [`Undo::plan`] says
    /// now, and forgets them: what it did.
    pub(crate) fn undo(&mut self, from: usize, hides: Hides) -> Plan {
        let mut plan = self.plan(from, hides);
        let root = self.root();
        let befores: BTreeMap<String, Seen> = self
            .turns
            .get(from..)
            .unwrap_or_default()
            .iter()
            .rev()
            .flat_map(|turn| &turn.changes)
            .filter_map(|(path, change)| Some((path.clone(), change.before.clone()?)))
            .collect();
        let mut done = Plan { said: plan.said.clone(), turns: plan.turns, left: std::mem::take(&mut plan.left), lost: std::mem::take(&mut plan.lost), ..Plan::default() };
        for path in plan.put_back {
            let put = befores.get(&path).ok_or_else(|| "nothing of it was kept".to_owned()).and_then(|before| self.put_back(&root, &path, before));
            match put {
                Ok(()) => done.put_back.push(path),
                Err(why) => done.failed.push((path, why)),
            }
        }
        for path in plan.take_away {
            match crate::files::remove(&root, Path::new(&path)) {
                Ok(()) => done.take_away.push(path),
                Err(e) => done.failed.push((path, e.to_string())),
            }
        }
        self.turns.truncate(from);
        // What's there now is the start of what comes next.
        for path in done.put_back.iter().chain(&done.take_away) {
            self.before_one(path);
        }
        // A file put back is as an earlier turn left it, written again: that
        // turn's to undo as it would have been.
        for path in &done.put_back {
            let (Some(before), Some(now)) = (befores.get(path), self.seen.get(path).cloned()) else { continue };
            let earlier = self.turns.iter_mut().rev().find_map(|turn| turn.changes.get_mut(path));
            if let Some(change) = earlier.filter(|change| change.after.as_ref().is_some_and(|after| after.same(before))) {
                change.after = Some(now);
            }
        }
        self.copies.save(&self.turns);
        done
    }

    /// Writes the copy of `before` at `path`, with its mode, never through
    /// a symlink swapped in since.
    fn put_back(&self, root: &Path, path: &str, before: &Seen) -> Result<(), String> {
        let name = before.copy.as_deref().ok_or("it was never kept")?;
        let bytes = self.copies.bytes(name).map_err(|e| format!("its copy can't be read: {e}"))?;
        let mut file = crate::files::create(root, Path::new(path)).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut file, &bytes).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(before.mode & 0o7777)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// `session` as a folder's name: only what can't lead anywhere else.
fn plain(session: &str) -> String {
    let name: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect();
    format!("conversation-{name}")
}
