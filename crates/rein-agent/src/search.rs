//! Finding things in the project: `grep` for what files say, `glob` for
//! what they're called. Both look only where the project's `.gitignore`
//! files allow, as git would, so build output and dependencies stay out of
//! the way. They never follow a symlink, so they can't wander out of the
//! project, and they never look inside `.git`.
//!
//! What they send back is shaped for the model: a summary first, so it
//! survives if the rest is cut, then paths relative to the project root,
//! ready to `read`, sorted so the same search gives the same answer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use regex_lite::{Regex, RegexBuilder, escape};

/// The most matching lines `grep` shows; the summary still counts them all.
const MAX_MATCHES: usize = 200;
/// The most paths `glob` lists.
const MAX_FILES: usize = 500;
/// How much of the output may be results; the rest is the tool's own budget.
const BUDGET: usize = 25_000;
/// A matching line longer than this is cut: minified code is one long line.
const MAX_LINE: usize = 300;
/// The most lines of context around a match.
const MAX_CONTEXT: usize = 5;
/// Files bigger than this aren't searched: they're data, not code.
const MAX_FILE: u64 = 10 * 1024 * 1024;

const IGNORED_NOTE: &str = "files that .gitignore leaves out aren't searched";

/// What `grep` was asked for.
pub struct Grep<'a> {
    pub pattern: &'a str,
    /// A file or folder, resolved and inside the project.
    pub start: &'a Path,
    pub glob: Option<&'a str>,
    pub ignore_case: bool,
    /// Only which files match, not the lines.
    pub files_only: bool,
    pub context: usize,
}

/// Searches what files under `start` say. `root` is the project, resolved.
pub fn grep(root: &Path, args: &Grep) -> Result<String, String> {
    let regex = RegexBuilder::new(args.pattern)
        .case_insensitive(args.ignore_case)
        // `^` and `$` are the start and end of each line, as in grep.
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|e| format!("{:?} isn't a regular expression rein understands: {}", args.pattern, last_line(&e.to_string())))?;
    let filter = args.glob.map(Pattern::new).transpose()?;
    let start = relative(root, args.start);
    let files = if args.start.is_file() {
        vec![start.clone()]
    } else {
        let mut files = walk(root, args.start);
        if let Some(filter) = &filter {
            files.retain(|file| filter.matches(file, &start));
        }
        files
    };
    let context = args.context.min(MAX_CONTEXT);

    // Most of the time goes to opening and reading files, so they're
    // searched on every core at once; only those with matches are read
    // again, to show them.
    let found = in_parallel(&files, |file| text(&root.join(file)).map_or_else(Vec::new, |text| matching_lines(&regex, &text)));
    let (mut matches, mut matched_files) = (0, 0);
    let mut shown = 0;
    let mut body = String::new();
    let mut cut = false;
    for (file, found) in files.iter().zip(found) {
        if found.is_empty() {
            continue;
        }
        matches += found.len();
        matched_files += 1;
        if cut || body.len() > BUDGET {
            cut = true;
            continue;
        }
        if args.files_only {
            let times = if found.len() == 1 { "1 match".to_owned() } else { format!("{} matches", found.len()) };
            body += &format!("{file} ({times})\n");
            continue;
        }
        let Some(text) = text(&root.join(file)) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        body += &format!("\n{file}\n");
        let mut last: Option<usize> = None;
        for &line in &found {
            if shown == MAX_MATCHES || body.len() > BUDGET {
                cut = true;
                break;
            }
            let from = line.saturating_sub(context);
            let to = (line + context).min(lines.len().saturating_sub(1));
            // A gap between this match's lines and the last one's, when
            // there are lines around them.
            if context > 0 && last.is_some_and(|last| from > last + 1) {
                body += "--\n";
            }
            for (n, text) in lines.iter().enumerate().take(to + 1).skip(last.map_or(from, |last| from.max(last + 1))) {
                let mark = if n == line || found.binary_search(&n).is_ok() { ':' } else { '-' };
                body += &format!("{}{mark}{}\n", n + 1, shorten(text));
            }
            last = Some(to);
            shown += 1;
        }
    }
    if matches == 0 {
        let searched = count(files.len(), "file");
        return Ok(format!("No matches in {searched} ({IGNORED_NOTE})."));
    }
    let mut summary = if args.files_only {
        format!("{} with matches ({})", count(matched_files, "file"), count(matches, "matching line"))
    } else {
        format!("{} in {}", count(matches, "match"), count(matched_files, "file"))
    };
    if cut {
        summary += "; not all shown: narrow the pattern, path or glob to see the rest";
    }
    Ok(format!("{summary}\n{}", body.trim_start_matches('\n')).trim_end().to_owned())
}

/// Finds files under `start` by name or path. `root` is the project, resolved.
pub fn glob(root: &Path, pattern: &str, start: &Path) -> Result<String, String> {
    let pattern_ = Pattern::new(pattern)?;
    let from = relative(root, start);
    let files: Vec<String> = walk(root, start).into_iter().filter(|file| pattern_.matches(file, &from)).collect();
    if files.is_empty() {
        return Ok(format!("No files match {pattern:?} ({}).", IGNORED_NOTE.replace("searched", "listed")));
    }
    let mut out = count(files.len(), "file");
    if files.len() > MAX_FILES {
        out += &format!("; showing the first {MAX_FILES}: narrow the pattern to see the rest");
    }
    for file in files.iter().take(MAX_FILES) {
        out += &format!("\n{file}");
    }
    Ok(out)
}

/// `work` done for each of `items` on as many threads as there are cores,
/// the results in the items' order.
fn in_parallel<T: Sync, R: Send + Default>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(items.len()).max(1);
    let next = AtomicUsize::new(0);
    let mut results: Vec<R> = (0..items.len()).map(|_| R::default()).collect();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break done };
                        done.push((i, work(item)));
                    }
                })
            })
            .collect();
        for worker in workers {
            for (i, result) in worker.join().unwrap_or_default() {
                results[i] = result;
            }
        }
    });
    results
}

/// `3 files`, `1 file`.
fn count(n: usize, what: &str) -> String {
    let plural = if what.ends_with("ch") { "es" } else { "s" };
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}{plural}") }
}

/// `path` relative to `root`, with `/` between its parts; empty for the root.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// A file's text, or `None` for one that's too big or binary: one with a
/// zero byte near its start, as git decides.
fn text(path: &Path) -> Option<String> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return None;
    }
    Some(String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// The numbers of the lines with a match in them, from 0, in order.
fn matching_lines(regex: &Regex, text: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let (mut line, mut counted) = (0, 0);
    for m in regex.find_iter(text) {
        line += text.as_bytes()[counted..m.start()].iter().filter(|&&b| b == b'\n').count();
        counted = m.start();
        if found.last() != Some(&line) {
            found.push(line);
        }
    }
    found
}

/// A line as shown: without its line ending, and cut if it's very long.
fn shorten(line: &str) -> String {
    let line = line.trim_end_matches('\r');
    match line.char_indices().nth(MAX_LINE) {
        Some((at, _)) => format!("{}… [{} more characters]", &line[..at], line[at..].chars().count()),
        None => line.to_owned(),
    }
}

fn last_line(text: &str) -> &str {
    text.trim_end().lines().last().unwrap_or(text).trim_start_matches("error: ")
}

/// A glob the model gave: `*.rs` matches file names in any folder; one
/// with a slash, like `src/**/*.rs`, matches paths.
struct Pattern {
    regex: Regex,
    by_name: bool,
}

impl Pattern {
    fn new(glob: &str) -> Result<Pattern, String> {
        let glob = glob.trim().trim_start_matches("./").trim_start_matches('/');
        let regex = glob_regex(glob, true).map_err(|e| format!("{glob:?} isn't a glob rein understands: {e}"))?;
        Ok(Pattern { regex, by_name: !glob.contains('/') })
    }

    /// Whether `file` (relative to the project) matches: by its name, or by
    /// its path from the project root or from `from`, the folder searched.
    fn matches(&self, file: &str, from: &str) -> bool {
        if self.by_name {
            return self.regex.is_match(file.rsplit('/').next().unwrap_or(file));
        }
        let below = if from.is_empty() { Some(file) } else { file.strip_prefix(from).and_then(|rest| rest.strip_prefix('/')) };
        self.regex.is_match(file) || below.is_some_and(|below| self.regex.is_match(below))
    }
}

/// A glob as a regex that matches a whole path: `*` and `?` within one part
/// of it, `**` across parts, `[a-z]` as one of those characters, and, where
/// `braces` allows, `{a,b}` as either.
fn glob_regex(glob: &str, braces: bool) -> Result<Regex, regex_lite::Error> {
    let chars: Vec<char> = glob.chars().collect();
    let mut re = String::from("^");
    let mut open = 0;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                let whole_part = i == 0 || chars[i - 1] == '/';
                if whole_part && chars.get(i + 2) == Some(&'/') {
                    // `**/`: any folders, or none.
                    re += "(?:.*/)?";
                    i += 3;
                } else {
                    re += ".*";
                    i += 2;
                }
                continue;
            }
            '*' => re += "[^/]*",
            '?' => re += "[^/]",
            '[' => {
                // `]` right after `[` or `[!` is one of the characters.
                let first = i + 1 + usize::from(matches!(chars.get(i + 1), Some('!' | '^')));
                match (first + 1..chars.len()).find(|&j| chars[j] == ']') {
                    Some(close) => {
                        re += if first > i + 1 { "[^" } else { "[" };
                        for (k, &c) in chars[first..close].iter().enumerate() {
                            // A `-` between two characters is a range.
                            if c == '-' && k > 0 && first + k + 1 < close {
                                re.push('-');
                            } else {
                                re += &escape(&c.to_string());
                            }
                        }
                        re.push(']');
                        i = close + 1;
                        continue;
                    }
                    None => re += r"\[",
                }
            }
            '{' if braces => {
                open += 1;
                re += "(?:";
            }
            '}' if braces && open > 0 => {
                open -= 1;
                re.push(')');
            }
            ',' if braces && open > 0 => re.push('|'),
            '\\' if i + 1 < chars.len() => {
                re += &escape(&chars[i + 1].to_string());
                i += 2;
                continue;
            }
            c => re += &escape(&c.to_string()),
        }
        i += 1;
    }
    re += &")".repeat(open);
    re.push('$');
    Regex::new(&re)
}

/// One line of a `.gitignore`.
struct Rule {
    regex: Regex,
    /// `!pattern`: it's wanted after all.
    negate: bool,
    /// `pattern/`: only folders.
    folders_only: bool,
    /// It has a slash, so it's matched against the path from `base`, not
    /// against names at any depth.
    anchored: bool,
    /// The folder of the `.gitignore` it's from, relative to the root.
    base: String,
}

/// The rules in a `.gitignore` (or `.git/info/exclude`) file in `base`.
fn rules(file: &Path, base: &str) -> Vec<Rule> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            // Trailing spaces don't count, unless escaped.
            let mut line = line.trim_end_matches(['\r', ' ']).to_owned();
            if line.ends_with('\\') {
                line.push(' ');
            }
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (negate, line) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.to_owned()),
                None => (false, line.strip_prefix('\\').map_or(line.clone(), str::to_owned)),
            };
            let (folders_only, line) = match line.strip_suffix('/') {
                Some(rest) => (true, rest),
                None => (false, line.as_str()),
            };
            let anchored = line.contains('/');
            let pattern = line.trim_start_matches('/');
            let regex = glob_regex(pattern, false).ok()?;
            Some(Rule { regex, negate, folders_only, anchored, base: base.to_owned() })
        })
        .collect()
}

/// Whether `path` (relative to the root), named `name`, is ignored by
/// `rules`: the last one that matches decides, and deeper files' rules come
/// later.
fn ignored(rules: &[Rule], path: &str, name: &str, folder: bool) -> bool {
    let mut ignored = false;
    for rule in rules {
        if rule.folders_only && !folder {
            continue;
        }
        let below = if rule.base.is_empty() {
            path
        } else {
            match path.strip_prefix(&rule.base).and_then(|rest| rest.strip_prefix('/')) {
                Some(rest) => rest,
                None => continue,
            }
        };
        let target = if rule.anchored { below } else { name };
        if rule.regex.is_match(target) {
            ignored = !rule.negate;
        }
    }
    ignored
}

/// Every file under `start` that the `.gitignore` files don't leave out,
/// relative to `root`, sorted. The rules of the folders above `start` count
/// too, from the root down, and so does the repository's own
/// `.git/info/exclude`.
fn walk(root: &Path, start: &Path) -> Vec<String> {
    let mut all = rules(&root.join(".git/info/exclude"), "");
    all.extend(rules(&root.join(".gitignore"), ""));
    let from = relative(root, start);
    let mut base = PathBuf::new();
    for part in Path::new(&from).components() {
        base.push(part);
        all.extend(rules(&root.join(&base).join(".gitignore"), &relative(Path::new(""), &base)));
    }
    let mut files = Vec::new();
    visit(root, &from, &mut all, &mut files);
    files
}

fn visit(root: &Path, folder: &str, rules_: &mut Vec<Rule>, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root.join(folder)) else { return };
    let mut entries: Vec<(String, std::fs::FileType)> =
        entries.filter_map(|entry| entry.ok()).filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.file_type().ok()?))).collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, kind) in entries {
        // Never followed: a link could lead out of the project, or round in circles.
        if kind.is_symlink() || matches!(name.as_str(), ".git" | ".hg" | ".svn" | ".jj") {
            continue;
        }
        let path = if folder.is_empty() { name.clone() } else { format!("{folder}/{name}") };
        if ignored(rules_, &path, &name, kind.is_dir()) {
            continue;
        }
        if kind.is_dir() {
            let kept = rules_.len();
            rules_.extend(rules(&root.join(&path).join(".gitignore"), &path));
            visit(root, &path, rules_, files);
            rules_.truncate(kept);
        } else if kind.is_file() {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-search-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn grep_in(root: &Path, pattern: &str, glob: Option<&str>, files_only: bool, context: usize) -> String {
        grep(root, &Grep { pattern, start: root, glob, ignore_case: false, files_only, context }).unwrap()
    }

    #[test]
    fn globs_match_like_a_shell() {
        let is = |glob: &str, path: &str| glob_regex(glob, true).unwrap().is_match(path);
        assert!(is("*.rs", "main.rs") && !is("*.rs", "src/main.rs"), "* stays in one part of the path");
        assert!(is("**/*.rs", "main.rs") && is("**/*.rs", "src/ui/main.rs"));
        assert!(is("src/**", "src/a/b.txt") && !is("src/**", "srcx/a"));
        assert!(is("a/**/b", "a/b") && is("a/**/b", "a/x/y/b"));
        assert!(is("*.{ts,tsx}", "app.tsx") && !is("*.{ts,tsx}", "app.js"));
        assert!(is("test_?.py", "test_1.py") && !is("test_?.py", "test_10.py"));
        assert!(is("[a-c]*.md", "b.md") && !is("[!a-c]*.md", "b.md") && is("[!a-c]*.md", "x.md"));
        assert!(is("a+b(1).txt", "a+b(1).txt"), "other characters are themselves");
    }

    #[test]
    fn leaves_out_what_gitignore_does() {
        let root = project("ignore", &[
            (".gitignore", "# build output\ntarget/\n*.log\n!keep.log\n/only-top.txt\ndocs/*.html\n"),
            ("src/main.rs", ""),
            ("src/.gitignore", "generated.rs\n"),
            ("src/generated.rs", ""),
            ("target/debug/out", ""),
            ("a.log", ""),
            ("keep.log", ""),
            ("only-top.txt", ""),
            ("sub/only-top.txt", ""),
            ("docs/index.html", ""),
            ("docs/deep/page.html", ""),
            (".github/ci.yml", ""),
            (".git/config", ""),
        ]);
        assert_eq!(walk(&root, &root), [".github/ci.yml", ".gitignore", "docs/deep/page.html", "keep.log", "src/.gitignore", "src/main.rs", "sub/only-top.txt"]);
        // Starting further down, the rules above still count.
        assert_eq!(walk(&root, &root.join("src")), ["src/.gitignore", "src/main.rs"]);
    }

    #[test]
    fn never_follows_a_symlink() {
        let root = project("links", &[("a.txt", "secret-free")]);
        let outside = project("links-outside", &[("key", "secret")]);
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.join("key"), root.join("key")).unwrap();
        assert_eq!(walk(&root, &root), ["a.txt"]);
        assert!(grep_in(&root, "secret$", None, false, 0).starts_with("No matches in 1 file"));
    }

    #[test]
    fn grep_shows_matching_lines_by_file() {
        let root = project("grep", &[
            ("src/parser.rs", "use x;\nfn parse_list() {}\nfn other() {}\nfn parse_item() {}\n"),
            ("src/main.rs", "fn main() {\n    parse_list();\n}\n"),
            ("README.md", "parse_list is documented here\n"),
            ("logo.png", "\u{0}\u{1}parse_list"),
        ]);
        assert_eq!(
            grep_in(&root, r"parse_\w+\(", None, false, 0),
            "3 matches in 2 files\nsrc/main.rs\n2:    parse_list();\n\nsrc/parser.rs\n2:fn parse_list() {}\n4:fn parse_item() {}"
        );
        assert_eq!(grep_in(&root, "^fn", Some("*.rs"), true, 0), "2 files with matches (4 matching lines)\nsrc/main.rs (1 match)\nsrc/parser.rs (3 matches)", "^ is a line's start");
        assert_eq!(grep_in(&root, "parse_list", Some("src/**"), true, 0), "2 files with matches (2 matching lines)\nsrc/main.rs (1 match)\nsrc/parser.rs (1 match)");
        let one = grep(&root, &Grep { pattern: "PARSE_ITEM", start: &root.join("src/parser.rs"), glob: None, ignore_case: true, files_only: false, context: 0 }).unwrap();
        assert_eq!(one, "1 match in 1 file\nsrc/parser.rs\n4:fn parse_item() {}");
        assert_eq!(grep_in(&root, "nowhere", None, false, 0), "No matches in 4 files (files that .gitignore leaves out aren't searched).");
        let bad = grep(&root, &Grep { pattern: "fn (", start: &root, glob: None, ignore_case: false, files_only: false, context: 0 }).unwrap_err();
        assert!(bad.starts_with("\"fn (\" isn't a regular expression rein understands: ") && bad.contains("closing"), "{bad}");
    }

    #[test]
    fn grep_shows_context_without_repeating_lines() {
        let text: String = (1..=12).map(|n| if n == 3 || n == 5 || n == 11 { format!("hit {n}\n") } else { format!("line {n}\n") }).collect();
        let root = project("context", &[("a.txt", &text)]);
        assert_eq!(grep_in(&root, "hit", None, false, 1), "3 matches in 1 file\na.txt\n2-line 2\n3:hit 3\n4-line 4\n5:hit 5\n6-line 6\n--\n10-line 10\n11:hit 11\n12-line 12");
    }

    #[test]
    fn grep_says_when_it_doesnt_show_everything() {
        let text: String = (1..=300).map(|n| format!("match {n}\n")).collect();
        let root = project("many", &[("a.txt", &text), ("b.txt", "match\n")]);
        let out = grep_in(&root, "match", None, false, 0);
        assert!(out.starts_with("301 matches in 2 files; not all shown: narrow the pattern, path or glob to see the rest\na.txt\n1:match 1\n"), "{}", &out[..200]);
        assert!(out.ends_with("200:match 200"), "the first {MAX_MATCHES} are shown");
        let long = project("long-line", &[("min.js", &"x".repeat(1000))]);
        assert_eq!(grep_in(&long, "x", None, false, 0), format!("1 match in 1 file\nmin.js\n1:{}… [700 more characters]", "x".repeat(300)));
    }

    #[test]
    fn glob_finds_files_by_name_or_path() {
        let root = project("glob", &[("Cargo.toml", ""), ("src/main.rs", ""), ("src/ui/box.rs", ""), ("tests/screen.rs", ""), ("notes.md", "")]);
        assert_eq!(glob(&root, "*.rs", &root).unwrap(), "3 files\nsrc/main.rs\nsrc/ui/box.rs\ntests/screen.rs", "a name, in any folder");
        assert_eq!(glob(&root, "src/**/*.rs", &root).unwrap(), "2 files\nsrc/main.rs\nsrc/ui/box.rs");
        assert_eq!(glob(&root, "ui/*.rs", &root.join("src")).unwrap(), "1 file\nsrc/ui/box.rs", "from the folder asked about");
        assert_eq!(glob(&root, "./Cargo.toml", &root).unwrap(), "1 file\nCargo.toml");
        assert_eq!(glob(&root, "*.py", &root).unwrap(), "No files match \"*.py\" (files that .gitignore leaves out aren't listed).");
    }
}
