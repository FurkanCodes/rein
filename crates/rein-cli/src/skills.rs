//! Skills: yours, in `~/.agents/skills` and `~/.claude/skills`, the plugins
//! you turned on in Claude Code, and the project's once you've said yes to
//! them as they are. They're looked at again before each message, so one
//! you add or change is there without starting again.
//!
//! You call one with `/name` (or `/skill:name`, when rein has a command of
//! its name) at the start of a message, given what follows; or with
//! `/name` anywhere in it, which loads it with the message. Typing `/`
//! lists them under the box. `/skills` lists them all, to call one, turn
//! one off or keep it for you to call; and says what was left out and
//! why. See [`rein_agent::skills`].

use std::collections::BTreeMap;
use std::path::Path;

use rein_agent::agent::{Agent, Llm};
use rein_agent::skills::{Skill, Skills, Whose};
use serde::{Deserialize, Serialize};

use crate::input::{Menu, Question};
use crate::terminal::Terminal;
use crate::ui::slash::Offer;
use crate::ui::style::GUTTER;
use crate::{CliResult, config_dir};

/// What you chose for your skills, in rein's folder.
const CHOSEN_FILE: &str = "skills.json";

/// The projects whose skills you said yes to, each as they were then.
const TRUSTED_FILE: &str = "skills-trusted.json";

/// How often each skill has been loaded, for the list to keep the ones you
/// use whole.
const USED_FILE: &str = "skills-used.json";

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

/// What picking a skill in `/skills` can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// Put `/name ` in the box, to say what for.
    Use,
    Offer(Offered),
}

/// The skills there are for the project at `root`: yours, and the
/// project's if you've said yes to them as they are, or say so now (when
/// `ask`, which a task run with `-p` isn't). What you chose in `/skills`
/// holds.
pub(crate) async fn find(root: &Path, terminal: &mut Terminal, ask: bool) -> Skills {
    let dir = config_dir().ok();
    let home = std::env::home_dir();
    let fingerprint = rein_agent::skills::fingerprint(root, home.as_deref());
    let project = match &fingerprint {
        Some(fingerprint) => {
            let key = root.display().to_string();
            let trusted = dir.as_deref().map(trusted).unwrap_or_default();
            if trusted.get(&key) == Some(fingerprint) {
                true
            } else if ask && say_yes(root, home.as_deref(), terminal).await {
                if let Some(dir) = dir.as_deref() {
                    let mut trusted = trusted;
                    trusted.insert(key, fingerprint.clone());
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
    let folders = rein_agent::skills::folders(root, home.as_deref(), project);
    let mut skills = rein_agent::skills::find(&folders);
    if !project && fingerprint.is_some() {
        skills.left_out.push((root.join(".agents/skills"), "the project's: you haven't said yes to them as they are now".into()));
    }
    let (chosen, used) = dir.as_deref().map(|dir| (chosen(dir), used(dir))).unwrap_or_default();
    apply(&mut skills, &chosen, &used);
    skills
}

/// Asks whether the model may use the project's skills.
async fn say_yes(root: &Path, home: Option<&Path>, terminal: &mut Terminal) -> bool {
    let theirs = rein_agent::skills::find(&rein_agent::skills::folders(root, home, true).into_iter().filter(|folder| folder.whose == Whose::Project).collect::<Vec<_>>());
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

fn used(dir: &Path) -> BTreeMap<String, u32> {
    rein_llm::store::read_json(&dir.join(USED_FILE)).ok().flatten().unwrap_or_default()
}

/// `skills` as you chose: those off kept apart, offered to no one; those
/// only you call kept from the model; each with how often it's been used.
fn apply(skills: &mut Skills, chosen: &Chosen, used: &BTreeMap<String, u32>) {
    for skill in &mut skills.found {
        skill.used = used.get(&skill.name).copied().unwrap_or(0);
    }
    let (off, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut skills.found).into_iter().partition(|skill| chosen.off.contains(&skill.name));
    skills.off = off;
    skills.found = kept;
    for skill in &mut skills.found {
        if chosen.yours.contains(&skill.name) {
            skill.model = false;
        }
    }
}

/// Counts each skill in `loaded` as used once more.
pub(crate) fn count(loaded: Vec<String>) {
    let Some(dir) = config_dir().ok().filter(|_| !loaded.is_empty()) else { return };
    let mut used = used(&dir);
    for name in loaded {
        let count = used.entry(name).or_default();
        *count = count.saturating_add(1);
    }
    // A count not kept loses nothing that matters.
    let _ = rein_llm::store::write_json_private(&dir.join(USED_FILE), &used);
}

/// rein has a command called `/name`.
fn rein_has(name: &str) -> bool {
    crate::commands::COMMANDS.iter().any(|(command, ..)| command.strip_prefix('/') == Some(name))
}

/// What you call `skill` as the first word of a message.
fn call_as(skill: &Skill) -> String {
    if rein_has(&skill.name) { format!("/skill:{}", skill.name) } else { format!("/{}", skill.name) }
}

/// The skill `word` calls, if it calls one you may: `/skill:name`, or
/// `/name`, which as the `first` word is rein's command if it has one.
fn calls<'a>(word: &str, first: bool, skills: &'a Skills) -> Option<&'a Skill> {
    let explicit = word.strip_prefix("/skill:");
    let name = explicit.or_else(|| word.strip_prefix('/'))?;
    if first && explicit.is_none() && rein_has(name) {
        return None;
    }
    skills.found.iter().find(|skill| skill.name == name && skill.you)
}

/// The skill named by `line`'s first word that isn't yours to call as it
/// is: one you turned off, or one only the model loads.
pub(crate) fn not_callable<'a>(line: &str, skills: &'a Skills) -> Option<&'a Skill> {
    let word = line.split_whitespace().next()?;
    let explicit = word.strip_prefix("/skill:");
    let name = explicit.or_else(|| word.strip_prefix('/'))?;
    if explicit.is_none() && rein_has(name) {
        return None;
    }
    let off = skills.off.iter().filter(|skill| skill.you);
    off.chain(skills.found.iter().filter(|skill| !skill.you)).find(|skill| skill.name == name)
}

/// A message calls `skill`, which isn't yours to call as it is: one
/// turned off is turned on if you say so, and then it's called; of one
/// only the model loads, why. Says whether to send the message.
pub(crate) async fn call_anyway(agent: &mut Agent<Llm>, terminal: &mut Terminal, root: &Path, room: usize, skill: &Skill) -> CliResult<bool> {
    let name = &skill.name;
    if !skill.you {
        let why = format!("✗ /{name} is the model's to load, not yours to call: its SKILL.md says user-invocable: false");
        say!("{}{}", " ".repeat(GUTTER), terminal.err.red(&why));
        return Ok(false);
    }
    let question = Question {
        title: format!("/{name} is turned off. Turn it on, and send this with it?"),
        detail: Some("You turned it off in /skills: the model isn't told of it, and /name doesn't call it.".into()),
        command: None,
        answers: vec![("y", "Yes, turn it on and send".to_owned()), ("n", "No, keep it off".to_owned())],
    };
    if !terminal.yes(question).await {
        return Ok(false);
    }
    offer(agent, terminal, root, room, name, Offered::On).await?;
    Ok(true)
}

/// Offers the skill `name` as `offered` from your next message, as you
/// chose, kept in rein's folder; and says so.
async fn offer(agent: &mut Agent<Llm>, terminal: &mut Terminal, root: &Path, room: usize, name: &str, offered: Offered) -> CliResult {
    let dir = config_dir()?;
    let mut chosen = chosen(&dir);
    chosen.off.retain(|n| n != name);
    chosen.yours.retain(|n| n != name);
    match offered {
        Offered::On => {}
        Offered::YoursToCall => chosen.yours.push(name.to_owned()),
        Offered::Off => chosen.off.push(name.to_owned()),
    }
    rein_llm::store::write_json_private(&dir.join(CHOSEN_FILE), &chosen).map_err(crate::describe)?;
    refresh(agent, terminal, root, room, Look::Chosen).await;
    say!("{}{} /{name} is {} now, from your next message", " ".repeat(GUTTER), terminal.err.green("✓"), said(offered));
    Ok(())
}

/// How `offered` is said.
fn said(offered: Offered) -> &'static str {
    match offered {
        Offered::On => "on",
        Offered::YoursToCall => "only when you call it",
        Offered::Off => "off",
    }
}

/// `line` starts with a skill: it's a message, not a command.
pub(crate) fn starts_with_skill(line: &str, skills: &Skills) -> bool {
    line.split_whitespace().next().is_some_and(|word| calls(word, true, skills).is_some())
}

/// The skills `line` calls, each with what it's given: those it starts
/// with, given what follows them; and a `/name` later in it, given
/// nothing, which loads that skill with the message too.
pub(crate) fn called(line: &str, skills: &Skills) -> Vec<(String, Option<String>)> {
    let mut leading = Vec::new();
    let mut rest = line.trim_start();
    loop {
        let (word, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        match calls(word, leading.is_empty(), skills) {
            Some(skill) if !word.is_empty() => {
                leading.push(skill.name.clone());
                rest = after.trim_start();
            }
            _ => break,
        }
    }
    let given = (!rest.trim().is_empty()).then(|| rest.trim().to_owned());
    // The first word, when it's no skill, is rein's command, or the start
    // of the message: not one later in it.
    let skip = usize::from(leading.is_empty());
    let mut called: Vec<(String, Option<String>)> = leading.into_iter().map(|name| (name, given.clone())).collect();
    for word in rest.split_whitespace().skip(skip) {
        // A sentence may end on one: `then /deploy.`
        let word = if calls(word, false, skills).is_some() { word } else { word.trim_end_matches(['.', ',', ';', '!', '?', ')']) };
        if let Some(skill) = calls(word, false, skills)
            && !called.iter().any(|(name, _)| *name == skill.name)
        {
            called.push((skill.name.clone(), None));
        }
    }
    called
}

/// The skills you may call, as the list under the box shows them.
pub(crate) fn offers(skills: &Skills) -> Vec<Offer> {
    skills
        .found
        .iter()
        .filter(|skill| skill.you)
        .map(|skill| {
            let whose = if skill.whose == Whose::Project { "the project's · " } else { "" };
            let about: String = skill.description.chars().take(120).collect();
            Offer { name: skill.name.clone(), hint: skill.hint.clone().unwrap_or_default(), about: format!("{whose}{about}"), clash: rein_has(&skill.name) }
        })
        .collect()
}

/// What's changed of the skills from `before` to `after`, in a line: those
/// new, changed, and gone. `None` when nothing has.
fn changed(before: &Skills, after: &Skills) -> Option<String> {
    let was = |name: &str| before.found.iter().find(|skill| skill.name == name);
    let new: Vec<&str> = after.found.iter().filter(|skill| was(&skill.name).is_none()).map(|skill| skill.name.as_str()).collect();
    // Used more often since isn't changed.
    let other = |old: &Skill, new: &Skill| Skill { used: new.used, ..old.clone() } != *new;
    let changed: Vec<&str> = after.found.iter().filter(|skill| was(&skill.name).is_some_and(|old| other(old, skill))).map(|skill| skill.name.as_str()).collect();
    let gone: Vec<&str> = before.found.iter().filter(|old| !after.found.iter().any(|skill| skill.name == old.name)).map(|skill| skill.name.as_str()).collect();
    let parts: Vec<String> = [("new", new), ("changed", changed), ("gone", gone)]
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(what, names)| {
            let shown = if names.len() > 6 { format!("{} and {} more", names[..6].join(", "), names.len() - 6) } else { names.join(", ") };
            format!("{what}: {shown}")
        })
        .collect();
    (!parts.is_empty()).then(|| format!("skills · {}", parts.join(" · ")))
}

/// When the skills are looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    /// As rein starts: a project's are asked about if `ask`, which a task
    /// run with `-p` isn't.
    Start { ask: bool },
    /// Before a message: what's changed is said, and a project's skills
    /// changed are asked about again.
    Message,
    /// After you chose in `/skills`, which says what it changed.
    Chosen,
}

/// Looks at the skills again: offered under the box, and what's changed
/// told to the model with your next message; the list is made in `room`
/// characters, when it's made.
pub(crate) async fn refresh(agent: &mut Agent<Llm>, terminal: &mut Terminal, root: &Path, room: usize, look: Look) {
    let ask = match look {
        Look::Start { ask } => ask,
        Look::Message => true,
        Look::Chosen => false,
    };
    let skills = find(root, terminal, ask).await;
    if look == Look::Message
        && let Some(said) = changed(agent.skills(), &skills)
    {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&said));
    }
    terminal.input.set_skills(offers(&skills));
    agent.set_skills(skills, room);
}

/// `/skills`: every skill, where it's from and how it's offered; pick one
/// to call it, or to change how it's offered. What was left out is at the
/// end of the list, to see why.
pub(crate) async fn command(agent: &mut Agent<Llm>, terminal: &mut Terminal, root: &Path, room: usize) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let picked_chosen = chosen(&config_dir()?);
    let all = find(root, terminal, false).await;
    // Those turned off are still yours to turn on; those you use most come
    // first, and so win a tie when you type to narrow the list.
    let mut listed: Vec<&Skill> = all.found.iter().chain(&all.off).collect();
    listed.sort_by_key(|skill| std::cmp::Reverse(skill.used));
    let names: Vec<(String, Whose, String)> = listed.iter().map(|skill| (skill.name.clone(), skill.whose, skill.description.clone())).collect();
    let left_out: Vec<String> = all.left_out.iter().map(|(folder, why)| rein_mcp::clean::text(&format!("{}: {why}", folder.display()))).collect();
    if names.is_empty() && left_out.is_empty() {
        say!("{pad}No skills yet: rein reads them from ~/.agents/skills and ~/.claude/skills, the plugins on in Claude Code, and the project's .agents/skills");
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
    let mut items: Vec<(String, String)> = names
        .iter()
        .map(|(name, whose, description)| {
            let whose = match whose {
                Whose::Project => "the project's · ",
                Whose::Plugin => "a plugin's · ",
                Whose::Yours => "",
            };
            // Whole, to be found by any word in it; the line is cut to fit.
            (format!("/{name}"), format!("{whose}{} · {description}", said(state(name))))
        })
        .collect();
    if !left_out.is_empty() {
        items.push(("left out".into(), format!("{} that rein can't use, and why", left_out.len())));
    }
    let Some(picked) = terminal.input.choose(Menu { title: "skills: pick one to call or change".into(), items, current: None, at: None }).await else { return Ok(()) };
    let Some((name, ..)) = names.get(picked).cloned() else {
        for why in &left_out {
            say!("{pad}{}", s.dim(&format!("– {why}")));
        }
        return Ok(());
    };
    let skill = all.found.iter().find(|skill| skill.name == name);
    let mut choices = vec![Pick::Offer(Offered::On), Pick::Offer(Offered::YoursToCall), Pick::Offer(Offered::Off)];
    if skill.is_some_and(|skill| skill.you) {
        choices.insert(0, Pick::Use);
    }
    let about = |pick: Pick| match pick {
        Pick::Use => "puts it in the box: say what for, and send".to_owned(),
        Pick::Offer(Offered::On) => format!("the model loads it when a task fits, and you call it with /{name}"),
        Pick::Offer(Offered::YoursToCall) => format!("the model isn't told of it: you call it with /{name}"),
        Pick::Offer(Offered::Off) => "left out".to_owned(),
    };
    let label = |pick: Pick| match pick {
        Pick::Use => "use it now".to_owned(),
        Pick::Offer(offered) => said(offered).to_owned(),
    };
    let items = choices.iter().map(|pick| (label(*pick), about(*pick))).collect();
    let current = choices.iter().position(|pick| *pick == Pick::Offer(state(&name)));
    let Some(choice) = terminal.input.choose(Menu { title: format!("/{name}"), items, current, at: None }).await.and_then(|i| choices.get(i).copied()) else {
        return Ok(());
    };
    let offered = match choice {
        Pick::Use => {
            if let Some(skill) = skill {
                terminal.input.insert(&format!("{} ", call_as(skill)));
            }
            return Ok(());
        }
        Pick::Offer(offered) => offered,
    };
    offer(agent, terminal, root, room, &name, offered).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, you: bool) -> Skill {
        Skill { name: name.into(), description: "d".into(), folder: format!("/s/{name}").into(), model: true, you, ..Skill::default() }
    }

    fn skills(names: &[(&str, bool)]) -> Skills {
        Skills { found: names.iter().map(|(name, you)| skill(name, *you)).collect(), ..Skills::default() }
    }

    #[test]
    fn a_line_calls_a_skill_by_its_name_unless_reins_own_command_has_it() {
        let skills = skills(&[("graphify", true), ("search", true), ("quiet", false), ("ckm:design-system", true), ("caveman-commit", true)]);
        let called = |line: &str| called(line, &skills);
        let some = |name: &str, given: Option<&str>| vec![(name.to_owned(), given.map(str::to_owned))];
        assert_eq!(called("/graphify src"), some("graphify", Some("src")));
        assert_eq!(called("/graphify"), some("graphify", None));
        assert!(called("/search off").is_empty() && !starts_with_skill("/search off", &skills), "rein's /search comes first");
        assert_eq!(called("/skill:search the docs"), some("search", Some("the docs")), "named as a skill, it's the skill");
        assert!(called("/quiet").is_empty(), "not one you call");
        assert_eq!(called("/ckm:design-system tokens"), some("ckm:design-system", Some("tokens")));
        assert!(starts_with_skill("/graphify src", &skills));
    }

    #[test]
    fn several_load_together_and_a_later_one_comes_with_the_message() {
        let skills = skills(&[("graphify", true), ("search", true), ("caveman-commit", true), ("quiet", false)]);
        let named = |line: &str| called(line, &skills);
        assert_eq!(
            named("/graphify /caveman-commit 123"),
            [("graphify".to_owned(), Some("123".to_owned())), ("caveman-commit".to_owned(), Some("123".to_owned()))],
            "both, each given what follows, as Claude Code does"
        );
        assert_eq!(named("fix the login, then /caveman-commit."), [("caveman-commit".to_owned(), None)], "anywhere, a sentence's end aside");
        assert_eq!(named("look at /search and /graphify /graphify"), [("search".to_owned(), None), ("graphify".to_owned(), None)], "later on rein has no command; once each");
        assert!(named("look at /usr/bin and /quiet").is_empty(), "a path isn't one, nor one only the model loads");
        assert!(named("look at a/graphify").is_empty(), "it starts the word");
    }

    #[test]
    fn what_you_chose_holds_off_left_out_and_yours_kept_from_the_model() {
        let mut found = skills(&[("a", true), ("b", true), ("c", true)]);
        apply(&mut found, &Chosen { off: vec!["a".into()], yours: vec!["b".into()] }, &BTreeMap::from([("c".to_owned(), 4)]));
        let shown: Vec<(&str, bool, u32)> = found.found.iter().map(|s| (s.name.as_str(), s.model, s.used)).collect();
        assert_eq!(shown, [("b", false, 0), ("c", true, 4)]);
        assert_eq!(found.off.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a"], "kept apart, to turn on");
        assert!(found.left_out.is_empty());
    }

    #[test]
    fn a_skill_turned_off_or_only_the_models_is_known_when_you_call_it() {
        let mut found = skills(&[("graphs", true), ("quiet", false), ("search", true)]);
        found.off = vec![skill("deploy", true), skill("hidden", false), skill("mode", true)];
        let named = |line: &str| not_callable(line, &found).map(|skill| skill.name.as_str());
        assert_eq!(named("/deploy staging"), Some("deploy"));
        assert_eq!(named("/skill:deploy"), Some("deploy"));
        assert_eq!(named("/quiet"), Some("quiet"), "the model's alone");
        assert_eq!(named("/graphs src"), None, "yours to call");
        assert_eq!(named("/hidden"), None, "off and the model's: nothing of yours to turn on");
        assert_eq!(named("look at /deploy"), None, "the first word only");
        assert_eq!(named("/mode trust"), None, "rein's command comes first");
    }

    #[test]
    fn the_list_offers_what_you_call_and_says_what_changed() {
        let mut found = skills(&[("graphify", true), ("search", true), ("quiet", false)]);
        found.found[0].hint = Some("[path]".into());
        found.found[1].whose = Whose::Project;
        let offered = offers(&found);
        assert_eq!(offered.iter().map(|offer| (offer.name.as_str(), offer.clash)).collect::<Vec<_>>(), [("graphify", false), ("search", true)]);
        assert_eq!((offered[0].hint.as_str(), offered[1].about.as_str()), ("[path]", "the project's · d"));
        assert_eq!(changed(&found, &found), None);
        let mut after = skills(&[("graphify", true), ("deploy", true)]);
        after.found[0].description = "new words".into();
        assert_eq!(changed(&found, &after).as_deref(), Some("skills · new: deploy · changed: graphify · gone: search, quiet"));
    }
}
