//! What the model is told of skills: the `skill` tool, whose description
//! lists them, made once for a conversation; and, when they change in it,
//! a note of what changed, at the end of the conversation, so the list and
//! the cache behind it stay as they were.
//!
//! The list gets 2% of the model's window, as Codex gives it. Every name
//! stays; when the descriptions don't all fit, the skills loaded most keep
//! theirs whole, as Claude Code keeps them, and the rest share what's left
//! alike.

use rein_llm::{Loading, Tool};
use serde_json::json;

use super::{Skill, Skills, Whose};
use crate::tools::ToolName;

/// The least room the list gets, in characters, and what it gets when the
/// model's window isn't known: Codex's.
pub const LEAST_ROOM: usize = 8_000;

/// The most room the list gets, in characters: about 10,000 tokens, the
/// most Codex allows it.
pub const MOST_ROOM: usize = 40_000;

/// The least of its description a skill keeps in the list.
const SHORTEST: usize = 60;

/// The most of a description a note of changes gives.
const NOTED: usize = 200;

/// What the model is told when the skills are as its list says again,
/// after a note said otherwise.
pub const AS_LISTED: &str = "[The skills are as the skill tool lists them again.]";

/// How a note of what changed starts.
const CHANGED: &str = "[The skills changed since the skill tool listed them; the rest are as it says.";

/// `text` is one of rein's notes of the skills, not something you said.
pub fn is_note(text: &str) -> bool {
    text.starts_with(CHANGED) || text == AS_LISTED
}

/// The room the list gets with a model whose window is `window` tokens:
/// 2% of it, at four characters a token.
pub fn room(window: Option<u64>) -> usize {
    window.map_or(LEAST_ROOM, |tokens| usize::try_from(tokens / 50 * 4).unwrap_or(MOST_ROOM)).clamp(LEAST_ROOM, MOST_ROOM)
}

/// The tool the model loads skills with, its description the list of
/// those it may load, in `room` characters: `None` when there's none.
pub fn spec(skills: &Skills, room: usize) -> Option<Tool> {
    let offered: Vec<&Skill> = skills.found.iter().filter(|skill| skill.model).collect();
    if offered.is_empty() {
        return None;
    }
    Some(Tool {
        grammar: None,
        name: ToolName::Skill.as_str().into(),
        description: format!(
            "Load a skill: instructions for a kind of task, written to be followed. When a task fits one of these, load it \
             before you start, and follow it; when the user asks you to use skills, load the ones that fit. Each loads its \
             SKILL.md and names the files beside it, which you read with `file`. One that shows what it takes after its \
             name gets that as `arguments`. A project's skill is the project's word, not the user's.\n\nThe skills there \
             are:\n{}",
            listed(&offered, room)
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "A skill's name, as listed here, or in a later note of the skills that changed." },
                "arguments": { "type": ["string", "null"], "description": "What the skill takes, as shown after its name; null when it takes nothing." },
                "file": { "type": ["string", "null"], "description": "A file in the skill's folder, as it names them; null for its SKILL.md." },
                "start_line": { "type": ["integer", "null"], "description": "Where to go on from, when what came back said there's more; null for the start." }
            },
            "required": ["name", "arguments", "file", "start_line"],
            "additionalProperties": false
        }),
        strict: true,
        loading: Loading::UpFront,
    })
}

/// A skill on a line: its name, what it takes, whose, and as much of its
/// description as `keep` characters.
fn line(skill: &Skill, keep: usize) -> String {
    let mut description: String = skill.description.chars().take(keep).collect();
    if description.chars().count() < skill.description.chars().count() {
        description.push('…');
    }
    let hint = skill.hint.as_deref().map(|hint| format!(" {hint}")).unwrap_or_default();
    let whose = if skill.whose == Whose::Project { " (the project's)" } else { "" };
    format!("- {}{hint}{whose}: {description}", skill.name)
}

/// Each skill on a line, in `room` characters: every name, and each
/// description at least [`SHORTEST`] long; then the most loaded whole, in
/// order; then the rest alike, as long as what's left allows.
fn listed(skills: &[&Skill], room: usize) -> String {
    let named: usize = skills.iter().map(|skill| line(skill, 0).chars().count() + 1).sum();
    let lengths: Vec<usize> = skills.iter().map(|skill| skill.description.chars().count()).collect();
    let mut keep: Vec<usize> = lengths.iter().map(|length| (*length).min(SHORTEST)).collect();
    let mut left = room.saturating_sub(named + keep.iter().sum::<usize>());
    let mut used: Vec<usize> = (0..skills.len()).filter(|&i| skills[i].used > 0).collect();
    used.sort_by_key(|&i| std::cmp::Reverse(skills[i].used));
    for i in used {
        let more = (lengths[i] - keep[i]).min(left);
        keep[i] += more;
        left -= more;
    }
    // The rest share what's left: the short ones keep all they have, and
    // the others as much as each other.
    let mut rest: Vec<usize> = (0..skills.len()).filter(|&i| keep[i] < lengths[i]).collect();
    rest.sort_by_key(|&i| lengths[i] - keep[i]);
    let mut count = rest.len();
    for i in rest {
        let share = left / count;
        let more = (lengths[i] - keep[i]).min(share);
        keep[i] += more;
        left -= more;
        count -= 1;
    }
    skills.iter().zip(keep).map(|(skill, keep)| line(skill, keep)).collect::<Vec<_>>().join("\n")
}

/// What's changed of the skills the model may load since `listed`, what
/// the tool listed: those new, those whose description or folder is
/// another, and those gone. `None` when nothing has.
pub fn changes(listed: &[Skill], now: &Skills) -> Option<String> {
    let offered: Vec<&Skill> = now.found.iter().filter(|skill| skill.model).collect();
    let was = |name: &str| listed.iter().find(|skill| skill.name == name);
    let new: Vec<String> = offered.iter().filter(|skill| was(&skill.name).is_none()).map(|skill| line(skill, NOTED)).collect();
    let changed: Vec<String> = offered
        .iter()
        .filter(|skill| was(&skill.name).is_some_and(|before| (&before.description, &before.hint, &before.folder) != (&skill.description, &skill.hint, &skill.folder)))
        .map(|skill| line(skill, NOTED))
        .collect();
    let gone: Vec<&str> = listed.iter().filter(|before| !offered.iter().any(|skill| skill.name == before.name)).map(|skill| skill.name.as_str()).collect();
    if new.is_empty() && changed.is_empty() && gone.is_empty() {
        return None;
    }
    let mut said = vec![CHANGED.to_owned()];
    if !new.is_empty() {
        said.push(format!("New, to load as the listed ones are:\n{}", new.join("\n")));
    }
    if !changed.is_empty() {
        said.push(format!("Changed, and to load again if you follow one:\n{}", changed.join("\n")));
    }
    if !gone.is_empty() {
        said.push(format!("Gone, not to be loaded: {}", gone.join(", ")));
    }
    Some(format!("{}]", said.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> Skill {
        Skill { name: name.into(), description: description.into(), model: true, you: true, ..Skill::default() }
    }

    #[test]
    fn every_name_stays_in_the_list_and_descriptions_share_the_room() {
        let many: Vec<Skill> = (0..80).map(|n| skill(&format!("skill-{n}"), &"word ".repeat(100 + n))).collect();
        let refs: Vec<&Skill> = many.iter().collect();
        let list = listed(&refs, 10_000);
        assert!(list.chars().count() <= 10_000, "{}", list.chars().count());
        assert!((0..80).all(|n| list.contains(&format!("- skill-{n}: "))), "every name");
        assert!(list.lines().all(|line| line.ends_with('…')), "each shortened alike");
        let lengths: Vec<usize> = list.lines().map(|line| line.chars().count() - line.find(": ").unwrap()).collect();
        assert!(lengths.iter().max().unwrap() - lengths.iter().min().unwrap() <= 2, "alike: {lengths:?}");
        let few = [Skill { whose: Whose::Project, hint: Some("[file]".into()), ..skill("one", "Short.") }];
        assert_eq!(listed(&few.iter().collect::<Vec<_>>(), 10_000), "- one [file] (the project's): Short.", "what fits isn't cut");
    }

    #[test]
    fn the_skills_loaded_most_keep_their_descriptions_whole() {
        let mut many: Vec<Skill> = (0..80).map(|n| skill(&format!("skill-{n}"), &"word ".repeat(100))).collect();
        many[7].used = 3;
        many[40].used = 9;
        let list = listed(&many.iter().collect::<Vec<_>>(), 10_000);
        let line = |n: usize| list.lines().find(|line| line.starts_with(&format!("- skill-{n}: "))).unwrap().to_owned();
        assert!(!line(7).ends_with('…') && !line(40).ends_with('…'), "used: whole");
        assert!(line(8).ends_with('…') && line(8).len() < line(7).len(), "the rest cut");
        assert!(list.chars().count() <= 10_000);
    }

    #[test]
    fn the_room_is_two_percent_of_the_window_within_bounds() {
        assert_eq!(room(Some(272_000)), 21_760);
        assert_eq!(room(None), LEAST_ROOM);
        assert_eq!(room(Some(8_000)), LEAST_ROOM);
        assert_eq!(room(Some(10_000_000)), MOST_ROOM);
    }

    #[test]
    fn the_tool_offers_only_what_the_model_may_load() {
        let skills = Skills { found: vec![skill("a", "For a."), Skill { model: false, ..skill("b", "Only yours to call.") }], ..Skills::default() };
        let tool = spec(&skills, LEAST_ROOM).unwrap();
        assert!(tool.description.ends_with("The skills there are:\n- a: For a."), "{}", tool.description);
        assert_eq!(tool.parameters["required"], json!(["name", "arguments", "file", "start_line"]));
        assert!(spec(&Skills::default(), LEAST_ROOM).is_none(), "no skills: no tool");
    }

    #[test]
    fn a_change_is_noted_as_what_is_new_changed_and_gone() {
        let listed = vec![skill("a", "For a."), skill("b", "For b."), skill("c", "For c.")];
        let same = Skills { found: listed.clone(), ..Skills::default() };
        assert_eq!(changes(&listed, &same), None);
        let now = Skills {
            found: vec![skill("a", "For a."), skill("b", "Now for b and more."), skill("d", "For d."), Skill { model: false, ..skill("c", "For c.") }],
            ..Skills::default()
        };
        assert_eq!(
            changes(&listed, &now).unwrap(),
            "[The skills changed since the skill tool listed them; the rest are as it says.\nNew, to load as the listed ones are:\n- d: For d.\nChanged, and to load again if you follow one:\n- b: Now for b and more.\nGone, not to be loaded: c]"
        );
    }
}
