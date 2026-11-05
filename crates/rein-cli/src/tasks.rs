//! `/tasks`: the commands in the background, running and lately ended,
//! to stop one. Its output is a Ctrl-O away; the model hears it was you.

use rein_agent::agent::{Agent, Llm};
use rein_agent::background::{Listed, plain};

use crate::CliResult;
use crate::input::{Menu, Question};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// Lists the background commands: pick a running one to stop it.
pub(crate) async fn command(agent: &Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let jobs = agent.tools.jobs().clone();
    let listed = jobs.list();
    if listed.is_empty() {
        say!(
            "{pad}Nothing runs in the background. The model starts a server or a watcher there, and a command still running at its timeout goes there; so does one you press Ctrl-B on, with nothing typed."
        );
        return Ok(());
    }
    let items = listed.iter().map(|job| (format!("{}  {}", job.id, first_line(&job.command)), about(job))).collect();
    let menu = Menu { title: "background commands: pick one to stop it · ctrl-o shows what each printed".into(), items, current: None, at: None };
    let Some(picked) = terminal.input.choose(menu).await else { return Ok(()) };
    let job = &listed[picked];
    if job.ended.is_some() {
        say!("{pad}{} {} has ended: {}", s.dim("·"), job.id, about(job));
        return Ok(());
    }
    let question = Question {
        title: format!("Stop {}?", job.id),
        detail: Some(format!("{}: it's asked to stop, and everything it started with it; killed if it hasn't in 2 seconds.", first_line(&job.command))),
        command: None,
        answers: vec![("y", "Yes, stop it".to_owned()), ("n", "No, leave it running".to_owned())],
    };
    if !terminal.yes(question).await {
        return Ok(());
    }
    match jobs.stop_yours(&job.id).await {
        Ok(()) => say!("{pad}{} {} is stopped; the model hears of it with your next message", s.green("✓"), job.id),
        Err(why) => say!("{pad}{}", s.red(&format!("✗ {why}"))),
    }
    terminal.input.set_bar(terminal.bar());
    Ok(())
}

fn first_line(command: &str) -> String {
    plain(command.lines().next().unwrap_or_default())
}

/// How a background command is doing, in a line: how long, and what it
/// printed last.
fn about(job: &Listed) -> String {
    let secs = job.took.as_secs();
    let took = if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) };
    let state = match job.ended {
        Some(exit) => format!("{} after {took}", exit.said()),
        None => format!("running for {took}"),
    };
    match &job.last {
        Some(last) => format!("{state} · {last}"),
        None => state,
    }
}
