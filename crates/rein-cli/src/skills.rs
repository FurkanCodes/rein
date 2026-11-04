//! Skills: yours, in `~/.agents/skills` and `~/.claude/skills`, and the
//! project's once you've said yes to them as they are; `/skills` to see
//! them and turn one off or keep it for you to call; `/name` (or
//! `/skill:name`) to call one with your message. See
//! [`rein_agent::skills`].

use std::collections::BTreeMap;
use std::path::Path;

use rein_agent::agent::{Agent, Llm};
use rein_agent::skills::{Skills, Whose};
use serde::{Deserialize, Serialize};

use crate::input::{Menu, Question};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;
use crate::{CliResult, config_dir};

/// What you chose for your skills, in rein's folder.
const CHOSEN_FILE: &str = "skills.json";

/// The projects whose skills you said yes to, each as they were then.
const TRUSTED_FILE: &str = "skills-trusted.json";

/// What you chose: skills turned off, and those only you call.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Chosen {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    off: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    yours: Vec<String>,
}

/// How a skill is offered, as you chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offered {
    /// The model loads it when a task fits, and you call it.
    On,
    /// Only you call it, with `/name`: the model isn't told of it.
    YoursToCall,
    Off,
}

/// The skills there are for the project at `root`: yours, and the
/// project's if you've said yes to them as they are, or say so now (when
/// `ask`, which a task run with `-p` isn't). What you chose in `/skills`
/// holds.
pub(crate) async fn find(root: &Path, terminal: &mut Terminal, ask: bool) -> Skills {
    let dir = config_dir().ok();
    let project = match rein_agent::skills::fingerprint(root) {
        Some(fingerprint) => {
            let key = root.display().to_string();
            let trusted = dir.as_deref().map(trusted).unwrap_or_default();
            if trusted.get(&key) == Some(&fingerprint) {
                true
            } else if ask && say_yes(root, terminal).await {
                if let Some(dir) = dir.as_deref() {
                    let mut trusted = trusted;
                    trusted.insert(key, fingerprint);
                    if let Err(e) = rein_llm::store::write_json_private(&dir.join(TRUSTED_FILE), &trusted) {
                        say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&format!("✗ can't keep that yes: {}", crate::describe(e))));
                    }
                }
                true
            } else {
                false
            }
        }
        None => false,
    };
    let folders = rein_agent::skills::folders(root, std::env::home_dir().as_deref(), project);
    let mut skills = rein_agent::skills::find(&folders);
    if !project && rein_agent::skills::fingerprint(root).is_some() {
        skills.left_out.push((root.join(".agents/skills"), "the project's: you haven't said yes to them as they are now".into()));
    }
    apply(&mut skills, &dir.as_deref().map(chosen).unwrap_or_default());
    skills
}

/// Asks whether the model may use the project's skills.
async fn say_yes(root: &Path, terminal: &mut Terminal) -> bool {
    let theirs = rein_agent::skills::find(&rein_agent::skills::folders(root, None, true));
    let names: Vec<&str> = theirs.found.iter().map(|skill| skill.name.as_str()).collect();
    if names.is_empty() {
        return false;
    }
    let question = Question {
        title: format!("Use this project's skills: {}?", names.join(", ")),
        detail: Some(
            "They came with the project, and may be anyone's: the model follows what they say, and their scripts run as commands do, asking as \
             yours do. A change to any of their files asks again."
                .into(),
        ),
        command: None,
        answers: vec![("y", "Yes, use them".to_owned()), ("n", "No, not this time".to_owned())],
    };
    terminal.yes(question).await
}

fn trusted(dir: &Path) -> BTreeMap<String, String> {
    rein_llm::store::read_json(&dir.join(TRUSTED_FILE)).ok().flatten().unwrap_or_default()
}

fn chosen(dir: &Path) -> Chosen {
    rein_llm::store::read_json(&dir.join(CHOSEN_FILE)).ok().flatten().unwrap_or_default()
}

/// `skills` as you chose: those off left out, saying so; those only you
/// call kept from the model.
fn apply(skills: &mut Skills, chosen: &Chosen) {
    let (off, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut skills.found).into_iter().partition(|skill| chosen.off.contains(&skill.name));
    skills.left_out.extend(off.into_iter().map(|skill| (skill.folder, format!("{}: you turned it off; /skills turns it on", skill.name))));
    skills.found = kept;
    for skill in &mut skills.found {
        if chosen.yours.contains(&skill.name) {
            skill.model = false;
        }
    }
}

/// The skill `line` calls, if it calls one: `/skill:name`, or `/name` for
/// one you may call that isn't one of rein's commands.
pub(crate) fn called(line: &str, skills: &Skills) -> Option<String> {
    let word = line.split_whitespace().next()?;
    let name = word.strip_prefix("/skill:").or_else(|| word.strip_prefix('/'))?;
    let rein_has = crate::commands::COMMANDS.iter().any(|(command, ..)| command.strip_prefix('/') == Some(name));
    let explicit = word.starts_with("/skill:");
    skills.found.iter().find(|skill| skill.name == name && skill.you && (explicit || !rein_has)).map(|skill| skill.name.clone())
}

/// `/skills`: every skill, where it's from and how it's offered, and what
/// was left out and why; pick one to change how it's offered.
pub(crate) async fn command(agent: &mut Agent<Llm>, terminal: &mut Terminal, root: &Path) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let dir = config_dir()?;
    let mut picked_chosen = chosen(&dir);
    let all = find(root, terminal, false).await;
    // Those turned off are still yours to turn on.
    let mut names: Vec<(String, Whose, String)> = all.found.iter().map(|skill| (skill.name.clone(), skill.whose, skill.description.clone())).collect();
    for name in &picked_chosen.off {
        if !names.iter().any(|(n, ..)| n == name) {
            names.push((name.clone(), Whose::Yours, String::new()));
        }
    }
    for (folder, why) in &all.left_out {
        if !why.contains("you turned it off") {
            say!("{pad}{}", s.dim(&format!("– {}: {why}", folder.display())));
        }
    }
    if names.is_empty() {
        say!("{pad}No skills yet: rein reads them from ~/.agents/skills and ~/.claude/skills, and the project's .agents/skills");
        return Ok(());
    }
    let state = |name: &str| {
        if picked_chosen.off.iter().any(|n| n == name) {
            Offered::Off
        } else if picked_chosen.yours.iter().any(|n| n == name) {
            Offered::YoursToCall
        } else {
            Offered::On
        }
    };
    let said = |offered: Offered| match offered {
        Offered::On => "on",
        Offered::YoursToCall => "only when you call it",
        Offered::Off => "off",
    };
    let items = names
        .iter()
        .map(|(name, whose, description)| {
            let whose = if *whose == Whose::Project { "the project's · " } else { "" };
            let short: String = description.chars().take(70).collect();
            (format!("/{name}"), format!("{whose}{} · {short}", said(state(name))))
        })
        .collect();
    let Some(picked) = terminal.input.choose(Menu { title: "skills: pick one to change".into(), items, current: None, at: None }).await else { return Ok(()) };
    let name = names[picked].0.clone();
    let choices = [Offered::On, Offered::YoursToCall, Offered::Off];
    let about = |offered: Offered| match offered {
        Offered::On => "the model loads it when a task fits, and you call it with /name",
        Offered::YoursToCall => "the model isn't told of it: you call it with /name",
        Offered::Off => "left out",
    };
    let items = choices.iter().map(|offered| (said(*offered).to_owned(), about(*offered).to_owned())).collect();
    let current = choices.iter().position(|offered| *offered == state(&name));
    let Some(choice) = terminal.input.choose(Menu { title: format!("/{name}: offer it how"), items, current, at: None }).await.and_then(|i| choices.get(i).copied()) else {
        return Ok(());
    };
    picked_chosen.off.retain(|n| *n != name);
    picked_chosen.yours.retain(|n| *n != name);
    match choice {
        Offered::On => {}
        Offered::YoursToCall => picked_chosen.yours.push(name.clone()),
        Offered::Off => picked_chosen.off.push(name.clone()),
    }
    rein_llm::store::write_json_private(&dir.join(CHOSEN_FILE), &picked_chosen).map_err(crate::describe)?;
    let skills = find(root, terminal, false).await;
    agent.set_skills(skills);
    say!("{pad}{} /{name} is {} now, from your next message", s.green("✓"), said(choice));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rein_agent::skills::Skill;

    fn skill(name: &str, you: bool) -> Skill {
        Skill { name: name.into(), description: "d".into(), folder: format!("/s/{name}").into(), whose: Whose::Yours, model: true, you, hint: None }
    }

    #[test]
    fn a_line_calls_a_skill_by_its_name_unless_reins_own_command_has_it() {
        let skills = Skills { found: vec![skill("graphify", true), skill("search", true), skill("quiet", false), skill("ckm:design-system", true)], left_out: Vec::new() };
        assert_eq!(called("/graphify src", &skills).as_deref(), Some("graphify"));
        assert_eq!(called("/search off", &skills), None, "rein's /search comes first");
        assert_eq!(called("/skill:search the docs", &skills).as_deref(), Some("search"), "named as a skill, it's the skill");
        assert_eq!(called("/quiet", &skills), None, "not one you call");
        assert_eq!(called("/ckm:design-system tokens", &skills).as_deref(), Some("ckm:design-system"));
        assert_eq!(called("look at /graphify", &skills), None, "only at the start");
    }

    #[test]
    fn what_you_chose_holds_off_left_out_and_yours_kept_from_the_model() {
        let mut skills = Skills { found: vec![skill("a", true), skill("b", true), skill("c", true)], left_out: Vec::new() };
        apply(&mut skills, &Chosen { off: vec!["a".into()], yours: vec!["b".into()] });
        let found: Vec<(&str, bool)> = skills.found.iter().map(|s| (s.name.as_str(), s.model)).collect();
        assert_eq!(found, [("b", false), ("c", true)]);
        assert!(skills.left_out[0].1.starts_with("a: you turned it off"));
    }
}
