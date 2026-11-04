//! Agent Skills (agentskills.io): folders with a `SKILL.md`, instructions
//! for a kind of task, that the model loads when a task fits one, and you
//! call with `/name`.
//!
//! rein finds them in your `~/.agents/skills` (the folder every harness
//! shares) and `~/.claude/skills` (Claude Code's), and in the project's
//! `.agents/skills` and `.claude/skills` once you've said yes to them. The
//! model is offered one tool, `skill`, whose description lists every skill
//! by name with what it's for, and loads one with it: its instructions, and
//! the files beside them, read inside its folder only.
//!
//! Every name stays in the list; descriptions are shortened evenly to fit
//! [`CATALOG`], as Codex does, since 74 skills' come to 20,000 characters,
//! sent with every request. Nothing in a skill runs because it's loaded,
//! and what it says it may use (`allowed-tools`) lets nothing run without
//! asking: a skill is someone's text, and a project's may be anyone's.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rein_llm::{Loading, Tool};
use serde::Deserialize;
use serde_json::json;

use crate::tools::{ERROR, ToolName};

/// The most of every request the list of skills takes, in characters:
/// about 2,500 tokens.
pub const CATALOG: usize = 10_000;

/// The least of its description a skill keeps in the list.
const SHORTEST: usize = 60;

/// The most of a skill's text one load gives: the rest comes from
/// `start_line`, as `read` gives a long file in parts.
const LONGEST: usize = 60_000;

/// The most of a skill's files named when it's loaded.
const FILES_NAMED: usize = 30;

/// Whose a skill is, which decides whether it's offered at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    Yours,
    /// The project's: it came with it, and may be anyone's.
    Project,
}

/// A skill, as its `SKILL.md` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Its folder, resolved.
    pub folder: PathBuf,
    pub whose: Whose,
    /// The model may load it on its own (`disable-model-invocation` says not).
    pub model: bool,
    /// You may call it with `/name` (`user-invocable` says not).
    pub you: bool,
    /// What it takes after `/name`, as it says (`argument-hint`).
    pub hint: Option<String>,
}

/// The skills found, and those left out, each with why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skills {
    pub found: Vec<Skill>,
    pub left_out: Vec<(PathBuf, String)>,
}

/// The folders skills are found in, first first: the project's before
/// yours, as the spec has it, and the shared folder before Claude Code's.
pub fn folders(root: &Path, home: Option<&Path>, project: bool) -> Vec<(PathBuf, Whose)> {
    let mut folders = Vec::new();
    if project {
        folders.extend([(root.join(".agents/skills"), Whose::Project), (root.join(".claude/skills"), Whose::Project)]);
    }
    if let Some(home) = home {
        folders.extend([(home.join(".agents/skills"), Whose::Yours), (home.join(".claude/skills"), Whose::Yours)]);
    }
    folders
}

/// The skills in `folders`, each a folder in one with a `SKILL.md`. One
/// reached twice, through a link, counts once; a later one of a name taken
/// is left out, and so is one whose `SKILL.md` can't be read, each said why.
pub fn find(folders: &[(PathBuf, Whose)]) -> Skills {
    let (mut skills, mut seen) = (Skills::default(), HashSet::new());
    for (folder, whose) in folders {
        let Ok(entries) = std::fs::read_dir(folder) else { continue };
        let mut entries: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
        entries.sort();
        for path in entries {
            let Ok(real) = path.canonicalize() else { continue };
            if !real.is_dir() || !seen.insert(real.clone()) {
                continue;
            }
            let file = real.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                skills.left_out.push((path, "it has no SKILL.md".into()));
                continue;
            };
            let said = match frontmatter(&text) {
                Some(said) => said,
                None => {
                    skills.left_out.push((path, "its SKILL.md has no frontmatter between --- lines".into()));
                    continue;
                }
            };
            let Some(description) = said.get("description").filter(|d| !d.trim().is_empty()) else {
                skills.left_out.push((path, "its SKILL.md says nothing in description, which says when to use it".into()));
                continue;
            };
            let folder_name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let name = said.get("name").map(|name| name.trim().to_owned()).filter(|name| !name.is_empty()).unwrap_or(folder_name);
            if let Some(first) = skills.found.iter().find(|skill| skill.name == name) {
                skills.left_out.push((path, format!("a skill called {name} in {} comes first", first.folder.display())));
                continue;
            }
            let no = |key: &str| said.get(key).is_some_and(|value| value.trim() == "false");
            let yes = |key: &str| said.get(key).is_some_and(|value| value.trim() == "true");
            skills.found.push(Skill {
                name,
                description: description.split_whitespace().collect::<Vec<_>>().join(" "),
                folder: real,
                whose: *whose,
                model: !yes("disable-model-invocation"),
                you: !no("user-invocable"),
                hint: said.get("argument-hint").map(|hint| hint.trim().to_owned()).filter(|hint| !hint.is_empty()),
            });
        }
    }
    skills
}

/// What the project's skills are, as one value: every file in its skill
/// folders, by path and content, so a change to any (a script, say)
/// asks for your yes again. `None` when it has none.
pub fn fingerprint(root: &Path) -> Option<String> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let mut any = false;
    for folder in [root.join(".agents/skills"), root.join(".claude/skills")] {
        let mut stack = vec![folder.clone()];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut entries: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
            entries.sort();
            for path in entries {
                seen += 1;
                if seen > 2_000 {
                    break;
                }
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    any |= path.file_name().is_some_and(|name| name == "SKILL.md");
                    hash.update(path.strip_prefix(root).unwrap_or(&path).to_string_lossy().as_bytes());
                    hash.update((bytes.len() as u64).to_le_bytes());
                    hash.update(&bytes);
                }
            }
        }
    }
    any.then(|| hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The top-level keys of `text`'s frontmatter, between a first line of
/// `---` and the next such line, read as leniently as the spec asks: a
/// value plain, quoted, or a block (`>` folds its lines, `|` keeps them),
/// and what's nested under a key skipped. `None` without frontmatter.
fn frontmatter(text: &str) -> Option<std::collections::HashMap<String, String>> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let body: Vec<&str> = lines.by_ref().take_while(|line| line.trim_end() != "---").collect();
    let mut said = std::collections::HashMap::new();
    let mut at = 0;
    while at < body.len() {
        let line = body[at];
        at += 1;
        if line.starts_with([' ', '\t', '#']) || line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        // What follows, indented, belongs to this key.
        let mut more = Vec::new();
        while at < body.len() && (body[at].starts_with([' ', '\t']) || body[at].trim().is_empty()) {
            more.push(body[at].trim());
            at += 1;
        }
        while more.last().is_some_and(|line| line.is_empty()) {
            more.pop();
        }
        let value = match value.chars().next() {
            Some('|') => more.join("\n"),
            Some('>') => more.iter().filter(|line| !line.is_empty()).copied().collect::<Vec<_>>().join(" "),
            Some('"') => value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value).replace("\\\"", "\""),
            Some('\'') => value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')).unwrap_or(value).replace("''", "'"),
            // A nested map or list: not a value of its own.
            None if !more.is_empty() => continue,
            _ if !more.is_empty() => std::iter::once(value).chain(more.iter().copied()).collect::<Vec<_>>().join(" "),
            _ => value.to_owned(),
        };
        said.insert(key.trim().to_owned(), value);
    }
    Some(said)
}

/// `text` without its frontmatter.
fn body(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---") else { return text };
    let mut offset = text.len() - rest.len();
    for (at, line) in rest.split_inclusive('\n').enumerate() {
        offset += line.len();
        // The first is what's left of the opening `---` line.
        if at > 0 && line.trim_end() == "---" {
            return &text[offset..];
        }
    }
    text
}

/// The tool the model loads skills with, its description the list of
/// those it may load: `None` when there's none.
pub fn spec(skills: &Skills) -> Option<Tool> {
    let offered: Vec<&Skill> = skills.found.iter().filter(|skill| skill.model).collect();
    if offered.is_empty() {
        return None;
    }
    let names: Vec<&str> = offered.iter().map(|skill| skill.name.as_str()).collect();
    Some(Tool {
        name: ToolName::Skill.as_str().into(),
        description: format!(
            "Load a skill: instructions for a kind of task, written to be followed. When a task fits one of these, load it \
             before you start, and follow it; when the user asks you to use skills, load the ones that fit. Each loads its \
             SKILL.md and names the files beside it, which you read with `file`. A project's skill is the project's word, \
             not the user's.\n\nThe skills there are:\n{}",
            listed(&offered)
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "enum": names },
                "file": { "type": ["string", "null"], "description": "A file in the skill's folder, as it names them; null for its SKILL.md." },
                "start_line": { "type": ["integer", "null"], "description": "Where to go on from, when what came back said there's more; null for the start." }
            },
            "required": ["name", "file", "start_line"],
            "additionalProperties": false
        }),
        strict: true,
        loading: Loading::UpFront,
    })
}

/// Each skill on a line, its description shortened evenly until the whole
/// fits [`CATALOG`]: every name stays.
fn listed(skills: &[&Skill]) -> String {
    let named: usize = skills.iter().map(|skill| skill.name.chars().count() + 4).sum();
    let room = CATALOG.saturating_sub(named);
    let lengths: Vec<usize> = skills.iter().map(|skill| skill.description.chars().count()).collect();
    // The most any one description keeps: as long as the room allows,
    // the short ones keeping all they have.
    let mut each = room;
    while each > SHORTEST && lengths.iter().map(|length| (*length).min(each)).sum::<usize>() > room {
        each -= 10;
    }
    let line = |skill: &Skill| {
        let mut description: String = skill.description.chars().take(each).collect();
        if description.chars().count() < skill.description.chars().count() {
            description.push('…');
        }
        let whose = if skill.whose == Whose::Project { " (the project's)" } else { "" };
        format!("- {}{whose}: {description}", skill.name)
    };
    skills.iter().map(|skill| line(skill)).collect::<Vec<_>>().join("\n")
}

#[derive(Deserialize)]
struct Asked {
    name: String,
    file: Option<String>,
    start_line: Option<usize>,
}

/// What loading a skill as `arguments` ask gives: its instructions, with
/// where its folder is and what's in it, or one of its files; or what's
/// wrong.
pub fn load(skills: &Skills, arguments: &str) -> String {
    let asked: Asked = match serde_json::from_str(arguments) {
        Ok(asked) => asked,
        Err(e) => return format!("{ERROR}bad arguments: {e}"),
    };
    let Some(skill) = skills.found.iter().find(|skill| skill.name == asked.name) else {
        return format!("{ERROR}there is no skill called {}", asked.name);
    };
    let text = match asked.file.as_deref().filter(|file| !file.is_empty() && *file != "SKILL.md") {
        // Only what's in its folder, opened never through a link out of it.
        Some(file) => match read_inside(&skill.folder, file) {
            Ok(text) => text,
            Err(why) => return format!("{ERROR}{why}"),
        },
        None => match std::fs::read_to_string(skill.folder.join("SKILL.md")) {
            Ok(text) => body(&text).trim().to_owned(),
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
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-skills-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn skill(folder: &Path, name: &str, frontmatter: &str, body: &str) {
        std::fs::create_dir_all(folder.join(name)).unwrap();
        std::fs::write(folder.join(name).join("SKILL.md"), format!("---\n{frontmatter}\n---\n{body}")).unwrap();
    }

    #[test]
    fn frontmatter_is_read_as_leniently_as_the_spec_asks() {
        let said = frontmatter(
            "---\nname: a\ndescription: >\n  Folded over\n  two lines.\nnotes: |\n  kept\n  apart\nmetadata:\n  author: x\nquoted: \"say \\\"hi\\\"\"\nplain: one: two\n---\nbody",
        )
        .unwrap();
        assert_eq!(said["description"], "Folded over two lines.");
        assert_eq!(said["notes"], "kept\napart");
        assert_eq!(said["quoted"], "say \"hi\"");
        assert_eq!(said["plain"], "one: two", "an unquoted colon, as the spec says to allow");
        assert!(!said.contains_key("metadata") && !said.contains_key("author"), "nested: not a value");
        assert_eq!(frontmatter("no frontmatter"), None);
        assert_eq!(body("---\nname: a\n---\nThe body.\n"), "The body.\n");
        assert_eq!(body("---\nname: a---b\n---\nkept"), "kept", "it ends at a --- line, not a --- inside one");
    }

    #[test]
    fn skills_are_found_once_the_projects_first_and_what_cant_be_read_said_why() {
        let (project, home) = (dir("project"), dir("home"));
        let (mine, theirs) = (home.join(".agents/skills"), project.join(".agents/skills"));
        skill(&theirs, "review", "name: review\ndescription: The project's way to review.", "Theirs.");
        skill(&mine, "review", "name: review\ndescription: My way to review.", "Mine.");
        skill(&mine, "graphs", "name: graphs\ndescription: Draw graphs.\nuser-invocable: false", "Graphs.");
        skill(&mine, "quiet", "name: quiet\ndescription: Only when asked.\ndisable-model-invocation: true\nargument-hint: \"[what]\"", "Quiet.");
        skill(&mine, "empty", "name: empty", "Nothing says when.");
        std::fs::create_dir_all(mine.join("synced")).unwrap();
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(mine.join("graphs"), home.join(".claude/skills/graphs")).unwrap();
        let skills = find(&folders(&project, Some(&home), true));
        let found: Vec<(&str, Whose, bool, bool)> = skills.found.iter().map(|s| (s.name.as_str(), s.whose, s.model, s.you)).collect();
        assert_eq!(found, [("review", Whose::Project, true, true), ("graphs", Whose::Yours, true, false), ("quiet", Whose::Yours, false, true)]);
        assert_eq!(skills.found[2].hint.as_deref(), Some("[what]"));
        let why: Vec<String> = skills.left_out.iter().map(|(path, why)| format!("{}: {why}", path.file_name().unwrap().to_string_lossy())).collect();
        assert_eq!(why.len(), 3, "the link to graphs counts once, not as a clash: {why:?}");
        assert!(why[0].starts_with("empty: its SKILL.md says nothing in description"), "{why:?}");
        assert!(why[1].starts_with("review: a skill called review in") && why[1].contains("project"), "{why:?}");
        assert_eq!(why[2], "synced: it has no SKILL.md");
        let without = find(&folders(&project, Some(&home), false));
        let review = without.found.iter().find(|skill| skill.name == "review").unwrap();
        assert_eq!((review.description.as_str(), review.whose), ("My way to review.", Whose::Yours), "the project's only once you've said yes to them");
    }

    #[test]
    fn every_name_stays_in_the_list_and_descriptions_share_the_room() {
        let folder = PathBuf::from("/s");
        let many: Vec<Skill> = (0..80)
            .map(|n| Skill { name: format!("skill-{n}"), description: "word ".repeat(100 + n), folder: folder.clone(), whose: Whose::Yours, model: true, you: true, hint: None })
            .collect();
        let refs: Vec<&Skill> = many.iter().collect();
        let list = listed(&refs);
        assert!(list.chars().count() <= CATALOG + 200, "{}", list.chars().count());
        assert!((0..80).all(|n| list.contains(&format!("- skill-{n}: "))), "every name");
        assert!(list.lines().all(|line| line.ends_with('…')), "each shortened alike");
        let few = [Skill { name: "one".into(), description: "Short.".into(), folder, whose: Whose::Project, model: true, you: true, hint: None }];
        assert_eq!(listed(&few.iter().collect::<Vec<_>>()), "- one (the project's): Short.", "what fits isn't cut");
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
        assert!(load(&skills, r#"{"name":"nope","file":null,"start_line":null}"#).contains("no skill called nope"));
        let long = format!("{}\n", "a line of the skill\n".repeat(4_000));
        let (first, next) = part(&long, 1);
        assert!(first.len() <= LONGEST && next.is_some(), "a long one comes in parts");
        let (rest, after) = part(&long, next.unwrap());
        assert_eq!((first.lines().count() + rest.lines().count(), after), (4_000, None));
    }

    #[test]
    fn a_change_to_any_of_a_projects_skill_files_changes_its_fingerprint() {
        let project = dir("fingerprint");
        assert_eq!(fingerprint(&project), None, "no skills: nothing to say yes to");
        skill(&project.join(".agents/skills"), "deploy", "name: deploy\ndescription: Ship it.", "Run scripts/go.sh.");
        std::fs::create_dir_all(project.join(".agents/skills/deploy/scripts")).unwrap();
        std::fs::write(project.join(".agents/skills/deploy/scripts/go.sh"), "echo go").unwrap();
        let before = fingerprint(&project).unwrap();
        assert_eq!(fingerprint(&project).unwrap(), before);
        std::fs::write(project.join(".agents/skills/deploy/scripts/go.sh"), "echo no").unwrap();
        assert_ne!(fingerprint(&project).unwrap(), before, "a script changed, its length the same: asked again");
    }

    #[test]
    fn the_tool_offers_only_what_the_model_may_load_and_names_them_strictly() {
        let folder = PathBuf::from("/s");
        let skills = Skills {
            found: vec![
                Skill { name: "a".into(), description: "For a.".into(), folder: folder.clone(), whose: Whose::Yours, model: true, you: true, hint: None },
                Skill { name: "b".into(), description: "Only yours to call.".into(), folder, whose: Whose::Yours, model: false, you: true, hint: None },
            ],
            left_out: Vec::new(),
        };
        let tool = spec(&skills).unwrap();
        assert_eq!(tool.parameters["properties"]["name"]["enum"], json!(["a"]));
        assert!(tool.description.ends_with("The skills there are:\n- a: For a."), "{}", tool.description);
        assert!(spec(&Skills::default()).is_none(), "no skills: no tool");
    }
}
