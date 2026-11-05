//! `/memory`: the notes kept across conversations, rein's and Claude
//! Code's, to read one, or forget one of rein's. They're plain files, so
//! each says where it is, to edit as you like.

use rein_agent::agent::{Agent, Llm};
use rein_agent::memory::{Note, Whose};

use crate::CliResult;
use crate::input::{Menu, Question};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// Lists the notes: pick one to read it, and forget it if it's rein's.
pub(crate) async fn command(agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    let Some(memory) = agent.tools.memory().cloned() else {
        say!("{pad}Memory is off: memory = off in your settings, or $REIN_MEMORY");
        return Ok(());
    };
    let notes = memory.all();
    if notes.is_empty() {
        say!("{pad}No notes yet. The model saves one as it learns something later conversations need; they go in {}", crate::rules::shown(memory.folder()));
        return Ok(());
    }
    let items = notes.iter().map(|note| (note.name.clone(), about(note))).collect();
    let menu = Menu { title: "notes kept across conversations: pick one to read".into(), items, current: None, at: None };
    let Some(picked) = terminal.input.choose(menu).await else { return Ok(()) };
    let note = &notes[picked];
    say!("{pad}{} {}", s.bold(&note.name), s.dim(&format!("· {}", about(note))));
    for line in note.text.lines() {
        say!("{pad}{line}");
    }
    say!("{pad}{}", s.dim(&crate::rules::shown(&note.path)));
    if note.whose == Whose::ClaudeCode {
        say!("{pad}{}", s.dim("(Claude Code's: rein reads it, and doesn't change it)"));
        return Ok(());
    }
    let question = Question {
        title: format!("Forget {}?", note.name),
        detail: Some("Later conversations won't know it. The file goes too.".into()),
        command: None,
        answers: vec![("n", "No, keep it".to_owned()), ("y", "Yes, forget it".to_owned())],
    };
    if !terminal.yes(question).await {
        return Ok(());
    }
    let arguments = serde_json::json!({ "action": "forget", "name": note.name, "type": null, "description": null, "text": null }).to_string();
    let call = rein_llm::ToolCall { call_id: "memory_forget".into(), name: "memory".into(), arguments };
    let said = agent.tools.run(&call, rein_agent::sandbox::Run::Unsandboxed).await;
    match said.strip_prefix("error: ") {
        Some(why) => say!("{pad}{}", s.red(&format!("✗ {why}"))),
        None => say!("{pad}{} {} is forgotten", s.green("✓"), note.name),
    }
    Ok(())
}

/// What a note is, in a line: whose, what about, when, and its description.
fn about(note: &Note) -> String {
    let whose = if note.whose == Whose::Rein { "rein's" } else { "Claude Code's" };
    let kind = note.kind.map(|kind| format!(" {}", kind.as_str())).unwrap_or_default();
    let day = note.modified.as_deref().map(|day| format!(", {day}")).unwrap_or_default();
    format!("{whose}{kind}{day} · {}", note.description)
}
