//! Agent Skills (agentskills.io): folders with a `SKILL.md`, instructions
//! for a kind of task, that the model loads when a task fits one, and you
//! call with `/name`.
//!
//! rein finds them in your `~/.agents/skills` (the folder every harness
//! shares) and `~/.claude/skills` (Claude Code's), in the plugins you've
//! turned on in Claude Code (named `plugin:skill`, as there), and in the
//! project's `.agents/skills` and `.claude/skills`, from where rein starts
//! up to the repository's root, once you've said yes to them. The model is
//! offered one tool, `skill`, whose description lists every skill by name
//! with what it's for ([`catalog`]), and loads one with it ([`load`]): its
//! instructions, and the files beside them, read inside its folder only.
//!
//! Nothing in a skill runs because it's loaded, and what it says it may use
//! (`allowed-tools`) lets nothing run without asking: a skill is someone's
//! text, and a project's may be anyone's. So its name is held to letters,
//! digits and `-_.:`, and what it says is cleaned of what a terminal would
//! act on, before you or the model see it.

mod catalog;
mod load;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use catalog::{AS_LISTED, LEAST_ROOM, MOST_ROOM, changes, is_note, room, spec};
pub use load::load;

/// The most a name may have, as the spec says, a plugin's apart.
const NAME_LONGEST: usize = 64;

/// The most of a description kept, with its `when_to_use`, as Claude Code
/// keeps.
const DESCRIPTION_LONGEST: usize = 1_536;

/// Whose a skill is, which decides whether it's offered at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Whose {
    #[default]
    Yours,
    /// The project's: it came with it, and may be anyone's.
    Project,
    /// A Claude Code plugin's that you turned on there: yours, as you
    /// installed it.
    Plugin,
}

/// A skill, as its `SKILL.md` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skill {
    /// Its name; a plugin's is `plugin:skill`.
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
    /// The names its arguments go by, in order (`arguments`), each `$name`
    /// in its text.
    pub arguments: Vec<String>,
    /// How often it's been loaded, which keeps its description whole when
    /// the list must be cut.
    pub used: u32,
}

/// The skills found, and those left out, each with why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skills {
    pub found: Vec<Skill>,
    pub left_out: Vec<(PathBuf, String)>,
}

impl Skills {
    pub fn named(&self, name: &str) -> Option<&Skill> {
        self.found.iter().find(|skill| skill.name == name)
    }
}

/// A folder skills are found in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub path: PathBuf,
    pub whose: Whose,
    /// The plugin its skills are named after, `plugin:skill`.
    pub plugin: Option<String>,
    /// It's a skill's own folder, not a folder of them: a plugin may name
    /// one.
    pub single: bool,
}

impl Folder {
    fn of(path: PathBuf, whose: Whose) -> Folder {
        Folder { path, whose, plugin: None, single: false }
    }
}

/// The folders skills are found in, first first: the project's before
/// yours, as the spec has it, nearest first; the shared folder before
/// Claude Code's; plugins' last, whose names can't clash with the others'.
pub fn folders(root: &Path, home: Option<&Path>, project: bool) -> Vec<Folder> {
    let mut folders = Vec::new();
    if project {
        folders.extend(project_folders(root, home).into_iter().map(|path| Folder::of(path, Whose::Project)));
    }
    if let Some(home) = home {
        folders.extend([Folder::of(home.join(".agents/skills"), Whose::Yours), Folder::of(home.join(".claude/skills"), Whose::Yours)]);
        folders.extend(plugins(&home.join(".claude")));
    }
    folders
}

/// The project's skill folders: in `root`, and in each folder above it up
/// to the repository's root, as Codex and pi look, nearest first. Never
/// your home folder or above it, whose skills are yours, nor anywhere
/// above `root` outside a repository.
pub fn project_folders(root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_owned()];
    let mut at = root;
    while !at.join(".git").exists() {
        match at.parent() {
            Some(up) if !home.is_some_and(|home| home.starts_with(up)) => {
                dirs.push(up.to_owned());
                at = up;
            }
            _ => {
                dirs.truncate(1);
                break;
            }
        }
    }
    dirs.iter().flat_map(|dir| [dir.join(".agents/skills"), dir.join(".claude/skills")]).collect()
}

/// The skill folders of the plugins turned on in Claude Code, whose own
/// folder is `claude`: its settings say which are on, and its list of
/// what's installed where each is. A plugin's skills are in its `skills`
/// folder, and any its manifest names inside it. One installed only for a
/// project is that project's settings' to turn on, so it's left out.
fn plugins(claude: &Path) -> Vec<Folder> {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default, rename = "enabledPlugins")]
        enabled: HashMap<String, serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Installed {
        #[serde(default)]
        plugins: HashMap<String, Vec<Install>>,
    }
    #[derive(Deserialize)]
    struct Install {
        #[serde(default)]
        scope: String,
        #[serde(rename = "installPath")]
        path: PathBuf,
    }
    #[derive(Deserialize, Default)]
    struct Manifest {
        name: Option<String>,
        #[serde(default)]
        skills: Paths,
    }
    #[derive(Deserialize, Default)]
    #[serde(untagged)]
    enum Paths {
        #[default]
        None,
        One(String),
        Many(Vec<String>),
    }
    let read = |path: PathBuf| std::fs::read(path).ok();
    let Some(settings) = read(claude.join("settings.json")).and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok()) else { return Vec::new() };
    let Some(installed) = read(claude.join("plugins/installed_plugins.json")).and_then(|bytes| serde_json::from_slice::<Installed>(&bytes).ok()) else { return Vec::new() };
    let mut on: Vec<&String> = settings.enabled.iter().filter(|(_, value)| **value == serde_json::Value::Bool(true)).map(|(key, _)| key).collect();
    on.sort();
    let mut folders = Vec::new();
    for key in on {
        let Some(install) = installed.plugins.get(key).and_then(|installs| installs.iter().find(|install| install.scope == "user")) else { continue };
        let Ok(base) = install.path.canonicalize() else { continue };
        let manifest: Manifest = read(base.join(".claude-plugin/plugin.json")).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let fallback = key.split('@').next().unwrap_or(key);
        let name = manifest.name.filter(|name| fit_name(name)).unwrap_or_else(|| fallback.to_owned());
        if !fit_name(&name) {
            continue;
        }
        let named = match manifest.skills {
            Paths::None => Vec::new(),
            Paths::One(path) => vec![path],
            Paths::Many(paths) => paths,
        };
        let inside = |path: &str| {
            let path = Path::new(path);
            !path.is_absolute() && !path.components().any(|part| matches!(part, std::path::Component::ParentDir))
        };
        let paths = std::iter::once(base.join("skills")).chain(named.iter().filter(|path| inside(path)).map(|path| base.join(path)));
        for path in paths {
            if path.is_dir() {
                let single = path.join("SKILL.md").is_file();
                folders.push(Folder { path, whose: Whose::Plugin, plugin: Some(name.clone()), single });
            }
        }
    }
    folders
}

/// The skills in `folders`, each a folder in one with a `SKILL.md`. One
/// reached twice, through a link, counts once; a later one of a name taken
/// is left out, and so is one whose `SKILL.md` can't be read, or whose name
/// a skill's can't be, each said why.
pub fn find(folders: &[Folder]) -> Skills {
    let (mut skills, mut seen) = (Skills::default(), HashSet::new());
    for folder in folders {
        let entries = if folder.single {
            vec![folder.path.clone()]
        } else {
            let Ok(entries) = std::fs::read_dir(&folder.path) else { continue };
            let mut entries: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
            entries.sort();
            entries
        };
        for path in entries {
            // Kept out of sight, as Claude Code's trash is.
            if !folder.single && path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) {
                continue;
            }
            let Ok(real) = path.canonicalize() else { continue };
            if !real.is_dir() || !seen.insert(real.clone()) {
                continue;
            }
            if path.ends_with(".claude/skills/synced") && !real.join("SKILL.md").exists() {
                skills.left_out.push((path, "claude.ai's skills, which Claude Code keeps in step with your account: left to it".into()));
                continue;
            }
            match read(&path, &real, folder) {
                Ok(skill) => match skills.named(&skill.name) {
                    Some(first) => skills.left_out.push((path, format!("a skill called {} in {} comes first", skill.name, first.folder.display()))),
                    None => skills.found.push(skill),
                },
                Err(why) => skills.left_out.push((path, why)),
            }
        }
    }
    skills
}

/// The skill in the folder `path`, resolved as `real`, from `folder`; or
/// why it can't be one.
fn read(path: &Path, real: &Path, folder: &Folder) -> Result<Skill, String> {
    let text = std::fs::read_to_string(real.join("SKILL.md")).map_err(|_| "it has no SKILL.md".to_owned())?;
    let said = frontmatter(&text).ok_or("its SKILL.md has no frontmatter between --- lines")?;
    let tidy = |value: &str, longest: usize| {
        let words = rein_mcp::clean::text(value).split_whitespace().collect::<Vec<_>>().join(" ");
        words.chars().take(longest).collect::<String>()
    };
    let mut description = tidy(said.get("description").map_or("", String::as_str), DESCRIPTION_LONGEST);
    if description.is_empty() {
        return Err("its SKILL.md says nothing in description, which says when to use it".into());
    }
    if let Some(when) = said.get("when_to_use").map(|when| tidy(when, DESCRIPTION_LONGEST)).filter(|when| !when.is_empty()) {
        description = format!("{description} {when}").chars().take(DESCRIPTION_LONGEST).collect();
    }
    let folder_name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let name = said.get("name").map(|name| name.trim().to_owned()).filter(|name| !name.is_empty()).unwrap_or(folder_name);
    if !fit_name(&name) {
        return Err(format!("its name, {}, holds more than letters, digits and - _ . :, or is over {NAME_LONGEST} long", shown_name(&name)));
    }
    let no = |key: &str| said.get(key).is_some_and(|value| value.trim() == "false");
    let yes = |key: &str| said.get(key).is_some_and(|value| value.trim() == "true");
    let arguments = said.get("arguments").map(|named| named.split([' ', ',']).filter(|name| is_word(name)).map(str::to_owned).collect()).unwrap_or_default();
    Ok(Skill {
        name: folder.plugin.as_ref().map_or_else(|| name.clone(), |plugin| format!("{plugin}:{name}")),
        description,
        folder: real.to_owned(),
        whose: folder.whose,
        model: !yes("disable-model-invocation"),
        you: !no("user-invocable"),
        hint: said.get("argument-hint").map(|hint| tidy(hint, 100)).filter(|hint| !hint.is_empty()),
        arguments,
        used: 0,
    })
}

/// A name a skill may have: letters, digits and `-_.:`, not starting with
/// a dot, [`NAME_LONGEST`] at most. Nothing a terminal acts on, nothing
/// that turns text around, no space.
pub fn fit_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && name.chars().count() <= NAME_LONGEST && name.chars().all(|c| c.is_alphanumeric() || "-_.:".contains(c))
}

/// `name`, safe to show: anything but letters, digits and punctuation
/// written as its code.
fn shown_name(name: &str) -> String {
    name.chars().take(NAME_LONGEST + 1).map(|c| if c.is_alphanumeric() || c.is_ascii_punctuation() { c.to_string() } else { c.escape_unicode().to_string() }).collect()
}

/// A word an argument's name may be: `$name` in the skill's text.
fn is_word(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What the project's skills are, as one value: every file in its skill
/// folders, by path and content, so a change to any (a script, say)
/// asks for your yes again. `None` when it has none.
pub fn fingerprint(root: &Path, home: Option<&Path>) -> Option<String> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let mut any = false;
    for folder in project_folders(root, home) {
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
/// value plain (begun on its key's line or the next), quoted, a block (`>`
/// folds its lines, `|` keeps them), or a list (its items, spaced), and a
/// map nested under a key skipped. `None` without frontmatter.
fn frontmatter(text: &str) -> Option<HashMap<String, String>> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let body: Vec<&str> = lines.by_ref().take_while(|line| line.trim_end() != "---").collect();
    let mut said = HashMap::new();
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
        let lines: Vec<&str> = more.iter().copied().filter(|line| !line.is_empty()).collect();
        let joined = || std::iter::once(value).chain(lines.iter().copied()).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ");
        let value = match value.chars().next() {
            Some('|') => more.join("\n"),
            Some('>') => lines.join(" "),
            Some('"') => {
                let all = joined();
                all.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(&all).replace("\\\"", "\"")
            }
            Some('\'') => {
                let all = joined();
                all.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')).unwrap_or(&all).replace("''", "'")
            }
            None if lines.is_empty() => String::new(),
            None if lines.iter().all(|line| line.starts_with('-')) => {
                lines.iter().map(|line| line.trim_start_matches('-').trim().trim_matches(['"', '\''])).collect::<Vec<_>>().join(" ")
            }
            // A map nested under it: not a value of its own.
            None if lines.first().is_some_and(|line| looks_like_a_key(line)) => continue,
            _ => joined(),
        };
        said.insert(key.trim().to_owned(), value);
    }
    Some(said)
}

/// `line` starts a key of a map, `author: x`, not more of a value.
fn looks_like_a_key(line: &str) -> bool {
    line.split_once(':').is_some_and(|(key, rest)| !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || "-_".contains(c)) && (rest.is_empty() || rest.starts_with(' ')))
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-skills-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    pub(crate) fn skill(folder: &Path, name: &str, frontmatter: &str, body: &str) {
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

    /// As two of the skills Vercel publishes write it: a plain value begun
    /// on the line after its key, which rein once read as nothing and left
    /// the skill out.
    #[test]
    fn a_value_begun_on_the_next_line_or_a_list_is_read_too() {
        let said = frontmatter(
            "---\nname: rn\ndescription:\n  React Native and Expo best practices. Use\n  when building apps.\narguments:\n  - issue\n  - \"branch\"\nquoted: \"over\n  two\"\n---\n",
        )
        .unwrap();
        assert_eq!(said["description"], "React Native and Expo best practices. Use when building apps.");
        assert_eq!(said["arguments"], "issue branch");
        assert_eq!(said["quoted"], "over two");
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
        std::fs::create_dir_all(home.join(".claude/skills/.trash/old")).unwrap();
        skill(&home.join(".claude/skills/synced/account"), "pdf", "name: pdf\ndescription: claude.ai's.", "");
        std::os::unix::fs::symlink(mine.join("graphs"), home.join(".claude/skills/graphs")).unwrap();
        let skills = find(&folders(&project, Some(&home), true));
        let found: Vec<(&str, Whose, bool, bool)> = skills.found.iter().map(|s| (s.name.as_str(), s.whose, s.model, s.you)).collect();
        assert_eq!(found, [("review", Whose::Project, true, true), ("graphs", Whose::Yours, true, false), ("quiet", Whose::Yours, false, true)]);
        assert_eq!(skills.found[2].hint.as_deref(), Some("[what]"));
        let why: Vec<String> = skills.left_out.iter().map(|(path, why)| format!("{}: {why}", path.file_name().unwrap().to_string_lossy())).collect();
        assert_eq!(why.len(), 4, "the link to graphs counts once, not as a clash, and trash isn't said: {why:?}");
        assert!(why[0].starts_with("empty: its SKILL.md says nothing in description"), "{why:?}");
        assert!(why[1].starts_with("review: a skill called review in") && why[1].contains("project"), "{why:?}");
        assert_eq!(why[2], "synced: it has no SKILL.md", "only Claude Code's synced is claude.ai's");
        assert!(why[3].starts_with("synced: claude.ai's skills"), "{why:?}");
        let without = find(&folders(&project, Some(&home), false));
        let review = without.named("review").unwrap();
        assert_eq!((review.description.as_str(), review.whose), ("My way to review.", Whose::Yours), "the project's only once you've said yes to them");
    }

    /// A project's skill is someone else's text, shown in your terminal
    /// before you've said yes to it.
    #[test]
    fn what_a_terminal_would_act_on_never_gets_through() {
        let home = dir("names");
        let mine = home.join(".agents/skills");
        skill(&mine, "bell", "name: \"bell\u{7}\u{1b}[2J\"\ndescription: Rings.", "");
        skill(&mine, "spaced", "name: two words\ndescription: Spaced.", "");
        skill(&mine, "flipped", "name: flipped\ndescription: \"Safe \u{1b}]52;c;eA==\u{7}\u{202e}text\"\nargument-hint: \"[x]\u{1b}[31m\"", "");
        let skills = find(&folders(&home, Some(&home), false));
        assert_eq!(skills.found.len(), 1, "{skills:?}");
        assert_eq!(skills.found[0].description, "Safe ]52;c;eA==text");
        assert_eq!(skills.found[0].hint.as_deref(), Some("[x][31m"));
        let why: Vec<&str> = skills.left_out.iter().map(|(_, why)| why.as_str()).collect();
        assert!(why.iter().all(|why| !why.contains('\u{1b}') && !why.contains('\u{7}')), "{why:?}");
        assert!(why[0].contains(r"bell\u{7}\u{1b}[2J"), "said as its codes: {why:?}");
        assert!(why[1].contains("two\\u{20}words"), "{why:?}");
    }

    #[test]
    fn a_plugin_on_in_claude_code_brings_its_skills_named_after_it() {
        let home = dir("plugins");
        let claude = home.join(".claude");
        let installed = |name: &str| claude.join(format!("plugins/cache/market/{name}/1.0.0"));
        for name in ["kit", "off", "theirs"] {
            skill(&installed(name).join("skills"), "lint", "name: lint\ndescription: Lint things.", "Lint.");
        }
        skill(&installed("kit").join("extra"), "deploy", "name: deploy\ndescription: Ship it.", "Ship.");
        std::fs::create_dir_all(installed("kit").join(".claude-plugin")).unwrap();
        std::fs::write(installed("kit").join(".claude-plugin/plugin.json"), r#"{"name": "tools", "skills": ["./extra/deploy", "../../escape"]}"#).unwrap();
        std::fs::write(claude.join("settings.json"), r#"{"enabledPlugins": {"kit@market": true, "off@market": false, "theirs@market": true}}"#).unwrap();
        let entry = |name: &str, scope: &str| format!(r#""{name}@market": [{{"scope": "{scope}", "installPath": "{}"}}]"#, installed(name).display());
        std::fs::write(
            claude.join("plugins/installed_plugins.json"),
            format!(r#"{{"version": 2, "plugins": {{{}, {}, {}}}}}"#, entry("kit", "user"), entry("off", "user"), entry("theirs", "project")),
        )
        .unwrap();
        let skills = find(&folders(&home, Some(&home), false));
        let found: Vec<(&str, Whose)> = skills.found.iter().map(|s| (s.name.as_str(), s.whose)).collect();
        assert_eq!(found, [("tools:lint", Whose::Plugin), ("tools:deploy", Whose::Plugin)], "named as its manifest says; one turned off, or a project's, isn't");
        std::fs::write(claude.join("settings.json"), "{not json").unwrap();
        assert!(find(&folders(&home, Some(&home), false)).found.is_empty(), "settings that can't be read turn nothing on");
    }

    #[test]
    fn a_projects_skills_are_found_up_to_its_repository_and_never_at_home() {
        let home = dir("up");
        let repo = home.join("work/repo");
        let deep = repo.join("crates/one");
        std::fs::create_dir_all(&deep).unwrap();
        let shown = |folders: Vec<PathBuf>| folders.iter().map(|path| path.strip_prefix(&home).unwrap().display().to_string()).collect::<Vec<_>>();
        assert_eq!(shown(project_folders(&deep, Some(&home))), ["work/repo/crates/one/.agents/skills", "work/repo/crates/one/.claude/skills"], "outside a repository: here only");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(shown(project_folders(&deep, Some(&home)))[4..], ["work/repo/.agents/skills".to_owned(), "work/repo/.claude/skills".to_owned()]);
        std::fs::create_dir_all(home.join(".git")).unwrap();
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(project_folders(&deep, Some(&home)).len(), 2, "a repository at home doesn't make your skills the project's");
        skill(&repo.join(".agents/skills"), "up", "name: up\ndescription: Found from below.", "");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(fingerprint(&deep, Some(&home)).is_some(), "the yes covers what's found above too");
        assert_eq!(find(&folders(&deep, Some(&home), true)).found[0].whose, Whose::Project);
    }

    #[test]
    fn a_change_to_any_of_a_projects_skill_files_changes_its_fingerprint() {
        let project = dir("fingerprint");
        assert_eq!(fingerprint(&project, None), None, "no skills: nothing to say yes to");
        skill(&project.join(".agents/skills"), "deploy", "name: deploy\ndescription: Ship it.", "Run scripts/go.sh.");
        std::fs::create_dir_all(project.join(".agents/skills/deploy/scripts")).unwrap();
        std::fs::write(project.join(".agents/skills/deploy/scripts/go.sh"), "echo go").unwrap();
        let before = fingerprint(&project, None).unwrap();
        assert_eq!(fingerprint(&project, None).unwrap(), before);
        std::fs::write(project.join(".agents/skills/deploy/scripts/go.sh"), "echo no").unwrap();
        assert_ne!(fingerprint(&project, None).unwrap(), before, "a script changed, its length the same: asked again");
    }
}
