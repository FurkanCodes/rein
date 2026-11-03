//! The official MCP Registry (registry.modelcontextprotocol.io): servers
//! listed under the name of who published them, a GitHub account or a
//! domain the registry checked. Here is what it says of a server, how rein
//! ranks what a search finds, and the server rein would add for one.
//!
//! What the registry says is its publisher's word: it's cleaned before
//! it's shown, a version is pinned so what you said yes to is what runs,
//! and the command is shown before anything is added. Fetching it is
//! rein's, which has the HTTP client; this only reads what came back.

use rein_sandbox::Access;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Origin, Server};

/// Where the registry is.
pub const URL: &str = "https://registry.modelcontextprotocol.io";

/// The most of a description shown in a list.
const LONGEST_DESCRIPTION: usize = 120;

/// What searches `base` for the servers whose own name, after their
/// publisher's, starts with `word`. The registry matches a search anywhere
/// in the whole name, and every `io.github.…` publisher has "github" in it;
/// a `/` before the word keeps it to the server's own name. `None` if
/// `word` has nothing a name could have.
pub fn search_url(base: &str, word: &str) -> Option<String> {
    let word = searchable(word)?;
    Some(format!("{}/v0.1/servers?search=%2F{word}&limit=100&version=latest", base.trim_end_matches('/')))
}

/// `word` as names are written: lower case, `-` for a space, and only
/// letters, digits, `-`, `_` and `.`, so it goes in a URL as it is.
fn searchable(word: &str) -> Option<String> {
    let word: String = word.trim().to_lowercase().chars().map(|c| if c.is_whitespace() { '-' } else { c }).filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
    (!word.is_empty()).then_some(word)
}

/// A server a search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Its name in the registry: `io.github.microsoft/playwright-mcp`.
    pub name: String,
    /// What rein would call it: `playwright`.
    pub short: String,
    /// Who published it, as you'd find them: `github.com/microsoft`.
    pub publisher: String,
    /// What it says it does, cleaned and cut.
    pub description: String,
    /// Where its code is, as it says.
    pub repository: Option<String>,
    pub start: Start,
    /// The publisher's own name (`microsoft`), and the package's owner and
    /// own name (`playwright`, `mcp`): what ranking goes by.
    owner: String,
    package: Option<(String, String)>,
}

/// How a server found starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// On this computer, as this says.
    Here(Plan),
    /// It's reached at a URL, which rein doesn't do yet.
    AtUrl,
    /// On this computer, but not in a way rein knows: why.
    Not(String),
}

/// How a server found runs here: what rein would write in your mcp.json,
/// but for the settings it needs you to give as it's added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// What it's fetched as, for the list: `npm @playwright/mcp@0.0.41`.
    pub package: String,
    /// What runs it: `npx`, `uvx` or `docker`.
    pub command: String,
    args: Vec<Piece>,
    env: Vec<(String, Piece)>,
    /// Its walls: the sandbox's, but for Docker, which the sandbox can't
    /// reach and which walls in its containers itself.
    pub access: Access,
    /// The settings it needs that aren't keys, in order: a key is a
    /// `${NAME}` of yours instead, asked for in dots as it starts.
    pub asks: Vec<Ask>,
}

/// A setting a server needs you to give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// What it is, in its publisher's words, cleaned.
    pub about: String,
    /// What it is if you give nothing.
    pub default: Option<String>,
}

/// A word of a command, or of a variable's value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Fixed(String),
    /// What you answer to the ask at this place.
    Asked(usize),
}

impl Plan {
    /// The server called `name` this adds, with `answers` to its asks, in
    /// order: `None` unless there's one for each.
    pub fn server(&self, name: &str, answers: &[String]) -> Option<Server> {
        let fill = |piece: &Piece| match piece {
            Piece::Fixed(text) => Some(text.clone()),
            Piece::Asked(i) => answers.get(*i).cloned(),
        };
        Some(Server {
            name: name.to_owned(),
            origin: Origin::Yours,
            command: self.command.clone(),
            args: self.args.iter().map(fill).collect::<Option<_>>()?,
            env: self.env.iter().map(|(name, piece)| Some((name.clone(), fill(piece)?))).collect::<Option<_>>()?,
            access: self.access,
            allow: Vec::new(),
        })
    }
}

impl Found {
    /// What it's shown as in a list: its publisher, then its own name.
    pub fn label(&self) -> String {
        let own = self.name.rsplit_once('/').map_or(self.name.as_str(), |(_, own)| own);
        match self.publisher.strip_prefix("github.com/") {
            Some(account) => format!("{account}/{own}"),
            None => format!("{}/{own}", self.publisher),
        }
    }

    /// How well it matches `word`: its publisher is called that, its own
    /// name is, and so is the package it's fetched as. A copy of a server
    /// by someone else may share its name, but seldom its publisher's or
    /// its package's.
    fn score(&self, word: &str) -> u8 {
        let mut score = 0;
        if self.owner == word {
            score += 4;
        }
        if self.short == word {
            score += 3;
        } else if self.short.starts_with(word) {
            score += 1;
        }
        if let Some((owner, own)) = &self.package {
            if owner == word {
                score += 2;
            } else if bare(own) == word {
                score += 1;
            }
        }
        score
    }
}

/// `found` with those that start here first, those reached at a URL
/// last, and within each what matches `word` best, then the shorter names.
pub fn rank(found: &mut [Found], word: &str) {
    let word = searchable(word).unwrap_or_default();
    let place = |found: &Found| match found.start {
        Start::Here(_) => 0,
        Start::Not(_) => 1,
        Start::AtUrl => 2,
    };
    found.sort_by(|a, b| place(a).cmp(&place(b)).then(b.score(&word).cmp(&a.score(&word))).then(a.short.len().cmp(&b.short.len())).then(a.name.cmp(&b.name)));
}

/// The servers in a page the registry gave: each that's still listed (not
/// deprecated or taken down), once. One rein can't read is left out, not
/// the page.
pub fn found(text: &str) -> Result<Vec<Found>, String> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        servers: Vec<Value>,
    }
    let page: Page = serde_json::from_str(text).map_err(|e| format!("the MCP Registry sent what rein can't read: {e}"))?;
    let mut found: Vec<Found> =
        page.servers.iter().filter(|entry| listed(entry)).filter_map(|entry| serde_json::from_value::<Detail>(entry.get("server")?.clone()).ok()).map(Found::from).collect();
    found.dedup_by(|a, b| a.name == b.name);
    Ok(found)
}

/// Whether the registry still lists `entry`: it says so in its own words.
fn listed(entry: &Value) -> bool {
    let status = entry.pointer("/_meta/io.modelcontextprotocol.registry~1official/status").and_then(Value::as_str);
    status.is_none_or(|status| status == "active")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    name: String,
    #[serde(default)]
    description: String,
    version: Option<String>,
    repository: Option<Repository>,
    #[serde(default)]
    packages: Vec<Value>,
    #[serde(default)]
    remotes: Vec<Value>,
}

#[derive(Deserialize)]
struct Repository {
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Package {
    registry_type: String,
    identifier: String,
    version: Option<String>,
    runtime_hint: Option<String>,
    #[serde(default)]
    runtime_arguments: Vec<Argument>,
    #[serde(default)]
    package_arguments: Vec<Argument>,
    #[serde(default)]
    environment_variables: Vec<Variable>,
    transport: Option<Transport>,
}

#[derive(Deserialize)]
struct Transport {
    #[serde(rename = "type")]
    kind: String,
}

/// A word, or a flag and its value, of a command.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Argument {
    #[serde(rename = "type")]
    kind: ArgumentKind,
    /// A named one's flag, like `--port`.
    name: Option<String>,
    value: Option<String>,
    value_hint: Option<String>,
    default: Option<String>,
    description: Option<String>,
    format: Option<String>,
    #[serde(default)]
    is_required: bool,
    #[serde(default)]
    is_secret: bool,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ArgumentKind {
    Positional,
    Named,
}

/// A variable of a server's environment.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Variable {
    name: String,
    value: Option<String>,
    default: Option<String>,
    description: Option<String>,
    #[serde(default)]
    is_required: bool,
    #[serde(default)]
    is_secret: bool,
}

impl From<Detail> for Found {
    fn from(detail: Detail) -> Found {
        let (publisher, owner) = publisher(&detail.name);
        let packages: Vec<Package> = detail.packages.into_iter().filter_map(|package| serde_json::from_value(package).ok()).collect();
        // npm's first, as most are, then PyPI's, then Docker's.
        let order = |package: &Package| ["npm", "pypi", "oci"].iter().position(|kind| *kind == package.registry_type).unwrap_or(usize::MAX);
        let mut packages: Vec<&Package> = packages.iter().collect();
        packages.sort_by_key(|package| order(package));
        let plans: Vec<Result<Plan, String>> = packages.iter().map(|package| plan(package, detail.version.as_deref())).collect();
        let start = match plans.iter().find_map(|plan| plan.as_ref().ok()) {
            Some(plan) => Start::Here(plan.clone()),
            None if packages.is_empty() && !detail.remotes.is_empty() => Start::AtUrl,
            None => Start::Not(plans.into_iter().find_map(Result::err).unwrap_or_else(|| "it doesn't say how it starts".into())),
        };
        let package = packages.first().map(|package| package_owner(package));
        let short = short(&detail.name);
        let mut description = crate::clean::text(&detail.description);
        if let Some((cut, _)) = description.char_indices().nth(LONGEST_DESCRIPTION) {
            description.truncate(cut);
            description.push('…');
        }
        let repository = detail.repository.map(|repository| crate::clean::text(&repository.url)).filter(|url| !url.is_empty());
        Found { name: crate::clean::text(&detail.name), short, publisher, description, repository, start, owner, package }
    }
}

/// How `package` runs, as `command` and its words: the version pinned, the
/// settings it needs given, and a key it needs a `${NAME}` of yours.
fn plan(package: &Package, version: Option<&str>) -> Result<Plan, String> {
    if let Some(transport) = package.transport.as_ref().filter(|transport| transport.kind != "stdio") {
        return Err(format!("it runs as a server reached over {} on this computer, which rein doesn't start yet", transport.kind));
    }
    let version = package.version.as_deref().or(version).filter(|version| !version.is_empty() && *version != "latest");
    let id = package.identifier.trim();
    let hint = package.runtime_hint.as_deref();
    let (command, first, pinned, access) = match (package.registry_type.as_str(), hint) {
        ("npm", None | Some("npx")) => ("npx", vec!["-y"], version.map_or_else(|| id.to_owned(), |version| format!("{id}@{version}")), Access::Network),
        ("pypi", None | Some("uvx")) => ("uvx", Vec::new(), version.map_or_else(|| id.to_owned(), |version| format!("{id}=={version}")), Access::Network),
        ("oci", None | Some("docker")) => {
            let tagged = id.rsplit_once('/').map_or(id, |(_, last)| last).contains([':', '@']);
            let image = match version {
                Some(version) if !tagged => format!("{id}:{version}"),
                _ => id.to_owned(),
            };
            ("docker", vec!["run", "-i", "--rm"], image, Access::Outside)
        }
        (kind, Some(hint)) if ["npm", "pypi", "oci"].contains(&kind) => return Err(format!("it starts with {}, which rein doesn't know how to run", crate::clean::text(hint))),
        (kind, _) => return Err(format!("it's a {} package, which rein doesn't start yet", crate::clean::text(kind))),
    };
    if id.is_empty() || id.starts_with('-') {
        return Err("its package has no name rein can run".into());
    }
    let mut asks = Vec::new();
    let mut args: Vec<Piece> = first.into_iter().map(|word| Piece::Fixed(word.to_owned())).collect();
    for argument in &package.runtime_arguments {
        args.extend(argument_words(argument, &mut asks));
    }
    let mut env = Vec::new();
    for variable in &package.environment_variables {
        if let Some(value) = variable_value(variable, &mut asks) {
            env.push((variable.name.clone(), value));
        }
    }
    if command == "docker" {
        // A container gets only the variables it's given by name.
        for (name, _) in &env {
            args.extend([Piece::Fixed("-e".into()), Piece::Fixed(name.clone())]);
        }
    }
    args.push(Piece::Fixed(pinned.clone()));
    for argument in &package.package_arguments {
        args.extend(argument_words(argument, &mut asks));
    }
    let shown = match package.registry_type.as_str() {
        "oci" => format!("docker {pinned}"),
        kind => format!("{kind} {pinned}"),
    };
    Ok(Plan { package: crate::clean::text(&shown), command: command.to_owned(), args, env, access, asks })
}

/// The words `argument` adds: none for one it doesn't need, the flag
/// alone for a flag that's on, and a `${NAME}` for a key.
fn argument_words(argument: &Argument, asks: &mut Vec<Ask>) -> Vec<Piece> {
    let flag = argument.name.as_ref().filter(|_| argument.kind == ArgumentKind::Named).map(|name| Piece::Fixed(name.clone()));
    let given = argument.value.clone().or_else(|| argument.default.clone().filter(|_| argument.is_required));
    // A flag that's on or off, as `--headless` is.
    if argument.format.as_deref() == Some("boolean") && flag.is_some() {
        return if given.as_deref() == Some("true") { flag.into_iter().collect() } else { Vec::new() };
    }
    let value = match given {
        Some(value) => Piece::Fixed(value),
        None if !argument.is_required => return Vec::new(),
        None if argument.is_secret => Piece::Fixed(format!("${{{}}}", variable(argument.value_hint.as_deref().or(argument.name.as_deref()).unwrap_or_default()))),
        None => {
            let about = argument.description.as_deref().or(argument.value_hint.as_deref()).or(argument.name.as_deref()).unwrap_or("a setting");
            asks.push(Ask { about: crate::clean::text(about), default: None });
            Piece::Asked(asks.len() - 1)
        }
    };
    flag.into_iter().chain([value]).collect()
}

/// What `variable` is set to: a key is a `${NAME}` of yours, filled in as
/// the server starts; one it doesn't need isn't set.
fn variable_value(variable: &Variable, asks: &mut Vec<Ask>) -> Option<Piece> {
    if let Some(value) = &variable.value {
        return Some(Piece::Fixed(value.clone()));
    }
    if variable.is_secret {
        return variable.is_required.then(|| Piece::Fixed(format!("${{{}}}", variable.name)));
    }
    if !variable.is_required {
        return None;
    }
    if let Some(default) = &variable.default {
        return Some(Piece::Fixed(default.clone()));
    }
    let about = variable.description.as_deref().map_or_else(|| variable.name.clone(), |about| format!("{} ({about})", variable.name));
    asks.push(Ask { about: crate::clean::text(&about), default: None });
    Some(Piece::Asked(asks.len() - 1))
}

/// A variable's name for a key given as an argument: `database_url` is
/// `DATABASE_URL`.
fn variable(hint: &str) -> String {
    let name: String = hint.trim_start_matches('-').chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    let name = name.trim_matches('_');
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) { format!("KEY_{name}") } else { name.to_owned() }
}

/// Who published the server called `name`, as you'd find them, and their
/// own name: `io.github.microsoft` is `github.com/microsoft`, as the
/// registry checked a GitHub account, and `com.microsoft` is
/// `microsoft.com`, as it checked a domain.
fn publisher(name: &str) -> (String, String) {
    let namespace = crate::clean::text(name.split_once('/').map_or(name, |(namespace, _)| namespace));
    if let Some(account) = namespace.strip_prefix("io.github.") {
        return (format!("github.com/{account}"), account.to_lowercase());
    }
    let labels: Vec<&str> = namespace.split('.').collect();
    let owner = labels.get(1).copied().unwrap_or(&namespace).to_lowercase();
    (labels.iter().rev().copied().collect::<Vec<_>>().join("."), owner)
}

/// Who a package is published by on its own registry, and its own name:
/// `@playwright/mcp` is `playwright`'s `mcp`, `ghcr.io/github/x:1` is
/// `github`'s `x`.
fn package_owner(package: &Package) -> (String, String) {
    let id = package.identifier.to_lowercase();
    let id = id.split(['@', ':']).find(|part| !part.is_empty()).map_or_else(String::new, str::to_owned);
    let id = if package.identifier.starts_with('@') { format!("@{id}") } else { id };
    let parts: Vec<&str> = id.trim_start_matches('@').split('/').collect();
    match parts.as_slice() {
        [own] => (String::new(), (*own).to_owned()),
        [.., owner, own] => ((*owner).to_owned(), (*own).to_owned()),
        [] => (String::new(), String::new()),
    }
}

/// A server's own name with what says it's an MCP server taken off:
/// `playwright-mcp` and `mcp-server-git` are `playwright` and `git`.
fn bare(own: &str) -> String {
    let mut name = own.to_lowercase();
    loop {
        let before = name.clone();
        for prefix in ["mcp-server-", "server-mcp-", "mcp-", "server-"] {
            if let Some(rest) = name.strip_prefix(prefix) {
                name = rest.to_owned();
            }
        }
        for suffix in ["-mcp-server", "-server-mcp", "-mcp", "-server", "_mcp"] {
            if let Some(rest) = name.strip_suffix(suffix) {
                name = rest.to_owned();
            }
        }
        if name == before {
            break;
        }
    }
    if name.is_empty() { own.to_lowercase() } else { name }
}

/// What rein would call the server named `name` in the registry: its own
/// name, bare, as a server's name in your file may be.
fn short(name: &str) -> String {
    let own = name.rsplit_once('/').map_or(name, |(_, own)| own);
    let mut short = String::new();
    for c in bare(own).chars() {
        let c = if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' };
        if !(c == '-' && short.ends_with('-')) {
            short.push(c);
        }
    }
    let short = short.trim_matches('-');
    if short.is_empty() { "server".to_owned() } else { short.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A page as the registry gives one, of servers named like "playwright".
    fn page() -> String {
        let active = json!({ "io.modelcontextprotocol.registry/official": { "status": "active", "isLatest": true } });
        json!({ "servers": [
            { "server": { "name": "io.github.someone/playwright-mcp", "description": "A copy of it", "version": "1.0.0",
                          "packages": [{ "registryType": "npm", "identifier": "@someone/playwright-mcp", "version": "1.0.0", "transport": { "type": "stdio" } }] }, "_meta": active },
            { "server": { "name": "io.github.microsoft/playwright-mcp", "description": "Playwright Tools for MCP\u{1b}[2J", "version": "0.0.41",
                          "repository": { "url": "https://github.com/microsoft/playwright-mcp", "source": "github" },
                          "packages": [{ "registryType": "npm", "identifier": "@playwright/mcp", "version": "0.0.41", "transport": { "type": "stdio" } }] }, "_meta": active },
            { "server": { "name": "ai.smithery/playwright", "remotes": [{ "type": "streamable-http", "url": "https://x.example/mcp" }] }, "_meta": active },
            { "server": { "name": "io.github.old/playwright", "packages": [{ "registryType": "npm", "identifier": "old" }] },
              "_meta": { "io.modelcontextprotocol.registry/official": { "status": "deprecated" } } },
            { "server": { "name": "io.github.x/playwright-bundle", "packages": [{ "registryType": "mcpb", "identifier": "https://x/b.mcpb" }] } },
            { "server": "not a server" }
        ], "metadata": { "count": 6 } })
        .to_string()
    }

    fn plan_of(found: &Found) -> &Plan {
        match &found.start {
            Start::Here(plan) => plan,
            other => panic!("{} doesn't start here: {other:?}", found.name),
        }
    }

    #[test]
    fn a_search_finds_the_server_by_its_own_name_and_the_publishers_first() {
        assert_eq!(search_url("https://r.example/", "Playwright").as_deref(), Some("https://r.example/v0.1/servers?search=%2Fplaywright&limit=100&version=latest"));
        assert_eq!(
            search_url(URL, "a&b=c d").as_deref(),
            Some("https://registry.modelcontextprotocol.io/v0.1/servers?search=%2Fabc-d&limit=100&version=latest"),
            "only what a name has"
        );
        assert_eq!(search_url(URL, " &?"), None);
        let mut found = found(&page()).unwrap();
        assert_eq!(found.len(), 4, "the deprecated one and the one rein can't read are left out: {found:?}");
        rank(&mut found, "playwright");
        let names: Vec<&str> = found.iter().map(|found| found.name.as_str()).collect();
        assert_eq!(names[0], "io.github.microsoft/playwright-mcp", "its package is playwright's own: {names:?}");
        assert_eq!(names[1], "io.github.someone/playwright-mcp");
        assert_eq!(found[2].start, Start::Not("it's a mcpb package, which rein doesn't start yet".into()));
        assert_eq!(found[3].start, Start::AtUrl, "reached at a URL: last");
        let microsoft = &found[0];
        assert_eq!((microsoft.short.as_str(), microsoft.publisher.as_str(), microsoft.label()), ("playwright", "github.com/microsoft", "microsoft/playwright-mcp".to_owned()));
        assert_eq!(microsoft.description, "Playwright Tools for MCP[2J", "escape codes taken out");
        assert_eq!(found[3].publisher, "smithery.ai");
        assert_eq!(found[3].label(), "smithery.ai/playwright");
    }

    #[test]
    fn a_package_runs_pinned_and_a_key_is_a_variable_of_yours() {
        let detail =
            |packages: Value| Found::from(serde_json::from_value::<Detail>(json!({ "name": "io.github.acme/thing-mcp", "version": "2.0.0", "packages": packages })).unwrap());
        let npm = detail(json!([{ "registryType": "npm", "identifier": "@acme/thing", "version": "1.2.3",
            "environmentVariables": [
                { "name": "ACME_TOKEN", "isSecret": true, "isRequired": true },
                { "name": "ACME_OPTIONAL_KEY", "isSecret": true },
                { "name": "ACME_REGION", "isRequired": true, "default": "eu" },
                { "name": "ACME_MODE", "value": "fast" },
                { "name": "ACME_VERBOSE" },
                { "name": "ACME_SITE", "isRequired": true, "description": "your site's address" }],
            "packageArguments": [
                { "type": "positional", "value": "mcp" },
                { "type": "named", "name": "--transport", "value": "stdio" },
                { "type": "named", "name": "--headless", "format": "boolean", "value": "true" },
                { "type": "named", "name": "--dev", "format": "boolean" },
                { "type": "named", "name": "--port", "default": "9001" },
                { "type": "positional", "valueHint": "database_url", "isRequired": true, "isSecret": true },
                { "type": "named", "name": "--out", "isRequired": true, "format": "filepath", "description": "where it writes" }] }]));
        let plan = plan_of(&npm);
        assert_eq!((plan.package.as_str(), plan.command.as_str(), plan.access), ("npm @acme/thing@1.2.3", "npx", Access::Network));
        let asks: Vec<&str> = plan.asks.iter().map(|ask| ask.about.as_str()).collect();
        assert_eq!(asks, ["ACME_SITE (your site's address)", "where it writes"], "only what isn't a key and has no value");
        assert_eq!(plan.server("thing", &["one".into()]), None, "an answer for each ask");
        let server = plan.server("thing", &["https://acme.example".into(), "./out".into()]).unwrap();
        assert_eq!(
            server.args,
            ["-y", "@acme/thing@1.2.3", "mcp", "--transport", "stdio", "--headless", "${DATABASE_URL}", "--out", "./out"],
            "an optional --port and --dev left out"
        );
        let env = &server.env;
        assert_eq!(env["ACME_TOKEN"], "${ACME_TOKEN}", "a key is asked for in dots as it starts, and kept out of your mcp.json");
        assert_eq!((env["ACME_REGION"].as_str(), env["ACME_MODE"].as_str(), env["ACME_SITE"].as_str()), ("eu", "fast", "https://acme.example"));
        assert!(!env.contains_key("ACME_VERBOSE") && !env.contains_key("ACME_OPTIONAL_KEY"), "nothing it doesn't need: {env:?}");

        let pypi = detail(
            json!([{ "registryType": "pypi", "identifier": "thing-mcp", "runtimeHint": "uvx", "runtimeArguments": [{ "type": "named", "name": "--with", "value": "extra>=1" }] }]),
        );
        assert_eq!(plan_of(&pypi).server("t", &[]).unwrap().args, ["--with", "extra>=1", "thing-mcp==2.0.0"], "the server's version, when the package doesn't say");
        let docker = detail(
            json!([{ "registryType": "oci", "identifier": "ghcr.io/acme/thing:3.1", "environmentVariables": [{ "name": "ACME_TOKEN", "isSecret": true, "isRequired": true }] }]),
        );
        let docker = plan_of(&docker);
        assert_eq!(docker.access, Access::Outside, "the sandbox can't reach Docker; Docker walls in its container itself");
        assert_eq!(docker.server("t", &[]).unwrap().args, ["run", "-i", "--rm", "-e", "ACME_TOKEN", "ghcr.io/acme/thing:3.1"]);
        let both = detail(json!([{ "registryType": "oci", "identifier": "acme/thing" }, { "registryType": "npm", "identifier": "thing" }]));
        assert_eq!(plan_of(&both).command, "npx", "npm's before Docker's");
        let http = detail(json!([{ "registryType": "npm", "identifier": "thing", "transport": { "type": "streamable-http", "url": "http://localhost:3000" } }]));
        assert!(matches!(&http.start, Start::Not(why) if why.contains("streamable-http")), "{:?}", http.start);
        let bun = detail(json!([{ "registryType": "npm", "identifier": "thing", "runtimeHint": "bunx" }]));
        assert!(matches!(&bun.start, Start::Not(why) if why.contains("bunx")), "{:?}", bun.start);
    }

    #[test]
    fn names_are_short_and_say_who_published_them() {
        for (name, want) in [
            ("io.github.microsoft/playwright-mcp", "playwright"),
            ("io.github.github/github-mcp-server", "github"),
            ("io.github.x/mcp-server-git", "git"),
            ("io.github.upstash/context7", "context7"),
            ("com.example/My_Tool.Thing", "my-tool-thing"),
            ("io.github.x/mcp", "mcp"),
        ] {
            assert_eq!(short(name), want, "{name}");
        }
        assert_eq!(publisher("com.microsoft/x"), ("microsoft.com".into(), "microsoft".into()));
        assert_eq!(publisher("io.github.Microsoft/x"), ("github.com/Microsoft".into(), "microsoft".into()));
        assert_eq!(publisher("app.sub.crawlio/x").0, "crawlio.sub.app");
        assert_eq!(variable("--api-key"), "API_KEY");
        assert_eq!(variable("2fa"), "KEY_2FA");
    }
}
