//! Which MCP servers to start, as an `mcpServers` file says. It's the shape
//! Claude Code, Cursor and others read, so the snippet in a server's README
//! works as it is:
//!
//! ```json
//! { "mcpServers": {
//!     "github": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-github"],
//!       "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" },
//!       "access": "network",
//!       "allow": ["get_issue", "search_issues"]
//! } } }
//! ```
//!
//! `access` and `allow` are rein's own. `access` is the walls the server
//! runs inside, as a command's are: `network` when not said, and `outside`
//! for one that needs your whole home folder. `allow` names tools that run
//! without asking, or `*` for all of one server's; it counts only in your
//! own file, never in a project's. `"disabled": true`, as Cline writes it,
//! keeps a server in the file without starting it.

use std::collections::BTreeMap;

use rein_sandbox::Access;
use serde::Deserialize;
use serde_json::{Value, json};

/// Where a server was named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Your own file, in rein's folder: you wrote it.
    Yours,
    /// The project's `.mcp.json`, which came with the project and may be
    /// anyone's: started only once you've said yes to it as it is.
    Project,
}

impl Origin {
    /// Where it was named, in a few words: for `/mcp`.
    pub const fn describe(self) -> &'static str {
        match self {
            Origin::Yours => "yours",
            Origin::Project => "the project's",
        }
    }
}

/// A server to start: a command on this computer that rein talks to over
/// its standard input and output.
#[derive(Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub origin: Origin,
    pub command: String,
    pub args: Vec<String>,
    /// As written: `${NAME}` stands for a variable of yours, filled in
    /// only as the server starts.
    pub env: BTreeMap<String, String>,
    /// The walls it runs inside.
    pub access: Access,
    /// Its tools that run without asking: their names, or `*` for all.
    pub allow: Vec<String>,
}

/// Never what `env` says, or an argument that may be a key: a file of
/// yours may have one in it as it is.
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args: Vec<&str> = self.args.iter().zip(self.keyed()).map(|(arg, keyed)| if keyed { "…" } else { arg.as_str() }).collect();
        f.debug_struct("Server")
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("command", &self.command)
            .field("args", &args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("access", &self.access)
            .field("allow", &self.allow)
            .finish()
    }
}

/// A server a file names that rein won't start, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub origin: Origin,
    pub why: String,
}

/// What files name: the servers to start, those turned off, and those
/// rein can't start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Named {
    pub servers: Vec<Server>,
    /// Your servers you turned off (`"disabled": true`): kept, not started.
    pub off: Vec<Server>,
    pub skipped: Vec<Skipped>,
}

impl Named {
    /// Both files' servers. One of the project's with the name of one of
    /// yours, on or off, is left out: yours is the one you meant.
    pub fn and(mut self, project: Named) -> Named {
        for server in project.servers {
            if self.servers.iter().chain(&self.off).any(|yours| yours.name == server.name) {
                self.skipped.push(Skipped { name: server.name, origin: server.origin, why: "your own mcp.json has a server of that name".into() });
            } else {
                self.servers.push(server);
            }
        }
        self.skipped.extend(project.skipped);
        self
    }
}

#[derive(Deserialize)]
struct File {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, Value>,
}

/// One server as a file has it.
#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<Kind>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    access: Option<Access>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    disabled: bool,
}

/// How a server is reached. rein starts commands; the others are a URL.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Stdio,
    Http,
    Sse,
}

/// The servers in a file's `text`, named `origin`. A server rein can't
/// start is skipped, with why; a file that isn't JSON of this shape is an
/// error.
pub fn read(text: &str, origin: Origin) -> Result<Named, String> {
    let file: File = serde_json::from_str(text).map_err(|e| format!("isn't an mcpServers file: {e}"))?;
    Ok(servers(file.servers, origin))
}

/// The servers `/mcp add` was given, as yours. Either the snippet a
/// server's README gives, as it is (`{"mcpServers": {…}}`, or only
/// `"github": {…}`, or a name and then `{"command": …}`), or a server as
/// you'd type it in a shell, its name first:
///
/// `github GITHUB_PERSONAL_ACCESS_TOKEN=$GITHUB_TOKEN npx -y @modelcontextprotocol/server-github`
///
/// `--access outside` before the command gives it other walls. `-e
/// NAME=value` and a `--` before the command work too, as Claude Code's
/// and Codex's `mcp add` take them.
pub fn added(text: &str) -> Result<Named, String> {
    let text = text.trim();
    if text.starts_with('{') || text.starts_with('"') {
        return snippet(text);
    }
    if let Some((name, entry)) = text.split_once(char::is_whitespace)
        && entry.trim_start().starts_with('{')
    {
        let entry: Value = serde_json::from_str(entry.trim().trim_end_matches(',')).map_err(|e| format!("that isn't JSON as a server's entry: {e}"))?;
        return Ok(servers(BTreeMap::from([(name.to_owned(), entry)]), Origin::Yours));
    }
    typed(text)
}

/// The servers in a snippet from a server's README: a whole file, as
/// Claude Code's, Cursor's (`mcpServers`) or VS Code's (`servers`) are, or
/// only the servers by name, with or without the braces around them.
fn snippet(text: &str) -> Result<Named, String> {
    let text = text.trim_end_matches(',');
    let text = if text.starts_with('"') { format!("{{{text}}}") } else { text.to_owned() };
    let Ok(Value::Object(mut object)) = serde_json::from_str(&text) else {
        let e = serde_json::from_str::<Value>(&text).err().map_or_else(String::new, |e| format!(": {e}"));
        return Err(format!("that isn't JSON as a server's README gives it{e}"));
    };
    let servers = match object.remove("mcpServers").or_else(|| object.remove("servers")) {
        Some(Value::Object(servers)) => servers,
        Some(_) => return Err("its mcpServers isn't servers by name".into()),
        None if object.contains_key("command") || object.contains_key("url") => return Err("say what to call it first: /mcp add <name> {…}".into()),
        None => object,
    };
    Ok(self::servers(servers.into_iter().collect(), Origin::Yours))
}

/// A server as you'd type it in a shell: its name, then its environment
/// (`NAME=value`), its command and the command's arguments, with rein's
/// own options before the command.
fn typed(text: &str) -> Result<Named, String> {
    const USAGE: &str = "/mcp add takes a name, then the command that starts the server: /mcp add github npx -y @modelcontextprotocol/server-github";
    let mut words = words(text)?.into_iter();
    let (mut name, mut access, mut env) = (None, None, BTreeMap::new());
    let command = loop {
        let Some(word) = words.next() else { return Err(USAGE.into()) };
        match word.as_str() {
            "--access" => access = Some(words.next().ok_or("--access takes look, write, network or outside")?),
            _ if word.starts_with("--access=") => access = word.strip_prefix("--access=").map(str::to_owned),
            "-e" | "--env" => {
                let given = words.next().unwrap_or_default();
                let (key, value) = setting(&given).ok_or_else(|| format!("{word} takes NAME=value"))?;
                env.insert(key, value);
            }
            "--" => break words.next().ok_or(USAGE)?,
            _ if word.starts_with('-') => return Err(format!("/mcp add doesn't know {word}: before the command it takes --access and -e NAME=value")),
            _ if name.is_none() => name = Some(word),
            _ => match setting(&word) {
                Some((key, value)) => {
                    env.insert(key, value);
                }
                None => break word,
            },
        }
    };
    let name = name.ok_or(USAGE)?;
    let entry = json!({ "command": command, "args": words.collect::<Vec<_>>(), "env": env, "access": access });
    Ok(servers(BTreeMap::from([(name, entry)]), Origin::Yours))
}

/// Whether `word`, typed alone, may be a key rather than a name: then it's
/// never sent anywhere to look up. It starts as providers' keys do, or is
/// long and of letters and digits, as no server's name is, or is a URL with
/// a password in it.
pub fn looks_like_a_key(word: &str) -> bool {
    const STARTS: [&str; 13] = ["sk-", "sk_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "AKIA", "AIza"];
    let long = word.len() >= 32 && word.chars().any(|c| c.is_ascii_digit()) && word.chars().any(|c| c.is_ascii_alphabetic());
    STARTS.iter().any(|start| word.starts_with(start)) || long || rein_sandbox::looks_secret("", word)
}

/// `NAME=value`, as a shell sets a variable for a command.
fn setting(word: &str) -> Option<(String, String)> {
    let (name, value) = word.split_once('=')?;
    let fits = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    fits.then(|| (name.to_owned(), value.to_owned()))
}

/// `text` in words, as a shell splits it: at spaces, but not in quotes or
/// after a `\`. A shell's `$NAME` is rein's `${NAME}`, filled in as the
/// server starts, as a shell would as it ran; in single quotes it's left.
fn words(text: &str) -> Result<Vec<String>, String> {
    let (mut words, mut word): (Vec<String>, Option<String>) = (Vec::new(), None);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            words.extend(word.take());
            continue;
        }
        let word = word.get_or_insert_default();
        match c {
            '\'' => loop {
                match chars.next() {
                    Some('\'') => break,
                    Some(c) => word.push(c),
                    None => return Err("a ' isn't closed".into()),
                }
            },
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                        Some(c) => word.extend(['\\', c]),
                        None => return Err("a \" isn't closed".into()),
                    },
                    Some('$') => variable(&mut chars, word),
                    Some(c) => word.push(c),
                    None => return Err("a \" isn't closed".into()),
                }
            },
            '\\' => word.extend(chars.next()),
            '$' => variable(&mut chars, word),
            c => word.push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

/// What follows a `$`: a name, which becomes `${NAME}`. Anything else,
/// `{` included, follows the `$` as it is.
fn variable(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, word: &mut String) {
    word.push('$');
    if chars.peek().is_some_and(|c| c.is_ascii_alphabetic() || *c == '_') {
        word.push('{');
        while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
            word.push(c);
        }
        word.push('}');
    }
}

/// The servers `entries` name, by name. One rein can't start is skipped,
/// with why.
pub(crate) fn servers(entries: BTreeMap<String, Value>, origin: Origin) -> Named {
    let mut named = Named::default();
    for (name, entry) in entries {
        let skip = |why: String| Skipped { name: name.clone(), origin, why };
        if let Err(why) = check_name(&name) {
            named.skipped.push(skip(why));
            continue;
        }
        let entry: Entry = match serde_json::from_value(entry) {
            Ok(entry) => entry,
            Err(e) => {
                named.skipped.push(skip(format!("can't be read: {e}")));
                continue;
            }
        };
        let remote = entry.url.is_some() || matches!(entry.kind, Some(Kind::Http | Kind::Sse));
        let command = match entry.command {
            _ if remote => {
                named.skipped.push(skip("it's reached at a URL, and rein only starts servers on this computer so far".into()));
                continue;
            }
            None => {
                named.skipped.push(skip("it has no command to start".into()));
                continue;
            }
            Some(command) => command,
        };
        let server = Server {
            name,
            origin,
            command,
            args: entry.args,
            env: entry.env,
            access: entry.access.unwrap_or(Access::Network),
            // A project doesn't decide what runs without asking you.
            allow: if origin == Origin::Yours { entry.allow } else { Vec::new() },
        };
        match (entry.disabled, origin) {
            (false, _) => named.servers.push(server),
            (true, Origin::Yours) => named.off.push(server),
            (true, Origin::Project) => named.skipped.push(Skipped { name: server.name, origin, why: "its .mcp.json turns it off".into() }),
        }
    }
    named
}

/// A server's name goes into its tools' names (`mcp__github__get_issue`),
/// so it's what a tool's name may hold, without the `__` that ends it.
fn check_name(name: &str) -> Result<(), String> {
    let fits = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if fits && !name.contains("__") && !name.starts_with('_') && !name.ends_with('_') {
        Ok(())
    } else {
        Err("a server's name may have letters, digits, - and _, but not two _ in a row".into())
    }
}

impl Server {
    /// What you say yes to for a project's server: all of what it is, so
    /// that a change to any of it, another command, an argument or a
    /// variable, asks again.
    pub fn fingerprint(&self) -> String {
        json!({ "name": self.name, "command": self.command, "args": self.args, "env": self.env, "access": self.access.as_str() }).to_string()
    }

    /// As your file has it: what [`read`] reads back as this server.
    pub fn entry(&self) -> Value {
        let mut entry = serde_json::Map::new();
        entry.insert("command".into(), json!(self.command));
        if !self.args.is_empty() {
            entry.insert("args".into(), json!(self.args));
        }
        if !self.env.is_empty() {
            entry.insert("env".into(), json!(self.env));
        }
        if self.access != Access::Network {
            entry.insert("access".into(), json!(self.access.as_str()));
        }
        if !self.allow.is_empty() {
            entry.insert("allow".into(), json!(self.allow));
        }
        Value::Object(entry)
    }

    /// Whether what it runs, as it was typed, may hold a key: a value
    /// written into its environment as it is, not as a `${NAME}`, or an
    /// argument named like one (`--api-key sk-…`, `--token=…`). The line
    /// it was typed in is then never shown again, or kept.
    pub fn may_hold_keys(&self) -> bool {
        self.env.values().any(|value| written(value)) || self.keyed().any(|keyed| keyed)
    }

    /// For each argument, whether it may be a key, written in as it is:
    /// after a flag named like one, as `--api-key sk-…`, or one itself,
    /// as `--token=…`, or a URL with a password in it.
    fn keyed(&self) -> impl Iterator<Item = bool> + '_ {
        // A flag that takes the word after it: `--api-key`, not `--token=…`.
        fn flag(word: &str) -> Option<&str> {
            word.strip_prefix('-').map(|word| word.trim_start_matches('-')).filter(|name| !name.contains('='))
        }
        self.args.iter().enumerate().map(|(i, arg)| {
            let after = i.checked_sub(1).and_then(|before| flag(&self.args[before])).is_some_and(|name| rein_sandbox::looks_secret(name, ""));
            let own = arg.split_once('=').is_some_and(|(name, value)| rein_sandbox::looks_secret(name.trim_start_matches('-'), value));
            written(arg) && (after || own || rein_sandbox::looks_secret("", arg))
        })
    }

    /// What it runs, for a list where a key it was given mustn't show: its
    /// command and arguments, one that may be a key left out, and the names
    /// of its variables, never their values.
    pub fn described(&self) -> String {
        let args = self.args.iter().zip(self.keyed()).map(|(arg, keyed)| if keyed { "…".to_owned() } else { quote(arg) });
        let line = std::iter::once(quote(&self.command)).chain(args).collect::<Vec<_>>().join(" ");
        if self.env.is_empty() { line } else { format!("{line} (with {})", self.env.keys().cloned().collect::<Vec<_>>().join(", ")) }
    }

    /// What it runs as you'd type it in a shell, its environment first,
    /// since a variable can run code too (`NODE_OPTIONS`): for asking.
    pub fn command_line(&self) -> String {
        let settings = self.env.iter().map(|(name, value)| format!("{name}={}", quote(value)));
        settings.chain(std::iter::once(&self.command).chain(&self.args).map(|word| quote(word))).collect::<Vec<_>>().join(" ")
    }

    /// The variables of yours it gets, by name: those its `${NAME}`s name.
    pub fn takes(&self) -> Vec<String> {
        let mut names: Vec<String> = std::iter::once(&self.command).chain(&self.args).chain(self.env.values()).flat_map(|text| references(text)).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Its command, arguments and environment with each `${NAME}` filled
    /// in from `var`; what's missing if one isn't set.
    pub(crate) fn filled(&self, var: &dyn Fn(&str) -> Option<String>) -> Result<Filled, Unfilled> {
        Ok(Filled {
            command: fill(&self.command, var)?,
            args: self.args.iter().map(|arg| fill(arg, var)).collect::<Result<_, _>>()?,
            env: self.env.iter().map(|(name, value)| Ok((name.clone(), fill(value, var)?))).collect::<Result<_, Unfilled>>()?,
        })
    }

    /// The first variable of yours it needs that `var` has no value for,
    /// and no default stands in for: what to ask for before it can start.
    pub fn needs(&self, var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        match self.filled(var) {
            Err(Unfilled::Needs(name)) => Some(name),
            _ => None,
        }
    }

    /// The values of its environment that rein must hide wherever they
    /// show up: each filled in from a variable of yours, and each whose
    /// name or value says it's a key. Those that can't be filled in aren't
    /// there to hide.
    pub fn secrets(&self, var: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        self.env.iter().filter(|(name, value)| !references(value).is_empty() || rein_sandbox::looks_secret(name, value)).filter_map(|(_, value)| fill(value, var).ok()).collect()
    }
}

/// Why a server's `${NAME}`s can't be filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unfilled {
    /// A variable of yours it names isn't set: its name.
    Needs(String),
    /// A `${` doesn't end, in this text.
    Unended(String),
}

impl std::fmt::Display for Unfilled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unfilled::Needs(name) => write!(f, "it needs {name}, which isn't set"),
            Unfilled::Unended(text) => write!(f, "`{text}` has a ${{ that doesn't end"),
        }
    }
}

/// A server's command, arguments and environment, filled in.
pub(crate) struct Filled {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
}

/// `text` with each `${NAME}` the value of the variable `NAME`, and each
/// `${NAME:-default}` that or `default`, as in a shell. A `$` that doesn't
/// start one stays as it is.
fn fill(text: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<String, Unfilled> {
    let mut filled = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        filled.push_str(&rest[..at]);
        let inside = &rest[at + 2..];
        let end = inside.find('}').ok_or_else(|| Unfilled::Unended(text.to_owned()))?;
        let (name, default) = match inside[..end].split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (&inside[..end], None),
        };
        match var(name).filter(|value| !value.is_empty()).or_else(|| default.map(str::to_owned)) {
            Some(value) => filled.push_str(&value),
            None => return Err(Unfilled::Needs(name.to_owned())),
        }
        rest = &inside[end + 1..];
    }
    filled.push_str(rest);
    Ok(filled)
}

/// Whether `value` is written in as it is, not a `${NAME}` of yours.
fn written(value: &str) -> bool {
    !value.is_empty() && references(value).is_empty()
}

/// The names `text`'s `${NAME}`s name.
fn references(text: &str) -> Vec<String> {
    text.split("${").skip(1).filter_map(|part| part.split_once('}')).map(|(inside, _)| inside.split_once(":-").map_or(inside, |(name, _)| name).to_owned()).collect()
}

/// `word` as a shell reads it back as one word: as it is if it's plain,
/// in single quotes if not.
pub(crate) fn quote(word: &str) -> String {
    let plain = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain { word.to_owned() } else { format!("'{}'", word.replace('\'', r"'\''")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"{ "mcpServers": {
        "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}", "MODE": "${GH_MODE:-fast}" }, "allow": ["get_issue"] },
        "docs": { "type": "stdio", "command": "uvx", "args": ["docs-mcp"], "access": "look" },
        "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
        "web": { "url": "https://example.com/sse" },
        "bad name": { "command": "x" },
        "a__b": { "command": "x" },
        "typo": { "command": "x", "access": "net" },
        "nothing": { "args": ["x"] }
    } }"#;

    #[test]
    fn reads_what_claude_codes_files_say_and_skips_what_rein_cant_start() {
        let named = read(FILE, Origin::Yours).unwrap();
        let names: Vec<&str> = named.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["docs", "github"]);
        let github = &named.servers[1];
        assert_eq!((github.access, github.allow.as_slice()), (Access::Network, ["get_issue".to_owned()].as_slice()), "network when not said");
        assert_eq!(named.servers[0].access, Access::Look);
        let skipped: Vec<(&str, &str)> = named.skipped.iter().map(|s| (s.name.as_str(), s.why.as_str())).collect();
        assert_eq!(skipped.len(), 6, "{skipped:?}");
        assert!(skipped.iter().any(|(name, why)| *name == "linear" && why.contains("URL")));
        assert!(skipped.iter().any(|(name, why)| *name == "web" && why.contains("URL")));
        assert!(skipped.iter().any(|(name, why)| *name == "a__b" && why.contains("two _")));
        assert!(skipped.iter().any(|(name, why)| *name == "typo" && why.contains("net")), "a misspelt access is said, not taken as network");
        assert!(skipped.iter().any(|(name, why)| *name == "nothing" && why.contains("no command")));
        assert!(read("not json", Origin::Yours).is_err());
        assert_eq!(read("{}", Origin::Yours).unwrap(), Named::default());
    }

    #[test]
    fn a_project_never_says_what_runs_without_asking_and_yours_win_a_name() {
        let project = read(FILE, Origin::Project).unwrap();
        assert!(project.servers.iter().all(|s| s.allow.is_empty()));
        let yours = read(r#"{"mcpServers":{"github":{"command":"gh-mcp"}}}"#, Origin::Yours).unwrap();
        let both = yours.and(project);
        let github: Vec<&Server> = both.servers.iter().filter(|s| s.name == "github").collect();
        assert_eq!((github.len(), github[0].command.as_str()), (1, "gh-mcp"));
        assert!(both.skipped.iter().any(|s| s.name == "github" && s.origin == Origin::Project));
    }

    #[test]
    fn a_server_turned_off_is_kept_and_not_started_and_still_wins_its_name() {
        let file = r#"{ "mcpServers": { "gh": { "command": "gh-mcp", "disabled": true }, "docs": { "command": "uvx", "disabled": false } } }"#;
        let yours = read(file, Origin::Yours).unwrap();
        assert_eq!((yours.servers.len(), yours.servers[0].name.as_str()), (1, "docs"));
        assert_eq!((yours.off.len(), yours.off[0].command.as_str()), (1, "gh-mcp"), "kept, to turn on again");
        let project = read(r#"{ "mcpServers": { "gh": { "command": "evil" }, "x": { "command": "x", "disabled": true } } }"#, Origin::Project).unwrap();
        assert!(project.off.is_empty() && project.skipped.iter().any(|s| s.name == "x" && s.why.contains("turns it off")), "a project's is only skipped");
        let both = yours.and(project);
        assert!(both.servers.iter().all(|s| s.command != "evil"), "your gh, though off, is the one you meant");
        assert!(both.skipped.iter().any(|s| s.name == "gh" && s.origin == Origin::Project));
    }

    #[test]
    fn variables_are_filled_in_as_it_starts_and_what_they_hold_is_hidden() {
        let github = read(FILE, Origin::Yours).unwrap().servers.remove(1);
        assert_eq!(github.takes(), ["GH_MODE", "GITHUB_TOKEN"]);
        let var = |name: &str| (name == "GITHUB_TOKEN").then(|| "FAKE-gh-token-123".to_owned());
        let filled = github.filled(&var).unwrap();
        assert_eq!(filled.env["GITHUB_PERSONAL_ACCESS_TOKEN"], "FAKE-gh-token-123");
        assert_eq!(filled.env["MODE"], "fast", "the default, as in a shell");
        assert_eq!(github.secrets(&var), ["FAKE-gh-token-123", "fast"], "what came from your variables");
        assert_eq!(github.filled(&|_| None).err(), Some(Unfilled::Needs("GITHUB_TOKEN".into())), "what to ask for, by name");
        assert_eq!(github.needs(&|_| None).as_deref(), Some("GITHUB_TOKEN"));
        assert_eq!(github.needs(&var), None, "GH_MODE has a default");
        assert_eq!(fill("cost: $5 ${A}", &|_| Some("x".into())).unwrap(), "cost: $5 x");
        assert_eq!(fill("${A", &|_| None), Err(Unfilled::Unended("${A".into())));
        let literal = Server { env: [("API_KEY".to_owned(), "FAKE-literal-key".to_owned()), ("LEVEL".to_owned(), "verbose".to_owned())].into(), ..github };
        assert_eq!(literal.secrets(&|_| None), ["FAKE-literal-key"], "a key written in as it is, by its name");
        assert!(!format!("{literal:?}").contains("FAKE-literal-key"));
    }

    #[test]
    fn a_change_to_anything_a_server_is_changes_what_you_said_yes_to() {
        let server = read(FILE, Origin::Project).unwrap().servers.remove(1);
        let changed = [
            Server { command: "npm".into(), ..server.clone() },
            Server { args: vec!["-y".into(), "@evil/server-github".into()], ..server.clone() },
            Server { env: [("NODE_OPTIONS".to_owned(), "--require ./x.js".to_owned())].into(), ..server.clone() },
            Server { access: Access::Outside, ..server.clone() },
            Server { name: "gh".into(), ..server.clone() },
        ];
        for other in changed {
            assert_ne!(other.fingerprint(), server.fingerprint(), "{other:?}");
        }
        assert_eq!(server.fingerprint(), server.clone().fingerprint());
        assert_eq!(server.command_line(), "GITHUB_PERSONAL_ACCESS_TOKEN='${GITHUB_TOKEN}' MODE='${GH_MODE:-fast}' npx -y @modelcontextprotocol/server-github");
        assert_eq!(quote("it's here"), r"'it'\''s here'");
    }

    /// One server, as `/mcp add` took it.
    fn one(text: &str) -> Server {
        let named = added(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert!(named.skipped.is_empty(), "{text}: {:?}", named.skipped);
        let [server] = <[Server; 1]>::try_from(named.servers).unwrap_or_else(|servers| panic!("{text}: {servers:?}"));
        server
    }

    #[test]
    fn a_server_is_added_as_youd_type_it_in_a_shell() {
        let github = one("github GITHUB_PERSONAL_ACCESS_TOKEN=$GITHUB_TOKEN npx -y @modelcontextprotocol/server-github");
        assert_eq!((github.name.as_str(), github.command.as_str(), github.origin), ("github", "npx", Origin::Yours));
        assert_eq!(github.args, ["-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(github.env["GITHUB_PERSONAL_ACCESS_TOKEN"], "${GITHUB_TOKEN}", "a shell's $NAME is filled in as it starts");
        assert_eq!(github.access, Access::Network);
        let quoted = one(r#"docs --access look -e "ROOT=my docs" -- uvx docs-mcp --title 'it'\''s $HOME' "a \"b\"" c\ d ${X:-y}"#);
        assert_eq!(quoted.access, Access::Look);
        assert_eq!(quoted.env["ROOT"], "my docs");
        assert_eq!(quoted.args, ["docs-mcp", "--title", "it's $HOME", r#"a "b""#, "c d", "${X:-y}"], "single quotes leave a $ as it is");
        assert_eq!(one("x --access=outside A=1 B= sh -c 'exec x'").env.len(), 2);
        assert_eq!(one("x --access=outside sh").access, Access::Outside);
        for (text, why) in [
            ("github", "takes a name, then the command"),
            ("--access outside", "takes a name"),
            ("x --frobnicate sh", "doesn't know --frobnicate"),
            ("x -e NOPE sh", "-e takes NAME=value"),
            ("x sh 'open", "isn't closed"),
        ] {
            let e = added(text).err().unwrap_or_else(|| panic!("{text} isn't a server"));
            assert!(e.contains(why), "{text}: {e}");
        }
        let skipped = added("x --access net sh").unwrap().skipped;
        assert!(skipped[0].why.contains("net"), "a misspelt access is said: {skipped:?}");
        assert!(added("a__b sh").unwrap().skipped[0].why.contains("two _"));
    }

    #[test]
    fn a_servers_readme_snippet_is_added_as_it_is() {
        let file = r#"{ "mcpServers": { "github": { "command": "npx", "args": ["-y", "gh"] }, "linear": { "url": "https://mcp.linear.app/mcp" } } }"#;
        let named = added(file).unwrap();
        assert_eq!((named.servers.len(), named.servers[0].name.as_str()), (1, "github"));
        assert!(named.skipped[0].why.contains("URL"), "{:?}", named.skipped);
        assert_eq!(one(r#""github": { "command": "npx", "args": ["-y", "gh"] },"#).args, ["-y", "gh"], "the part to paste in, a comma after it");
        assert_eq!(one(r#"{ "github": { "command": "npx" } }"#).command, "npx");
        assert_eq!(one(r#"{ "servers": { "gh": { "type": "stdio", "command": "npx" } } }"#).name, "gh", "VS Code's shape");
        assert_eq!(one("gh {\n  \"command\": \"npx\",\n  \"access\": \"look\"\n}").access, Access::Look);
        assert!(added(r#"{ "command": "npx" }"#).unwrap_err().contains("say what to call it"));
        assert!(added("{ not json").unwrap_err().contains("isn't JSON"));
    }

    #[test]
    fn a_server_is_written_as_it_is_read_back() {
        for server in [one("github GITHUB_TOKEN=${GH} npx -y gh"), one("x --access outside sh"), Server { allow: vec!["*".into()], ..one("y sh") }] {
            let file = json!({ "mcpServers": { &server.name: server.entry() } }).to_string();
            assert_eq!(read(&file, Origin::Yours).unwrap().servers, std::slice::from_ref(&server), "{file}");
        }
        assert_eq!(one("y sh").entry(), json!({ "command": "sh" }), "nothing that goes without saying");
    }

    #[test]
    fn a_line_that_may_hold_a_key_is_known() {
        for text in ["gh GITHUB_TOKEN=ghp_FAKE npx gh", "gh LEVEL=debug npx gh", "s npx s --api-key sk-FAKE", "s npx s --token=FAKE", "db npx pg postgres://me:pw@host/db"] {
            assert!(one(text).may_hold_keys(), "{text}");
        }
        for text in ["gh GITHUB_TOKEN=$GITHUB_TOKEN npx -y gh", "s npx s --api-key ${KEY}", "s npx -y s --verbose", "d uvx docs-mcp ./docs"] {
            assert!(!one(text).may_hold_keys(), "{text}");
        }
        let debug = format!("{:?}", one("s npx s --api-key sk-FAKE --token=FAKE --verbose"));
        assert!(!debug.contains("FAKE") && debug.contains("--verbose"), "{debug}");
        for word in ["sk-or-v1-FAKE", "ghp_FAKE", "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7", "postgres://me:pw@db/x"] {
            assert!(looks_like_a_key(word), "{word}");
        }
        for word in ["playwright", "context7", "github-mcp-server", "mcp-server-sequential-thinking"] {
            assert!(!looks_like_a_key(word), "{word}");
        }
        let described = one("s GITHUB_TOKEN=ghp_FAKE npx s --api-key sk-FAKE --verbose").described();
        assert_eq!(described, "npx s --api-key … --verbose (with GITHUB_TOKEN)", "names, never values");
    }
}
