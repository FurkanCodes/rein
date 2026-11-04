//! `/mcp add <word>`: a server looked up in the official MCP Registry,
//! picked from what it lists with who published each, shown as it would
//! run, and added once you say yes. rein-mcp reads what the registry says
//! (cleaned, the version pinned); this fetches it and asks you.

use std::time::Duration;

use rein_agent::agent::{Agent, Llm};
use rein_agent::sandbox::Access;
use rein_mcp::registry::{self, Found, Start};
use rein_mcp::{Named, Origin};

use super::{Mcp, add};
use crate::CliResult;
use crate::input::{Menu, Question, Read};
use crate::terminal::Terminal;
use crate::ui::style::GUTTER;

/// How long the registry may take to answer.
const WAIT: Duration = Duration::from_secs(20);

/// The most of an answer rein reads: a page of a hundred servers is a few
/// hundred kilobytes.
const LONGEST: usize = 8 << 20;

/// How many of those that don't start here are named.
const NAMED: usize = 5;

/// Looks `word` up in the registry, and adds the server you pick, once
/// you've seen what it runs.
pub(super) async fn find(word: String, mcp: &mut Mcp, agent: &mut Agent<Llm>, terminal: &mut Terminal) -> CliResult {
    let (pad, s) = (" ".repeat(GUTTER), terminal.err);
    if rein_mcp::config::looks_like_a_key(&word) {
        return Err("that looks like a key, not a server's name, so rein doesn't look it up".into());
    }
    let base = std::env::var("REIN_MCP_REGISTRY").unwrap_or_else(|_| registry::URL.to_owned());
    let url = registry::search_url(&base, &word).ok_or_else(|| format!("“{word}” has nothing in it a server's name could have"))?;
    let http = mcp.http.clone();
    let Some(text) = crate::conversation::waiting(terminal, &format!("Looking up {word} in the MCP Registry"), fetch(&http, &url)).await? else { return Ok(()) };
    let mut found = registry::found(&text)?;
    registry::rank(&mut found, &word);
    let (here, elsewhere): (Vec<Found>, Vec<Found>) = found.into_iter().partition(|found| matches!(found.start, Start::Here(_)));
    say_elsewhere(&elsewhere, terminal);
    if here.is_empty() {
        say!("{pad}The MCP Registry has no server called {word} that starts on this computer.");
        say!("{pad}{}", s.dim("/mcp add <name> <command> [args …] adds one as you'd start it, from its README."));
        return Ok(());
    }
    let items = here.iter().map(|found| (found.label(), about(found))).collect();
    let menu = Menu { title: format!("add which {word} server"), items, current: None, at: None };
    let Some(found) = terminal.input.choose(menu).await.and_then(|picked| here.into_iter().nth(picked)) else { return Ok(()) };
    let Start::Here(plan) = &found.start else { return Ok(()) };
    if let Some(command) = plan.command().filter(|command| !on_path(command)) {
        return Err(format!("{} starts with {command}, which isn't installed here: {}", found.label(), installed_with(command)));
    }
    let mut answers = Vec::new();
    for ask in &plan.asks {
        let prompt = format!("{} needs {}:", found.short, ask.about);
        match terminal.input.read(&prompt, false).await {
            Read::Line(answer) if !answer.trim().is_empty() => answers.push(answer.trim().to_owned()),
            _ => {
                say!("{pad}{}", s.dim(&format!("{} isn't added: it needs that to start", found.short)));
                return Ok(());
            }
        }
    }
    let Some(server) = plan.server(&found.short, &answers) else { return Ok(()) };
    if !confirm(&found, &server, mcp, terminal).await {
        return Ok(());
    }
    add(Named { servers: vec![server], ..Named::default() }, mcp, agent, terminal).await
}

/// Asks the registry at `url`, within [`WAIT`], for at most [`LONGEST`].
async fn fetch(http: &rein_llm::reqwest::Client, url: &str) -> CliResult<String> {
    let response = http.get(url).timeout(WAIT).send().await.map_err(|e| format!("can't reach the MCP Registry: {}", crate::with_causes(&e)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("the MCP Registry answered {status}"));
    }
    if response.content_length().is_some_and(|length| length > LONGEST as u64) {
        return Err("the MCP Registry's answer is too long to read".into());
    }
    let body = response.bytes().await.map_err(|e| format!("the MCP Registry's answer didn't come: {}", crate::with_causes(&e)))?;
    if body.len() > LONGEST {
        return Err("the MCP Registry's answer is too long to read".into());
    }
    String::from_utf8(body.to_vec()).map_err(|_| "the MCP Registry sent what isn't text".into())
}

/// What a server found is, in a few words: what it's fetched as, how it
/// runs, and what it says it does.
fn about(found: &Found) -> String {
    let Start::Here(plan) = &found.start else { return found.description.clone() };
    let docker = if plan.command() == Some("docker") { " · in Docker" } else { "" };
    let missing = plan.command().filter(|command| !on_path(command)).map_or_else(String::new, |command| format!(" · needs {command}"));
    format!("{}{docker}{missing} · {}", plan.package, found.description)
}

/// Says which servers found don't start, and why, in a line.
fn say_elsewhere(elsewhere: &[Found], terminal: &Terminal) {
    let Some(first) = elsewhere.first() else { return };
    let mut names: Vec<String> = elsewhere.iter().take(NAMED).map(Found::label).collect();
    if elsewhere.len() > NAMED {
        names.push(format!("{} more", elsewhere.len() - NAMED));
    }
    let why = match &first.start {
        Start::Not(why) => format!(" ({}: {why})", first.label()),
        Start::Here(_) => String::new(),
    };
    say!("{}{}", " ".repeat(GUTTER), terminal.err.dim(&format!("Started in a way rein doesn't know yet: {}{why}", names.join(", "))));
}

/// Asks whether to add `server`, `found` in the registry: what it runs,
/// in which walls, who published it, and whether it takes the place of
/// one of yours. Only yes is yes.
async fn confirm(found: &Found, server: &rein_mcp::Server, mcp: &Mcp, terminal: &mut Terminal) -> bool {
    let yours = mcp.servers.server(&server.name).is_some_and(|theirs| theirs.origin == Origin::Yours) || mcp.off.iter().any(|off| off.name == server.name);
    let instead = if yours { format!(", in place of your {}", server.name) } else { String::new() };
    let mut detail = format!("Published by {}", found.publisher);
    if let Some(repository) = &found.repository {
        detail.push_str(&format!(", its code at {repository}"));
    }
    match server.access() {
        Some(access) => detail.push_str(&format!(". It runs {}", rein_mcp::walls(access))),
        None => detail.push_str(". It's reached at its URL: none of its code runs here"),
    }
    if server.local().is_some_and(|local| local.command == "docker" && local.access == Access::Outside) {
        detail.push_str(": Docker, which the sandbox can't reach, runs it in a container of its own");
    }
    detail.push('.');
    if !server.takes().is_empty() {
        detail.push_str(&format!(" It gets your {}, asked for in dots if you haven't set it.", server.takes().join(", ")));
    }
    let question = Question {
        title: format!("Add {} as {}{instead}?", found.label(), server.name),
        detail: Some(detail),
        command: Some(server.command_line()),
        answers: vec![("y", "Yes, add it".to_owned()), ("n", "No".to_owned())],
    };
    terminal.yes(question).await
}

/// Whether `command` is a program on your `PATH`.
fn on_path(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| runnable(&dir.join(command)))
}

#[cfg(unix)]
fn runnable(file: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(file: &std::path::Path) -> bool {
    file.is_file() || file.with_extension("exe").is_file() || file.with_extension("cmd").is_file()
}

/// What installs `command`.
fn installed_with(command: &str) -> &'static str {
    match command {
        "npx" => "it comes with Node.js, from nodejs.org",
        "uvx" => "it comes with uv, from docs.astral.sh/uv",
        "docker" => "it comes with Docker, from docker.com",
        _ => "install it, then add the server again",
    }
}
