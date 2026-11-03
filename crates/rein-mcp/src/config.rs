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
//! own file, never in a project's.

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

/// Never what `env` says: a file of yours may have a key in it as it is.
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("command", &self.command)
            .field("args", &self.args)
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

/// What files name: the servers to start, and those rein can't.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Named {
    pub servers: Vec<Server>,
    pub skipped: Vec<Skipped>,
}

impl Named {
    /// Both files' servers. One of the project's with the name of one of
    /// yours is left out: yours is the one you meant.
    pub fn and(mut self, project: Named) -> Named {
        for server in project.servers {
            if self.servers.iter().any(|yours| yours.name == server.name) {
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
    let mut named = Named::default();
    for (name, entry) in file.servers {
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
        match entry.command {
            _ if remote => named.skipped.push(skip("it's reached at a URL, and rein only starts servers on this computer so far".into())),
            None => named.skipped.push(skip("it has no command to start".into())),
            Some(command) => named.servers.push(Server {
                name,
                origin,
                command,
                args: entry.args,
                env: entry.env,
                access: entry.access.unwrap_or(Access::Network),
                // A project doesn't decide what runs without asking you.
                allow: if origin == Origin::Yours { entry.allow } else { Vec::new() },
            }),
        }
    }
    Ok(named)
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
    pub(crate) fn filled(&self, var: &dyn Fn(&str) -> Option<String>) -> Result<Filled, String> {
        Ok(Filled {
            command: fill(&self.command, var)?,
            args: self.args.iter().map(|arg| fill(arg, var)).collect::<Result<_, _>>()?,
            env: self.env.iter().map(|(name, value)| Ok((name.clone(), fill(value, var)?))).collect::<Result<_, String>>()?,
        })
    }

    /// The values of its environment that rein must hide wherever they
    /// show up: each filled in from a variable of yours, and each whose
    /// name or value says it's a key. Those that can't be filled in aren't
    /// there to hide.
    pub fn secrets(&self, var: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        self.env.iter().filter(|(name, value)| !references(value).is_empty() || rein_sandbox::looks_secret(name, value)).filter_map(|(_, value)| fill(value, var).ok()).collect()
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
fn fill(text: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut filled = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        filled.push_str(&rest[..at]);
        let inside = &rest[at + 2..];
        let end = inside.find('}').ok_or_else(|| format!("`{text}` has a ${{ that doesn't end"))?;
        let (name, default) = match inside[..end].split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (&inside[..end], None),
        };
        match var(name).filter(|value| !value.is_empty()).or_else(|| default.map(str::to_owned)) {
            Some(value) => filled.push_str(&value),
            None => return Err(format!("it needs {name}, which isn't set")),
        }
        rest = &inside[end + 1..];
    }
    filled.push_str(rest);
    Ok(filled)
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
    fn variables_are_filled_in_as_it_starts_and_what_they_hold_is_hidden() {
        let github = read(FILE, Origin::Yours).unwrap().servers.remove(1);
        assert_eq!(github.takes(), ["GH_MODE", "GITHUB_TOKEN"]);
        let var = |name: &str| (name == "GITHUB_TOKEN").then(|| "FAKE-gh-token-123".to_owned());
        let filled = github.filled(&var).unwrap();
        assert_eq!(filled.env["GITHUB_PERSONAL_ACCESS_TOKEN"], "FAKE-gh-token-123");
        assert_eq!(filled.env["MODE"], "fast", "the default, as in a shell");
        assert_eq!(github.secrets(&var), ["FAKE-gh-token-123", "fast"], "what came from your variables");
        let missing = github.filled(&|_| None).err().unwrap();
        assert!(missing.contains("GITHUB_TOKEN"), "{missing}");
        assert_eq!(fill("cost: $5 ${A}", &|_| Some("x".into())).unwrap(), "cost: $5 x");
        assert!(fill("${A", &|_| None).is_err());
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
}
