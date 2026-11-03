//! The reins at a glance: what each is called, which model it runs, what
//! it's doing, what it's editing, and which of them needs you.
//!
//! ```text
//!  rein peers                                        2 working · 1 needs you
//!
//!    NAME     WORKSPACE      MODEL            STATE              DOING
//!  ▸ api      health         gpt-6.1-sol      ⚠ needs you  12s   Allow this command?
//!    web      health         claude-opus-5-5  ● working    2m    Editing src/app.tsx
//!
//!  BEING EDITED   src/routes.rs  api      src/app.tsx  web
//!  LATEST         web → api: add a /health endpoint
//! ```
//!
//! `/peers` shows it once, in the conversation; `rein peers` keeps it up in
//! a tab of its own, drawn again every second, where y and n answer the
//! question the rein picked is waiting on.

use std::path::Path;
use std::time::Duration;

use super::{Body, Info, Reply, State, Status};
use crate::ui::style::{Style, fit, width};

/// Each rein and how it's doing; `None` for one that didn't say.
pub(crate) async fn gather(peers: Vec<Info>) -> Vec<(Info, Option<Status>)> {
    let mut gathered = Vec::with_capacity(peers.len());
    for peer in peers {
        let status = match super::ask(&peer, Body::Status).await {
            Ok(Reply::Status(status)) => Some(status),
            _ => None,
        };
        gathered.push((peer, status));
    }
    gathered
}

/// How long ago `since` was: `12s`, `4m`, `2h`.
fn ago(since: u64) -> String {
    let secs = super::now().saturating_sub(since);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// The table, `columns` wide, with `picked` marked. `me` is this rein,
/// shown first, when it's drawn inside one.
pub(crate) fn lines(reins: &[(Info, Option<Status>)], me: Option<&Status>, picked: Option<usize>, s: Style, columns: usize) -> Vec<String> {
    let all: Vec<(Option<&Info>, Option<&Status>)> = me.map(|me| (None, Some(me))).into_iter().chain(reins.iter().map(|(info, status)| (Some(info), status.as_ref()))).collect();
    let count = |state: State| all.iter().filter(|(_, status)| status.is_some_and(|s| s.state == state)).count();
    let (working, waiting) = (count(State::Working), count(State::Waiting));
    let mut summary = format!("{} rein{}", all.len(), if all.len() == 1 { "" } else { "s" });
    if working > 0 {
        summary += &format!(" · {working} working");
    }
    if waiting > 0 {
        summary += &format!(" · {}", s.yellow(&format!("{waiting} need{} you", if waiting == 1 { "s" } else { "" })));
    }
    let mut out = vec![format!(" {}  {summary}", s.bold("rein peers")), String::new()];
    out.push(s.dim(&fit(&format!("   {:<12} {:<16} {:<18} {:<16} DOING", "NAME", "WORKSPACE", "MODEL", "STATE"), columns)));
    for (i, (info, status)) in all.iter().enumerate() {
        let mark = if picked.is_some() && picked == i.checked_sub(usize::from(me.is_some())) { "▸" } else { " " };
        let name = match (info, status) {
            (None, Some(status)) => format!("{} (you)", status.name),
            (Some(info), _) => info.name.clone(),
            (None, None) => String::new(),
        };
        let row = match status {
            Some(status) => {
                let state = format!("{} {:<9} {}", status.state.mark(), status.state.word(), ago(status.since));
                let state = match status.state {
                    State::Waiting => s.yellow(&format!("{state:<16}")),
                    State::Working => s.green(&format!("{state:<16}")),
                    State::Idle | State::Done => format!("{state:<16}"),
                };
                let doing = status.question.as_ref().map(|q| q.text.clone()).or_else(|| status.doing.clone()).unwrap_or_default();
                format!(" {mark} {:<12} {:<16} {:<18} {state} {doing}", clip(&name, 12), clip(&status.workspace, 16), clip(&status.model, 18))
            }
            None => format!(" {mark} {:<12} {}", clip(&name, 12), s.dim("not answering")),
        };
        out.push(fit(&row, columns));
    }
    let editing: Vec<String> =
        all.iter().filter_map(|(_, status)| *status).flat_map(|status| status.editing.iter().take(3).map(move |path| format!("{path}  {}", s.dim(&status.name)))).collect();
    let recent: Vec<String> = all.iter().filter_map(|(_, status)| *status).flat_map(|status| status.recent.iter().take(2).cloned()).take(4).collect();
    if !editing.is_empty() {
        out.push(String::new());
        out.push(fit(&format!(" {}  {}", s.dim("BEING EDITED"), editing.join("    ")), columns));
    }
    if !recent.is_empty() {
        out.push(String::new());
        out.push(format!(" {}", s.dim("LATEST")));
        out.extend(recent.iter().map(|line| fit(&format!("   {line}"), columns)));
    }
    out
}

/// `text`, at most `most` characters.
fn clip(text: &str, most: usize) -> String {
    if width(text) <= most { text.to_owned() } else { format!("{}…", text.chars().take(most.saturating_sub(1)).collect::<String>()) }
}

/// `rein peers`: the table in a screen of its own, drawn again every
/// second, until q, Esc or Ctrl-C. ↑↓ pick a rein; y and n answer the
/// question it's waiting on.
pub(crate) async fn run(folder: &Path) -> Result<(), String> {
    use std::io::Write;
    use tokio::sync::mpsc;
    let Some(raw) = crate::ui::screen::Raw::enter() else {
        // Not a terminal: the table once, as text.
        let reins = gather(super::list(folder).await).await;
        for line in lines(&reins, None, None, Style::stdout(), 120) {
            println!("{line}");
        }
        return Ok(());
    };
    let (tx, mut keys) = mpsc::unbounded_channel();
    // The keyboard is read on a thread of its own, as the conversation's is.
    std::thread::spawn(move || {
        let mut parser = crate::ui::keys::Keys::default();
        let mut buffer = [0u8; 256];
        let mut stdin = std::io::stdin();
        while let Ok(n @ 1..) = std::io::Read::read(&mut stdin, &mut buffer) {
            for key in parser.feed(&buffer[..n]) {
                if tx.send(key).is_err() {
                    return;
                }
            }
        }
    });
    let s = Style::stderr();
    let mut err = std::io::stderr();
    // Its own screen, put back as it was when it's left.
    let _ = err.write_all(b"\x1b[?1049h\x1b[?25l");
    let mut picked = 0usize;
    let mut note = String::new();
    let mut reins = gather(super::list(folder).await).await;
    loop {
        picked = picked.min(reins.len().saturating_sub(1));
        let (rows, columns) = crate::ui::screen::size();
        let mut screen = lines(&reins, None, (!reins.is_empty()).then_some(picked), s, columns);
        if reins.is_empty() {
            screen.push(String::new());
            screen.push(format!(" {}", s.dim("No rein is running. Start one in another tab, and it shows here.")));
        }
        screen.truncate(rows.saturating_sub(2));
        let keys_line = s.dim(" ↑↓ pick · y/n answer its question · q leave");
        let frame: String = screen.iter().map(|line| format!("{line}\x1b[K\r\n")).collect();
        let _ = write!(err, "\x1b[H{frame}\x1b[J\x1b[{rows};1H{}\x1b[K", fit(&if note.is_empty() { keys_line } else { format!(" {note}") }, columns));
        let _ = err.flush();
        let key = tokio::select! {
            key = keys.recv() => key,
            () = tokio::time::sleep(Duration::from_secs(1)) => {
                reins = gather(super::list(folder).await).await;
                continue;
            }
        };
        use crate::ui::keys::Key;
        match key {
            None | Some(Key::Char('q') | Key::Esc | Key::CtrlC | Key::CtrlD) => break,
            Some(Key::Up) => picked = picked.saturating_sub(1),
            Some(Key::Down) => picked += 1,
            Some(Key::Char(c @ ('y' | 'n'))) => {
                note = answer(reins.get(picked), c == 'y').await;
                reins = gather(super::list(folder).await).await;
            }
            Some(_) => {}
        }
    }
    let _ = err.write_all(b"\x1b[?25h\x1b[?1049l");
    let _ = err.flush();
    drop(raw);
    Ok(())
}

/// Answers the question `rein` is waiting on, and says how that went.
pub(crate) async fn answer(rein: Option<&(Info, Option<Status>)>, yes: bool) -> String {
    let Some((info, status)) = rein else { return "no rein picked".into() };
    let Some(question) = status.as_ref().and_then(|status| status.question.clone()) else { return format!("{} isn't waiting on you", info.name) };
    match super::ask(info, Body::Answer { question: question.id, yes }).await {
        Ok(Reply::Delivered) => format!("✓ told {} {}", info.name, if yes { "yes" } else { "no" }),
        Ok(Reply::Refused { why }) => format!("{}: {why}", info.name),
        Ok(_) => format!("{} answered with something else", info.name),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, state: State) -> Status {
        Status {
            name: name.into(),
            workspace: "health".into(),
            root: "/p".into(),
            model: "gpt-6.1-sol".into(),
            state,
            doing: Some("Running cargo test".into()),
            since: super::super::now(),
            tokens: 0,
            editing: vec!["src/routes.rs".into()],
            question: None,
            recent: vec!["web → api: add a /health endpoint".into()],
        }
    }

    #[test]
    fn the_table_shows_each_rein_and_who_needs_you() {
        let info = Info { pid: 2, name: "api".into(), workspace: "health".into(), root: "/p".into(), socket: "/s".into(), token: "t".into(), tty: None };
        let mut waiting = status("api", State::Waiting);
        waiting.question = Some(super::super::Question { id: 1, text: "Allow this command?".into() });
        let me = status("web", State::Working);
        let shown = lines(&[(info, Some(waiting))], Some(&me), Some(0), Style::plain(), 120).join("\n");
        assert!(shown.contains("2 reins · 1 working · 1 needs you"), "{shown}");
        assert!(shown.contains("web (you)") && shown.contains("● working"), "{shown}");
        assert!(shown.contains("▸ api") && shown.contains("⚠ needs you") && shown.contains("Allow this command?"), "{shown}");
        assert!(shown.contains("BEING EDITED  src/routes.rs  web"), "{shown}");
        assert!(shown.contains("web → api: add a /health endpoint"), "{shown}");
    }
}
