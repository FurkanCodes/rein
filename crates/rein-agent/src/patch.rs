//! The patch format GPT models are trained to change files with, as Codex
//! gives it to them (`apply_patch`): files added, deleted and updated in one
//! call, each change found by the lines around it rather than by line
//! numbers. Given rein's `edit` (exact text, replaced), they changed files
//! in a shape their own harness never asks of them.
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/app.py
//! @@ def greet():
//! -    print("hi")
//! +    print("hello")
//! *** Add File: notes.txt
//! +first line
//! *** Delete File: old.txt
//! *** End Patch
//! ```
//!
//! Lines are found as Codex finds them: exactly, then without what ends
//! them, then trimmed, then with typographic dashes and quotes as plain
//! ones. This only reads and makes text; [`crate::tools`] writes it.

/// Codex's grammar for the patch (Lark), which a provider that takes it
/// holds the model to: what doesn't fit it can't be sent.
pub const GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

/// What the model is told of the tool, as Codex tells it.
pub const ABOUT: &str = "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON. \
                         A patch starts with `*** Begin Patch` and ends with `*** End Patch`; between them, each file is \
                         `*** Add File: <path>` (every line after it starts with +), `*** Delete File: <path>`, or \
                         `*** Update File: <path>` (optionally `*** Move to: <path>`), then its changes: `@@` with a line \
                         to find them by, then lines starting with a space (kept), - (removed) or + (added). Paths are \
                         relative to the project folder.";

/// A patch, read: what it does to each file, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub files: Vec<FileChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    /// A file made with these lines (or remade, if it's there).
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    /// A file changed where each chunk says, and moved to `to` if it names
    /// a place.
    Update {
        path: String,
        to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

/// One change in a file: the lines it replaces, found after the lines that
/// introduce it (`@@ …`), and what goes in their place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chunk {
    /// Lines to find first, in turn, like `class Foo` and then `def bar`.
    pub after: Vec<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
    /// The lines end the file (`*** End of File`).
    pub at_end: bool,
}

impl FileChange {
    /// The file it's about.
    pub fn path(&self) -> &str {
        match self {
            FileChange::Add { path, .. } | FileChange::Delete { path } | FileChange::Update { path, .. } => path,
        }
    }

    /// Every file it changes: a move changes two.
    pub fn paths(&self) -> Vec<&str> {
        match self {
            FileChange::Update { path, to: Some(to), .. } => vec![path, to],
            _ => vec![self.path()],
        }
    }

    /// The lines it adds and removes.
    pub fn sizes(&self) -> (usize, usize) {
        match self {
            FileChange::Add { lines, .. } => (lines.len(), 0),
            FileChange::Delete { .. } => (0, 0),
            FileChange::Update { chunks, .. } => chunks.iter().fold((0, 0), |(added, removed), chunk| {
                // What's kept is in both: only what differs counts.
                let kept = chunk.old.iter().filter(|line| chunk.new.contains(line)).count();
                (added + chunk.new.len().saturating_sub(kept), removed + chunk.old.len().saturating_sub(kept))
            }),
        }
    }

    /// The text it adds, in every chunk.
    pub fn added(&self) -> String {
        match self {
            FileChange::Add { lines, .. } => lines.join("\n"),
            FileChange::Delete { .. } => String::new(),
            FileChange::Update { chunks, .. } => chunks.iter().flat_map(|chunk| &chunk.new).cloned().collect::<Vec<_>>().join("\n"),
        }
    }
}

impl Patch {
    /// Every file it changes, each once, in order.
    pub fn paths(&self) -> Vec<&str> {
        let mut paths: Vec<&str> = Vec::new();
        for path in self.files.iter().flat_map(FileChange::paths) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }
}

/// Reads `input`, a whole patch.
pub fn parse(input: &str) -> Result<Patch, String> {
    let mut lines = input.trim().lines().map(|line| line.strip_suffix('\r').unwrap_or(line)).enumerate().peekable();
    if lines.next().map(|(_, line)| line.trim()) != Some("*** Begin Patch") {
        return Err("a patch starts with the line `*** Begin Patch`".into());
    }
    let mut files = Vec::new();
    loop {
        let Some((n, line)) = lines.next() else { return Err("the patch has no `*** End Patch` line at its end".into()) };
        if line.trim() == "*** End Patch" {
            break;
        }
        let file = |rest: &str| -> Result<String, String> {
            let path = rest.trim();
            if path.is_empty() { Err(format!("line {} of the patch names no file", n + 1)) } else { Ok(path.to_owned()) }
        };
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            let path = file(rest)?;
            let mut added = Vec::new();
            while let Some((_, line)) = lines.next_if(|(_, line)| line.starts_with('+')) {
                added.push(line[1..].to_owned());
            }
            files.push(FileChange::Add { path, lines: added });
        } else if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            files.push(FileChange::Delete { path: file(rest)? });
        } else if let Some(rest) = line.strip_prefix("*** Update File: ") {
            let path = file(rest)?;
            let to = match lines.next_if(|(_, line)| line.starts_with("*** Move to: ")) {
                Some((_, line)) => Some(file(&line["*** Move to: ".len()..])?),
                None => None,
            };
            let chunks = chunks(&mut lines, &path)?;
            if chunks.is_empty() && to.is_none() {
                return Err(format!("the update of {path} changes nothing"));
            }
            files.push(FileChange::Update { path, to, chunks });
        } else {
            return Err(format!("line {} of the patch, {line:?}, isn't `*** Add File: `, `*** Delete File: `, `*** Update File: ` or `*** End Patch`", n + 1));
        }
    }
    if files.is_empty() {
        return Err("the patch changes no file".into());
    }
    Ok(Patch { files })
}

/// An update's chunks, up to the next file or the patch's end.
fn chunks<'a>(lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a str)>>, path: &str) -> Result<Vec<Chunk>, String> {
    let mut chunks = Vec::new();
    let mut chunk = Chunk::default();
    let changes = |chunk: &Chunk| !chunk.old.is_empty() || !chunk.new.is_empty();
    while let Some((n, line)) = lines.next_if(|(_, line)| !line.starts_with("*** ") || line.trim() == "*** End of File") {
        if line.trim() == "*** End of File" {
            chunk.at_end = true;
            chunks.push(std::mem::take(&mut chunk));
            continue;
        }
        if let Some(after) = line.strip_prefix("@@") {
            // A new chunk, unless this one has only its `@@` lines so far.
            if changes(&chunk) {
                chunks.push(std::mem::take(&mut chunk));
            }
            let after = after.strip_prefix(' ').unwrap_or(after);
            if !after.trim().is_empty() {
                chunk.after.push(after.to_owned());
            }
            continue;
        }
        match line.chars().next() {
            Some('+') => chunk.new.push(line[1..].to_owned()),
            Some('-') => chunk.old.push(line[1..].to_owned()),
            Some(' ') => {
                chunk.old.push(line[1..].to_owned());
                chunk.new.push(line[1..].to_owned());
            }
            // A blank line kept, its space lost on the way.
            None => {
                chunk.old.push(String::new());
                chunk.new.push(String::new());
            }
            Some(_) => return Err(format!("line {} of the patch, in {path}, {line:?}, doesn't start with a space, - or +", n + 1)),
        }
    }
    if changes(&chunk) {
        chunks.push(chunk);
    }
    Ok(chunks)
}

/// A file made of `lines`.
pub fn made(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// `file` with `chunks` applied, ending in a newline, as Codex leaves a
/// file. Its byte-order mark and line ends are kept.
pub fn updated(file: &str, chunks: &[Chunk]) -> Result<String, String> {
    let (bom, body) = match file.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", file),
    };
    let crlf = body.contains("\r\n");
    let body = body.replace("\r\n", "\n");
    let mut lines: Vec<String> = body.split('\n').map(str::to_owned).collect();
    // What the last newline leaves after it.
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut replaced: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut at = 0;
    for chunk in chunks {
        for after in &chunk.after {
            let found = seek(&lines, std::slice::from_ref(after), at, false).ok_or_else(|| format!("couldn't find the line {after:?} that the change follows"))?;
            at = found + 1;
        }
        // Only adds: at the file's end, as Codex puts them.
        if chunk.old.is_empty() {
            replaced.push((lines.len(), 0, chunk.new.clone()));
            continue;
        }
        let (mut old, mut new) = (&chunk.old[..], &chunk.new[..]);
        let mut found = seek(&lines, old, at, chunk.at_end);
        // A blank line at the end of what's replaced, which the file may
        // not have there.
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            found = seek(&lines, old, at, chunk.at_end);
        }
        let start = found.ok_or_else(|| format!("couldn't find the lines to change:\n{}", chunk.old.join("\n")))?;
        replaced.push((start, old.len(), new.to_vec()));
        at = start + old.len();
    }
    // From the last, so the places before stay where they were.
    replaced.sort_by_key(|(start, _, _)| *start);
    for (start, len, new) in replaced.into_iter().rev() {
        lines.splice(start..start + len, new);
    }
    let mut text = lines.join("\n");
    text.push('\n');
    if crlf {
        text = text.replace('\n', "\r\n");
    }
    Ok(format!("{bom}{text}"))
}

/// Where `pattern` starts in `lines`, at `from` or after: exactly, then
/// each line without what ends it, then trimmed, then with typographic
/// punctuation as plain. `at_end` looks at the end first.
fn seek(lines: &[String], pattern: &[String], from: usize, at_end: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(from);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    // Where to look, in order: from `from` on, or the end first.
    let starts: Vec<usize> = if at_end { std::iter::once(last).chain(from.min(last)..last).collect() } else { (from..=last).collect() };
    Alike::ALL.into_iter().find_map(|alike| starts.iter().copied().find(|&start| pattern.iter().enumerate().all(|(i, line)| alike.same(&lines[start + i], line))))
}

/// How alike two lines must be to be one, strictest first.
#[derive(Clone, Copy)]
enum Alike {
    Exactly,
    /// But for what ends them.
    ToTheirEnd,
    /// But for what starts and ends them.
    Trimmed,
    /// Trimmed, typographic punctuation as plain.
    Plain,
}

impl Alike {
    const ALL: [Alike; 4] = [Alike::Exactly, Alike::ToTheirEnd, Alike::Trimmed, Alike::Plain];

    fn same(self, a: &str, b: &str) -> bool {
        match self {
            Alike::Exactly => a == b,
            Alike::ToTheirEnd => a.trim_end() == b.trim_end(),
            Alike::Trimmed => a.trim() == b.trim(),
            Alike::Plain => plain(a.trim()) == plain(b.trim()),
        }
    }
}

/// `text` with typographic dashes, quotes and spaces as plain ones.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn reads_each_kind_of_change() {
        let patch = parse(
            "*** Begin Patch\n*** Add File: notes.txt\n+one\n+two\n*** Delete File: old.txt\n*** Update File: src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-    print(\"hi\")\n+    print(\"hello\")\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(
            patch.files,
            [
                FileChange::Add { path: "notes.txt".into(), lines: lines(&["one", "two"]) },
                FileChange::Delete { path: "old.txt".into() },
                FileChange::Update {
                    path: "src/app.py".into(),
                    to: Some("src/main.py".into()),
                    chunks: vec![Chunk { after: lines(&["def greet():"]), old: lines(&["    print(\"hi\")"]), new: lines(&["    print(\"hello\")"]), at_end: false }],
                },
            ]
        );
        assert_eq!(patch.paths(), ["notes.txt", "old.txt", "src/app.py", "src/main.py"]);
        assert_eq!(patch.files[2].sizes(), (1, 1));
    }

    #[test]
    fn what_isnt_a_patch_is_said_plainly() {
        for (input, said) in [
            ("{\"input\": \"x\"}", "starts with the line `*** Begin Patch`"),
            ("*** Begin Patch\n*** Update File: a.rs\n-x\n", "no `*** End Patch`"),
            ("*** Begin Patch\n*** End Patch", "changes no file"),
            ("*** Begin Patch\n*** Rename File: a.rs\n*** End Patch", "isn't `*** Add File: `"),
            ("*** Begin Patch\n*** Update File: a.rs\n-x\nnot a line\n*** End Patch", "doesn't start with a space, - or +"),
            ("*** Begin Patch\n*** Update File: a.rs\n*** End Patch", "changes nothing"),
        ] {
            let err = parse(input).unwrap_err();
            assert!(err.contains(said), "{input:?}: {err}");
        }
    }

    #[test]
    fn a_change_is_found_after_the_lines_it_follows() {
        let file = "def a():\n    return 1\n\ndef b():\n    return 1\n";
        let patch = parse("*** Begin Patch\n*** Update File: m.py\n@@ def b():\n-    return 1\n+    return 2\n*** End Patch").unwrap();
        let FileChange::Update { chunks, .. } = &patch.files[0] else { panic!() };
        assert_eq!(updated(file, chunks).unwrap(), "def a():\n    return 1\n\ndef b():\n    return 2\n", "b's, not a's");
        let missing = parse("*** Begin Patch\n*** Update File: m.py\n@@ def c():\n-    return 1\n+    x\n*** End Patch").unwrap();
        let FileChange::Update { chunks, .. } = &missing.files[0] else { panic!() };
        assert!(updated(file, chunks).unwrap_err().contains("def c():"));
    }

    #[test]
    fn lines_are_found_as_codex_finds_them() {
        let chunk = |old: &[&str], new: &[&str]| Chunk { old: lines(old), new: lines(new), ..Chunk::default() };
        // What ends a line, and what starts it.
        assert_eq!(updated("a  \nb\n", &[chunk(&["a", "b"], &["a", "c"])]).unwrap(), "a\nc\n");
        assert_eq!(updated("    a\n", &[chunk(&["a"], &["z"])]).unwrap(), "z\n");
        // Typographic quotes and dashes, as models write them.
        assert_eq!(updated("say(\"hi\") - ok\n", &[chunk(&["say(\u{201c}hi\u{201d}) \u{2014} ok"], &["done"])]).unwrap(), "done\n");
        assert!(updated("a\n", &[chunk(&["b"], &["c"])]).unwrap_err().contains("couldn't find the lines to change:\nb"));
    }

    #[test]
    fn a_file_keeps_its_line_ends_and_mark_and_ends_in_a_newline() {
        let chunk = Chunk { old: lines(&["two"]), new: lines(&["2"]), ..Chunk::default() };
        assert_eq!(updated("\u{feff}one\r\ntwo\r\n", std::slice::from_ref(&chunk)).unwrap(), "\u{feff}one\r\n2\r\n");
        assert_eq!(updated("one\ntwo", std::slice::from_ref(&chunk)).unwrap(), "one\n2\n");
        // Only adds go at the end, as Codex puts them.
        let adds = Chunk { new: lines(&["three"]), ..Chunk::default() };
        assert_eq!(updated("one\ntwo\n", &[adds]).unwrap(), "one\ntwo\nthree\n");
        let at_end = Chunk { old: lines(&["x"]), new: lines(&["y"]), at_end: true, ..Chunk::default() };
        assert_eq!(updated("x\nmid\nx\n", &[at_end]).unwrap(), "x\nmid\ny\n", "the last x: it ends the file");
        assert_eq!(made(&lines(&["a", "b"])), "a\nb\n");
    }

    #[test]
    fn several_chunks_change_a_file_in_order() {
        let patch = parse("*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+2\n@@\n three\n-four\n+4\n\n*** End Patch").unwrap();
        let FileChange::Update { chunks, .. } = &patch.files[0] else { panic!() };
        assert_eq!(chunks.len(), 2);
        assert_eq!(updated("one\ntwo\nthree\nfour\n", chunks).unwrap(), "one\n2\nthree\n4\n", "the blank line the file doesn't have is let go");
    }
}
