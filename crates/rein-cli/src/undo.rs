//! `/undo`: the turns there are to undo, the latest first, and what
//! undoing back to before one does, asked before it's done.

use std::time::{Duration, SystemTime};

use rein_agent::agent::{Agent, Llm};
use rein_agent::undo::{Became, Listed, Plan, Span, Unshown};

use crate::format::files;
use crate::input::{Menu, Question};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;
use crate::{CliResult, describe};

/// Undoes what rein did since a turn you pick, once you say so: right away
/// to the question when there's only one.
pub(crate) async fn command(agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let pad = " ".repeat(GUTTER);
    let listed = agent.undo_list().await;
    let picked = match listed.as_slice() {
        [] => {
            say!("{pad}nothing to undo: rein hasn't changed any files in this conversation");
            return Ok(());
        }
        [only] => only,
        _ => {
            let items = listed.iter().map(|turn| (said(turn), about(turn))).collect();
            let menu = Menu { title: "undo what rein did since".into(), items, current: None, at: None };
            let Some(at) = terminal.input.choose(menu).await else { return Ok(()) };
            &listed[at]
        }
    };
    let plan = agent.undo_plan(picked.turn).await;
    if !plan.does_anything() {
        say!("{pad}nothing to put back: {}", why_nothing(&plan));
        return Ok(());
    }
    let question = Question { title: title(&plan), detail: Some(what_it_does(&plan)), command: None, answers: vec![("y", "Yes, undo it".to_owned()), ("n", "No".to_owned())] };
    if !terminal.yes(question).await {
        return Ok(());
    }
    let done = agent.undo(picked.turn).await.map_err(describe)?;
    say_done(&done, terminal);
    Ok(())
}

/// `/changes`: what the conversation changed, file by file, as `/undo`
/// would see it.
pub(crate) async fn changes(agent: &Agent<Llm>, terminal: &Terminal) {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let changes = agent.changes(Span::Conversation, false).await;
    if changes.files.is_empty() {
        say!("{pad}nothing's changed in this conversation");
        return;
    }
    let widest = changes.files.iter().map(|file| file.path.chars().count()).max().unwrap_or(0);
    for file in &changes.files {
        let room = " ".repeat(widest.saturating_sub(file.path.chars().count()));
        let lines = match file.unshown {
            Some(Unshown::Binary) => s.dim("not text"),
            Some(Unshown::TooBig) => s.dim("too long to show"),
            Some(Unshown::NotKept) => s.dim("no copy of how it was"),
            None => format!("{} {}", s.green(&format!("+{}", file.added)), s.red(&format!("−{}", file.removed))),
        };
        let word = match (file.since, file.became) {
            (true, _) => "changed since",
            (false, Became::Made) => "made",
            (false, Became::Removed) => "removed",
            (false, Became::Changed) => "",
        };
        say!("{pad}{}{room}  {lines}  {}", file.path, s.dim(word));
    }
    let (files, added, removed) = changes.totals();
    let total = format!("{}, +{added} −{removed}", if files == 1 { "1 file".to_owned() } else { format!("{files} files") });
    say!("{pad}{}", s.dim(&total));
}

/// A turn as the list has it: what started it.
fn said(turn: &Listed) -> String {
    if turn.said.is_empty() { "(a turn that started with nothing said)".to_owned() } else { format!("“{}”", turn.said) }
}

/// When it was, and what it did, in a few words.
fn about(turn: &Listed) -> String {
    let counts = [(turn.changed, "changed"), (turn.made, "made"), (turn.removed, "removed")];
    let counts: Vec<String> = counts.iter().filter(|(n, _)| *n > 0).map(|(n, what)| format!("{n} {what}")).collect();
    format!("{} · {}", ago(turn.at), counts.join(", "))
}

/// How long ago `at`, in seconds since 1970, was.
fn ago(at: u64) -> String {
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(at);
    let minutes = SystemTime::now().duration_since(then).map_or(0, |since| since.as_secs() / 60);
    match minutes {
        0 => "just now".to_owned(),
        1..60 => format!("{minutes} min ago"),
        60..1440 => format!("{} h ago", minutes / 60),
        _ => crate::format::local_time(then).date(),
    }
}

fn title(plan: &Plan) -> String {
    match plan.turns {
        1 => format!("Undo what rein did after “{}”?", plan.said),
        turns => format!("Undo rein's last {turns} turns, back to before “{}”?", plan.said),
    }
}

/// What undoing does to the files, before it's done.
fn what_it_does(plan: &Plan) -> String {
    let mut parts = Vec::new();
    if !plan.put_back.is_empty() {
        parts.push(format!("puts back {}", files(&plan.put_back)));
    }
    if !plan.take_away.is_empty() {
        parts.push(format!("takes away {}, which rein made", files(&plan.take_away)));
    }
    if !plan.left.is_empty() {
        parts.push(format!("leaves {} as it is: changed since", files(&plan.left)));
    }
    if !plan.lost.is_empty() {
        parts.push(format!("can't put back {}: too big to keep", files(&plan.lost)));
    }
    let mut said = parts.join(" · ");
    if let Some(first) = said.get(..1) {
        said = first.to_uppercase() + &said[1..];
    }
    said + ". What commands did outside the project, or to anything but its files, stays."
}

/// Why undoing would put nothing back.
fn why_nothing(plan: &Plan) -> String {
    match (plan.left.is_empty(), plan.lost.is_empty()) {
        (false, _) => format!("{} changed since rein did, so it's left as it is", files(&plan.left)),
        (true, false) => format!("{} was too big to keep", files(&plan.lost)),
        (true, true) => "what rein did then is gone already".to_owned(),
    }
}

pub(crate) fn say_done(done: &Plan, terminal: &Terminal) {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let mut did = Vec::new();
    if !done.put_back.is_empty() {
        did.push(format!("put back {}", files(&done.put_back)));
    }
    if !done.take_away.is_empty() {
        did.push(format!("took away {}", files(&done.take_away)));
    }
    if !did.is_empty() {
        say!("{pad}{} {}", s.green("✓"), did.join("; "));
    }
    if !done.left.is_empty() {
        say!("{pad}{}", s.dim(&format!("left {} as it is: it changed since rein did", files(&done.left))));
    }
    if !done.lost.is_empty() {
        say!("{pad}{}", s.dim(&format!("couldn't put back {}: it was too big to keep", files(&done.lost))));
    }
    for (path, why) in &done.failed {
        say!("{pad}{}", s.red(&format!("✗ couldn't put back {path}: {why}")));
    }
    say!("{pad}{}", s.dim("The model has been told."));
}
