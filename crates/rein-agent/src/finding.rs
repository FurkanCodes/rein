//! The tool search: tools held back from the model until it finds them.
//!
//! A server's seventy tools can take 20,000 tokens of every request, most
//! of them never called. Held back (see `rein_mcp::HOLD_BACK_FROM`), they
//! take a line: the search's own description names each server's tools,
//! and the model finds the few it needs, by words in what they do or by
//! name. Each provider's format then shows what was found its own way,
//! after the conversation so far, which keeps the prompt cache: OpenAI's
//! adds them to the model's tools where they were found (its own tool
//! search isn't taken on a ChatGPT plan), Anthropic's references them as
//! its tool search does, and `claude -p` is told of them from then on.
//! Gemini and Chat Completions can't add a tool later: they're shown all.
//!
//! It searches the tools listed when the conversation's tools were offered,
//! and only those held back: what's shown already needs no finding.

use rein_llm::{Loading, TOOL_SEARCH, Tool};
use serde::Deserialize;
use serde_json::json;

use crate::tools::ERROR;

/// How many tools a search finds when the model doesn't say,
const FOUND: usize = 5;
/// and the most it may ask for.
const MOST: usize = 20;

/// The tool search for the tools held back among `tools`, naming each
/// server's: `None` when nothing is held back.
pub(crate) fn spec(tools: &[Tool]) -> Option<Tool> {
    let mut servers: Vec<(&str, Vec<&str>)> = Vec::new();
    for tool in tools.iter().filter(|tool| tool.loading == Loading::Found) {
        let (server, name) = rein_mcp::parts(&tool.name).unwrap_or(("", tool.name.as_str()));
        match servers.iter_mut().find(|(theirs, _)| *theirs == server) {
            Some((_, names)) => names.push(name),
            None => servers.push((server, vec![name])),
        }
    }
    if servers.is_empty() {
        return None;
    }
    let listed: Vec<String> = servers.iter().map(|(server, names)| format!("- {server}: {}", names.join(", "))).collect();
    Some(Tool {
        name: TOOL_SEARCH.into(),
        description: format!(
            "Find tools of the user's MCP servers that you aren't shown yet, and load them: you can call what it finds from \
             then on, by the names it gives. Search with words from what a tool does, like `create issue`, or name tools \
             exactly: `select:mcp__linear__create_issue,mcp__linear__get_issue`.\n\nThe tools there are, by server, each \
             called mcp__<server>__<tool>:\n{}",
            listed.join("\n")
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Words from what the tools do, or select: and their names, comma-separated." },
                "limit": { "type": ["integer", "null"], "description": format!("The most tools to find: {FOUND} when null, at most {MOST}.") }
            },
            "required": ["query", "limit"],
            "additionalProperties": false
        }),
        strict: true,
        loading: Loading::Search,
    })
}

#[derive(Deserialize)]
struct Search {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// The tools held back among `tools` that the search `arguments` asks for,
/// by name, best first; or what's wrong with the arguments.
pub(crate) fn search(tools: &[Tool], arguments: &str) -> Result<Vec<String>, String> {
    let search: Search = serde_json::from_str(arguments).map_err(|e| format!("{ERROR}bad arguments: {e}"))?;
    let held: Vec<&Tool> = tools.iter().filter(|tool| tool.loading == Loading::Found).collect();
    let query = search.query.trim();
    if let Some(names) = query.strip_prefix("select:") {
        let mut found: Vec<String> = Vec::new();
        for wanted in names.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            if let Some(tool) = held.iter().find(|tool| named(&tool.name, wanted))
                && !found.contains(&tool.name)
            {
                found.push(tool.name.clone());
            }
        }
        found.truncate(MOST);
        return Ok(found);
    }
    let words = words(query);
    let mut scored: Vec<(usize, usize, &Tool)> = held.iter().enumerate().map(|(at, tool)| (score(tool, &words), at, *tool)).filter(|(score, ..)| *score > 0).collect();
    // Best first; as listed where they score the same.
    scored.sort_by_key(|(score, at, _)| (std::cmp::Reverse(*score), *at));
    Ok(scored.into_iter().take(search.limit.unwrap_or(FOUND).clamp(1, MOST)).map(|(.., tool)| tool.name.clone()).collect())
}

/// `name` is what `wanted` names: in full, or without `mcp__`, or its
/// tool's own name.
fn named(name: &str, wanted: &str) -> bool {
    name == wanted || name.strip_prefix(rein_mcp::PREFIX) == Some(wanted) || rein_mcp::parts(name).is_some_and(|(_, tool)| tool == wanted)
}

/// The words in `text`, in lower case: what's between anything that isn't
/// a letter or a digit, so `create_issue` is two.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_lowercase).collect()
}

/// How well `tool` matches `wanted`: most for a word of its name, less
/// for one its description or its parameters' names have. A word matches
/// one that starts with it, so `issue` finds `issues` and `issue_id`.
fn score(tool: &Tool, wanted: &[String]) -> usize {
    let name = words(&tool.name);
    let description = words(&tool.description);
    let parameters: Vec<String> = tool.parameters["properties"].as_object().into_iter().flat_map(|properties| properties.keys()).flat_map(|key| words(key)).collect();
    let has = |words: &[String], word: &str| words.iter().any(|own| own == word || own.starts_with(word));
    wanted
        .iter()
        .map(|word| {
            if has(&name, word) {
                10
            } else if has(&description, word) {
                3
            } else if has(&parameters, word) {
                1
            } else {
                0
            }
        })
        .sum()
}

/// What a search found, for the step on screen: `found linear:
/// create_issue, list_issues`.
pub(crate) fn said(names: &[String]) -> String {
    if names.is_empty() {
        return "nothing found".to_owned();
    }
    let mut servers: Vec<(&str, Vec<&str>)> = Vec::new();
    for name in names {
        let (server, tool) = rein_mcp::parts(name).unwrap_or(("", name.as_str()));
        match servers.iter_mut().find(|(theirs, _)| *theirs == server) {
            Some((_, tools)) => tools.push(tool),
            None => servers.push((server, vec![tool])),
        }
    }
    let servers: Vec<String> = servers.iter().map(|(server, tools)| format!("{server}: {}", tools.join(", "))).collect();
    format!("found {}", servers.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, loading: Loading) -> Tool {
        Tool { name: name.into(), description: description.into(), parameters: json!({"type": "object", "properties": {"team_id": {"type": "string"}}}), strict: false, loading }
    }

    fn tools() -> Vec<Tool> {
        vec![
            tool("read", "Read a file.", Loading::UpFront),
            tool("mcp__linear__list_issues", "List issues in a team.", Loading::Found),
            tool("mcp__linear__create_issue", "Create a new issue.", Loading::Found),
            tool("mcp__linear__list_projects", "List projects.", Loading::Found),
            tool("mcp__linear__get_team", "A team, with how many issues it has.", Loading::Found),
            tool("mcp__node__eval", "Run JavaScript.", Loading::UpFront),
        ]
    }

    #[test]
    fn the_search_names_what_each_server_holds_back() {
        let spec = spec(&tools()).unwrap();
        assert_eq!(spec.loading, Loading::Search);
        assert!(spec.description.ends_with("- linear: list_issues, create_issue, list_projects, get_team"), "{}", spec.description);
        assert!(!spec.description.contains("eval"), "what's shown needs no finding");
        let shown: Vec<Tool> = tools().into_iter().filter(|tool| tool.loading == Loading::UpFront).collect();
        assert_eq!(super::spec(&shown), None, "nothing held back: no search");
    }

    #[test]
    fn words_find_the_tools_that_do_it_best_first() {
        let found = |query: &str| search(&tools(), &json!({"query": query, "limit": null}).to_string()).unwrap();
        assert_eq!(found("create issue"), ["mcp__linear__create_issue", "mcp__linear__list_issues", "mcp__linear__get_team"]);
        assert_eq!(found("Projects"), ["mcp__linear__list_projects"], "any case, and the plural");
        assert_eq!(found("issue"), ["mcp__linear__list_issues", "mcp__linear__create_issue", "mcp__linear__get_team"], "its name first, then its description");
        assert_eq!(found("team"), ["mcp__linear__get_team", "mcp__linear__list_issues", "mcp__linear__create_issue", "mcp__linear__list_projects"], "then its parameters");
        assert_eq!(found("read file"), Vec::<String>::new(), "only what's held back");
        assert_eq!(found("javascript"), Vec::<String>::new());
        let one = search(&tools(), r#"{"query":"issue","limit":1}"#).unwrap();
        assert_eq!(one, ["mcp__linear__list_issues"]);
    }

    #[test]
    fn select_finds_tools_by_name_however_theyre_named() {
        let found = |query: &str| search(&tools(), &json!({"query": query, "limit": null}).to_string()).unwrap();
        assert_eq!(
            found("select:mcp__linear__create_issue, linear__list_projects,list_issues"),
            ["mcp__linear__create_issue", "mcp__linear__list_projects", "mcp__linear__list_issues"]
        );
        assert_eq!(found("select:create_issue,create_issue,mcp__node__eval,nope"), ["mcp__linear__create_issue"], "once each, and only what's held back");
        assert!(search(&tools(), r#"{"limit":3}"#).unwrap_err().starts_with(ERROR), "no query");
    }

    #[test]
    fn what_was_found_is_said_by_server() {
        assert_eq!(said(&["mcp__linear__create_issue".into(), "mcp__linear__list_issues".into(), "mcp__gh__get_pr".into()]), "found linear: create_issue, list_issues; gh: get_pr");
        assert_eq!(said(&[]), "nothing found");
    }
}
