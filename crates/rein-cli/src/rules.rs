//! Your rules, for every project, in `~/.config/rein/permissions`: what
//! asks first, what's refused and what runs without asking, past what the
//! mode says. rein writes the file the first time it starts, with rules
//! that ask before deleting what may not come back; `/permissions` shows
//! them, and the project's, and adds and removes yours. The file stays the
//! record: you can edit it too, and what rein writes keeps what else is in
//! it.

use std::path::{Path, PathBuf};

use rein_agent::agent::{Agent, Llm};
use rein_agent::permissions::{FIRST_RULES, Kind, Rule, Whose, parse_rules};

use crate::commands::Rules;
use crate::input::{Menu, Question, Read};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;
use crate::{CliResult, config_dir};

/// Your rules' file, in rein's folder.
pub(crate) const FILE: &str = "permissions";

/// What the first-time rules are in a few words, for saying so.
const STARTED: &str = "deleting (rm, git clean, git push --force and the like) asks first";

/// Your rules file's text, and whether rein has only now written it, with
/// the rules you start with.
pub(crate) fn load(dir: &Path) -> CliResult<(String, bool)> {
    let path = dir.join(FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok((text, false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write(&path, FIRST_RULES)?;
            Ok((FIRST_RULES.to_owned(), true))
        }
        Err(e) => Err(format!("can't read {}: {e}", shown(&path))),
    }
}

/// What's said the first time, once your rules are written: what rein does
/// now, in `mode`, and where to change it.
pub(crate) fn first_time(trusting: bool) -> String {
    let file = shown(&config_dir().map_or_else(|_| PathBuf::from(FILE), |dir| dir.join(FILE)));
    if trusting {
        format!("rein runs everything without asking, but {STARTED}. /mode changes what runs without asking, /permissions what asks; your rules are in {file}")
    } else {
        format!("Your rules say {STARTED}: /permissions changes them, or {file}")
    }
}

/// `/permissions`: your rules and the project's to pick from, or one of
/// yours added or taken out as typed.
pub(crate) async fn command(command: Rules, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    match command {
        Rules::Show => picker(agent, terminal).await,
        Rules::Add(line) => add(&line, agent, terminal),
        Rules::Remove(line) => {
            let rule = Rule::parse(&line, Whose::Yours)?;
            remove(&rule, agent, terminal)
        }
    }
}

/// Every rule to pick from, "+ add" first: one of yours is taken out once
/// you say so; the project's is only said where it is.
async fn picker(agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let rules = agent.permissions.rules().to_vec();
    let mut items = vec![("+ add".to_owned(), "a rule: allow, ask or deny, then a command".to_owned())];
    items.extend(rules.iter().map(|rule| (rule.to_string(), about(rule).to_owned())));
    let title = "what asks first, past the mode";
    let Some(picked) = terminal.input.choose(Menu { title: title.into(), items, current: None, at: None }).await else { return Ok(()) };
    let Some(rule) = picked.checked_sub(1).and_then(|at| rules.get(at)) else {
        let line = match terminal.input.read("A rule, like ask git push, allow cargo test or deny curl:", false).await {
            Read::Line(line) if !line.trim().is_empty() => line,
            _ => return Ok(()),
        };
        return add(&line, agent, terminal);
    };
    if rule.whose() == Whose::Project {
        say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&format!("`{rule}` is this project's, in .rein/permissions: it comes with the project, so it's changed there")));
        return Ok(());
    }
    let question = Question {
        title: format!("Take `{rule}` out of your rules?"),
        detail: Some(
            match rule.kind() {
                Kind::Ask => "What it names then runs as the mode says, without asking first.",
                Kind::Deny => "What it names is then no longer refused.",
                Kind::Allow => "What it names then asks again, as the mode says.",
            }
            .to_owned(),
        ),
        command: None,
        answers: vec![("y", "Yes, take it out".to_owned()), ("n", "No, keep it".to_owned())],
    };
    if terminal.yes(question).await { remove(rule, agent, terminal) } else { Ok(()) }
}

/// Whose `rule` is, and so how far it reaches, in a few words.
fn about(rule: &Rule) -> &'static str {
    match (rule.whose(), rule.kind()) {
        (Whose::Yours, _) => "yours",
        // Its allow rules count only in the walls: it may be anyone's.
        (Whose::Project, Kind::Allow) => "this project's: only inside the sandbox",
        (Whose::Project, _) => "this project's, in .rein/permissions",
    }
}

/// Adds the rule `line` says to yours, at the end of your file.
fn add(line: &str, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    let line = line.trim();
    let line = line.strip_prefix("/permissions add").map_or(line, str::trim);
    let rule = Rule::parse(line, Whose::Yours)?;
    if agent.permissions.rules().iter().any(|own| own.whose() == Whose::Yours && own.says_the_same(&rule)) {
        return Err(format!("`{rule}` is one of your rules already"));
    }
    let (text, _) = load(&config_dir()?)?;
    let text = format!("{}{}{rule}\n", text, if text.is_empty() || text.ends_with('\n') { "" } else { "\n" });
    keep(&text, agent)?;
    say!("{}{} `{rule}` is one of your rules now, for every project", " ".repeat(GUTTER), terminal.err.green("✓"));
    Ok(())
}

/// Takes `rule` out of yours: every line that says it, and nothing else.
fn remove(rule: &Rule, agent: &mut Agent<Llm>, terminal: &Terminal) -> CliResult {
    let (text, _) = load(&config_dir()?)?;
    let says_it = |line: &str| Rule::parse(line, Whose::Yours).is_ok_and(|own| own.says_the_same(rule));
    let kept: Vec<&str> = text.lines().filter(|line| line.trim_start().starts_with('#') || !says_it(line)).collect();
    if kept.len() == text.lines().count() {
        return Err(format!("`{rule}` isn't one of your rules; /permissions lists them"));
    }
    keep(&(kept.join("\n") + "\n"), agent)?;
    say!("{}{} `{rule}` is out of your rules", " ".repeat(GUTTER), terminal.err.green("✓"));
    Ok(())
}

/// Writes `text` as your rules, once it reads as rules, and decides with
/// them from now on.
fn keep(text: &str, agent: &mut Agent<Llm>) -> CliResult {
    let path = config_dir()?.join(FILE);
    let what = |e: String| format!("{} {e}", shown(&path));
    parse_rules(text, Whose::Yours).map_err(what)?;
    write(&path, text)?;
    agent.permissions.set_yours(text).map_err(what)
}

fn write(path: &Path, text: &str) -> CliResult {
    rein_llm::store::write_private(path, text.as_bytes()).map_err(|e| format!("can't write {}: {}", shown(path), crate::describe(e)))
}

/// `path` with your home folder as `~`.
pub(crate) fn shown(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(inside) => format!("~/{}", inside.display()),
        None => path.display().to_string(),
    }
}
