//! Loading a skill: its instructions, with what you gave it put where it
//! says (`$ARGUMENTS`), where its folder is and what's in it; or one of its
//! files, read inside its folder only.

use std::path::Path;

use serde::Deserialize;

use super::{Skill, Skills, body};
use crate::tools::ERROR;

/// The most of a skill's text one load gives: the rest comes from
/// `start_line`, as `read` gives a long file in parts.
const LONGEST: usize = 60_000;

/// The most of a skill's files named when it's loaded.
const FILES_NAMED: usize = 30;

#[derive(Deserialize)]
struct Asked {
    name: String,
    /// Not in calls from before skills took any.
    #[serde(default)]
    arguments: Option<String>,
    file: Option<String>,
    start_line: Option<usize>,
}

/// What loading a skill as `arguments` ask gives: its instructions, with
/// where its folder is and what's in it, or one of its files; or what's
/// wrong, with the names nearest to one that isn't a skill's.
pub fn load(skills: &Skills, arguments: &str) -> String {
    let asked: Asked = match serde_json::from_str(arguments) {
        Ok(asked) => asked,
        Err(e) => return format!("{ERROR}bad arguments: {e}"),
    };
    let Some(skill) = skills.named(&asked.name) else {
        let near = nearest(skills, &asked.name);
        let near = if near.is_empty() { String::new() } else { format!("; the nearest are {}", near.join(", ")) };
        return format!("{ERROR}there is no skill called {}{near}", asked.name);
    };
    let text = match asked.file.as_deref().filter(|file| !file.is_empty() && *file != "SKILL.md") {
        // Only what's in its folder, opened never through a link out of it.
        Some(file) => match read_inside(&skill.folder, file) {
            Ok(text) => text,
            Err(why) => return format!("{ERROR}{why}"),
        },
        None => match std::fs::read_to_string(skill.folder.join("SKILL.md")) {
            Ok(text) => render(body(&text).trim(), skill, asked.arguments.as_deref().unwrap_or_default().trim()),
            Err(e) => return format!("{ERROR}can't read {}'s SKILL.md: {e}", skill.name),
        },
    };
    let (shown, more) = part(&text, asked.start_line.unwrap_or(1));
    let file = asked.file.as_deref().unwrap_or("SKILL.md");
    let mut said = format!("<skill name=\"{}\" file=\"{file}\" folder=\"{}\">\n{shown}\n</skill>", skill.name, skill.folder.display());
    if let Some(next) = more {
        said.push_str(&format!("\n[more of it: load it again with start_line {next}]"));
    }
    if asked.file.is_none() && asked.start_line.is_none() {
        let files = files_in(&skill.folder);
        if !files.is_empty() {
            said.push_str(&format!("\nIts files, read with `file` as they're named here: {}", files.join(", ")));
        }
    }
    said
}

/// A skill's text with what it was given put where it says, as Claude
/// Code's skills expect: `$ARGUMENTS` all of it, `$ARGUMENTS[0]` a word of
/// it, `$name` one its `arguments` name, and `${CLAUDE_SKILL_DIR}` its
/// folder. Given something nothing took, it's said at the end.
///
/// A bare `$0` or `$1` is a word only in a skill that says it takes some,
/// and never in a sum like `$1,234` or `$2.4M`: in the skills people have,
/// it's money and SQL's parameters far more often than a placeholder,
/// which Claude Code fills regardless.
fn render(text: &str, skill: &Skill, arguments: &str) -> String {
    let words = words(arguments);
    let positional = skill.hint.is_some() || !skill.arguments.is_empty();
    let mut out = String::with_capacity(text.len());
    let mut took = false;
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let after = &rest[1..];
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let word = after.len() - after.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_').len();
        let index = |digits: &str| digits.parse::<usize>().ok().and_then(|n| words.get(n));
        let (put, used) = if let Some(more) = after.strip_prefix("{CLAUDE_SKILL_DIR}") {
            (Some(skill.folder.display().to_string()), rest.len() - more.len())
        } else if let Some(more) = after.strip_prefix("ARGUMENTS[")
            && let Some((n, more)) = more.split_once(']')
            && let Some(given) = index(n)
        {
            (Some(given.clone()), rest.len() - more.len())
        } else if &after[..word] == "ARGUMENTS" && !after[word..].starts_with('[') {
            (Some(arguments.to_owned()), 1 + word)
        } else if digits > 0
            && digits == word
            && positional
            && !a_sum(&after[digits..])
            && let Some(given) = index(&after[..digits])
        {
            (Some(given.clone()), 1 + digits)
        } else if word > 0
            && let Some(n) = skill.arguments.iter().position(|name| *name == after[..word])
        {
            (Some(words.get(n).cloned().unwrap_or_default()), 1 + word)
        } else {
            (None, 1)
        };
        match put {
            Some(put) => {
                took |= !rest.starts_with("${");
                out.push_str(&put);
            }
            None => out.push('$'),
        }
        rest = &rest[used..];
    }
    out.push_str(rest);
    if !arguments.is_empty() && !took {
        out.push_str(&format!("\n\nARGUMENTS: {arguments}"));
    }
    out
}

/// What follows `$1` makes it a sum of money: `,234`, `.50`, `M`, `-3`.
fn a_sum(after: &str) -> bool {
    let mut chars = after.chars();
    match chars.next() {
        Some(',' | '.' | '-') => chars.next().is_some_and(|c| c.is_ascii_digit()),
        Some(c) => c.is_alphabetic(),
        None => false,
    }
}

/// `arguments` in words, as a shell splits them: quotes keep a word's
/// spaces, and a backslash keeps the next character as it is.
fn words(arguments: &str) -> Vec<String> {
    let (mut words, mut word, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    let mut chars = arguments.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, c) if c.is_whitespace() => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (Some(open), c) if c == open => quote = None,
            (Some('\''), c) => word.push(c),
            (_, '\\') => {
                word.extend(chars.next());
                any = true;
            }
            (_, c) => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

/// The names of skills nearest to `name`, a few at most: those it's part
/// of, or a slip of a letter or two from.
fn nearest(skills: &Skills, name: &str) -> Vec<String> {
    let mut near: Vec<(usize, &str)> = skills
        .found
        .iter()
        .filter(|skill| skill.model)
        .map(|skill| {
            let short = skill.name.rsplit(':').next().unwrap_or(&skill.name);
            let distance = if skill.name.contains(name) || name.contains(short) { 0 } else { distance(short, name) };
            (distance, skill.name.as_str())
        })
        .filter(|(distance, _)| *distance <= 2.max(name.chars().count() / 4))
        .collect();
    near.sort();
    near.into_iter().take(3).map(|(_, name)| name.to_owned()).collect()
}

/// How many letters to change, add or take away to make `a` into `b`.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut last = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let was = row[j + 1];
            row[j + 1] = (last + usize::from(ca != *cb)).min(row[j] + 1).min(was + 1);
            last = was;
        }
    }
    row[b.len()]
}

/// `text` from line `first`, as much as [`LONGEST`] holds, and the line to
/// go on from when there's more.
fn part(text: &str, first: usize) -> (String, Option<usize>) {
    let lines: Vec<&str> = text.lines().collect();
    let first = first.max(1);
    let mut shown = String::new();
    for (at, line) in lines.iter().enumerate().skip(first - 1) {
        if shown.len() + line.len() > LONGEST && !shown.is_empty() {
            return (shown, Some(at + 1));
        }
        shown.push_str(line);
        shown.push('\n');
    }
    (shown.trim_end().to_owned(), None)
}

/// `file`, inside `folder`: never above it, and never through a link that
/// leads out.
fn read_inside(folder: &Path, file: &str) -> Result<String, String> {
    let path = Path::new(file);
    if path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(format!("{file} isn't in the skill's folder: name it as the skill does"));
    }
    let mut opened = crate::files::open(folder, path).map_err(|e| format!("can't read {file}: {e}"))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut opened, &mut bytes).map_err(|e| format!("can't read {file}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{file} isn't a UTF-8 text file"))
}

/// The files in `folder` but its SKILL.md, by their paths in it, a few
/// levels down, [`FILES_NAMED`] at most.
fn files_in(folder: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![(folder.to_owned(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() && depth < 3 {
                stack.push((path, depth + 1));
            } else if kind.is_file()
                && let Ok(rel) = path.strip_prefix(folder)
            {
                let rel = rel.to_string_lossy().into_owned();
                if rel != "SKILL.md" {
                    found.push(rel);
                }
            }
        }
    }
    found.sort();
    let more = found.len().saturating_sub(FILES_NAMED);
    found.truncate(FILES_NAMED);
    if more > 0 {
        found.push(format!("and {more} more"));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::super::tests::{dir, skill};
    use super::super::{find, folders};
    use super::*;

    fn asked(name: &str, arguments: Option<&str>) -> String {
        serde_json::json!({ "name": name, "arguments": arguments, "file": null, "start_line": null }).to_string()
    }

    #[test]
    fn a_skill_loads_its_text_and_its_files_but_nothing_outside_its_folder() {
        let home = dir("load");
        let mine = home.join(".agents/skills");
        skill(&mine, "graphs", "name: graphs\ndescription: Draw graphs.", "Step one.\nStep two.");
        std::fs::create_dir_all(mine.join("graphs/references")).unwrap();
        std::fs::write(mine.join("graphs/references/styles.md"), "Use blue.").unwrap();
        std::fs::write(home.join("secret.txt"), "FAKE-secret").unwrap();
        std::os::unix::fs::symlink(home.join("secret.txt"), mine.join("graphs/leak.md")).unwrap();
        let skills = find(&folders(&home, Some(&home), false));
        let loaded = load(&skills, r#"{"name":"graphs","file":null,"start_line":null}"#);
        assert!(loaded.starts_with(&format!("<skill name=\"graphs\" file=\"SKILL.md\" folder=\"{}\">\nStep one.\nStep two.\n</skill>", mine.join("graphs").display())), "{loaded}");
        assert!(loaded.ends_with("Its files, read with `file` as they're named here: references/styles.md"), "a link isn't named: {loaded}");
        assert!(load(&skills, r#"{"name":"graphs","file":"references/styles.md","start_line":null}"#).contains("Use blue."));
        for outside in ["../../../secret.txt", "/etc/passwd", "leak.md"] {
            let refused = load(&skills, &format!(r#"{{"name":"graphs","file":"{outside}","start_line":null}}"#));
            assert!(refused.starts_with(ERROR) && !refused.contains("FAKE"), "{outside}: {refused}");
        }
        let above = load(&skills, r#"{"name":"graphs","file":"../../../secret.txt","start_line":null}"#);
        assert!(above.contains("isn't in the skill's folder"), "said why: {above}");
        assert_eq!(load(&skills, &asked("grahps", None)), format!("{ERROR}there is no skill called grahps; the nearest are graphs"));
        assert_eq!(load(&skills, &asked("deploy", None)), format!("{ERROR}there is no skill called deploy"));
        let long = format!("{}\n", "a line of the skill\n".repeat(4_000));
        let (first, next) = part(&long, 1);
        assert!(first.len() <= LONGEST && next.is_some(), "a long one comes in parts");
        let (rest, after) = part(&long, next.unwrap());
        assert_eq!((first.lines().count() + rest.lines().count(), after), (4_000, None));
    }

    #[test]
    fn what_you_give_a_skill_goes_where_it_says() {
        let folder = std::path::PathBuf::from("/s/migrate");
        let migrate = Skill { name: "migrate".into(), folder: folder.clone(), hint: Some("[component] [from] [to]".into()), ..Skill::default() };
        assert_eq!(render("Migrate $0 from $1 to $2.", &migrate, "SearchBar JavaScript \"Type Script\""), "Migrate SearchBar from JavaScript to Type Script.");
        assert_eq!(render("Fix $ARGUMENTS, then $ARGUMENTS[1].", &migrate, "issue 12"), "Fix issue 12, then 12.");
        assert_eq!(render("Run ${CLAUDE_SKILL_DIR}/go.sh", &migrate, ""), "Run /s/migrate/go.sh", "the folder, given nothing");
        assert_eq!(render("Run ${CLAUDE_SKILL_DIR}/go.sh", &migrate, "now"), "Run /s/migrate/go.sh\n\nARGUMENTS: now", "the folder isn't what was given");
        assert_eq!(render("Use $3 here.", &migrate, "a"), "Use $3 here.\n\nARGUMENTS: a", "a word not given stays, and the rest is said");
        assert_eq!(render("Then $ARGUMENTS[2].", &migrate, "a"), "Then $ARGUMENTS[2].\n\nARGUMENTS: a", "so does one asked for by its place");
        let named = Skill { arguments: vec!["issue".into(), "branch".into()], ..migrate.clone() };
        assert_eq!(render("Fix #$issue on $branch, $issue again.", &named, "7"), "Fix #7 on , 7 again.");
    }

    /// From the skills people have: python-design-patterns' SQL, harden's
    /// `$1,234.56`, and board-deck-builder's `$2.4M ARR`.
    #[test]
    fn money_and_sql_are_left_as_they_are() {
        let plain = Skill { name: "patterns".into(), ..Skill::default() };
        let sql = "VALUES ($1, $2) RETURNING *";
        assert_eq!(render(sql, &plain, "refactor this"), format!("{sql}\n\nARGUMENTS: refactor this"), "a skill that takes nothing has no $1 to fill");
        let hinted = Skill { hint: Some("[target]".into()), ..plain };
        for money in ["format(1234.56); // $1,234.56", "We closed Q3 at $2.4M ARR", "a $1-3M range", "costs $0.50 each"] {
            assert_eq!(render(money, &hinted, "the form"), format!("{money}\n\nARGUMENTS: the form"), "{money}");
        }
        assert_eq!(render("a $ sign, $$, and $", &hinted, ""), "a $ sign, $$, and $");
    }

    #[test]
    fn arguments_split_as_a_shell_splits_them() {
        assert_eq!(words(r#"a "b c" 'd "e"' f\ g """#), ["a", "b c", "d \"e\"", "f g", ""]);
        assert!(words("   ").is_empty());
        assert_eq!(distance("grahps", "graphs"), 2);
    }

    #[test]
    fn a_skill_is_loaded_with_what_the_model_gives_it() {
        let home = dir("load-args");
        skill(&home.join(".agents/skills"), "issue", "name: issue\ndescription: Fix an issue.\nargument-hint: \"[number]\"", "Fix issue $0.");
        let skills = find(&folders(&home, Some(&home), false));
        assert!(load(&skills, &asked("issue", Some("42"))).contains("\nFix issue 42.\n"));
        assert!(load(&skills, r#"{"name":"issue","file":null,"start_line":null}"#).contains("Fix issue $0."), "a call from before arguments still loads");
    }
}
