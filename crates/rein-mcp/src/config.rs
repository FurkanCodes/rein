//! Which MCP servers to start, or reach, as an `mcpServers` file says. It's
//! the shape Claude Code, Cursor and others read, so the snippet in a
//! server's README works as it is:
//!
//! ```json
//! { "mcpServers": {
//!     "github": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-github"],
//!       "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" },
//!       "access": "network",
//!       "allow": ["get_issue", "search_issues"]
//!     },
//!     "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
//!     "context7": { "type": "http", "url": "https://mcp.context7.com/mcp", "headers": { "CONTEXT7_API_KEY": "${CONTEXT7_API_KEY}" } }
//! } }
//! ```
//!
//! `access` and `allow` are rein's own. `access` is the walls the server
//! runs inside, as a command's are, whatever the mode: `outside` for one
//! that needs your whole home folder. Not said, the server runs as your
//! mode has it: outside the sandbox in trust, as your commands do, and in
//! it with `network` in the others. A server reached at a URL runs none of
//! its code here, and has none. `allow` names tools that run
//! without asking, or `*` for all of one server's; it counts only in your
//! own file, never in a project's. `"disabled": true`, as Cline writes it,
//! keeps a server in the file without starting it. `"tools": "search"`
//! holds a server's tools back until the model finds them with the tool
//! search, and `"tools": "upfront"` shows them from the start; without it,
//! they're held back once they'd take more than about 2,000 tokens.

use std::collections::BTreeMap;

use rein_sandbox::Access;
use serde::Deserialize;
use serde_json::{Value, json};

pub use crate::added::{added, looks_like_a_key};

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

/// A server to start, or to reach.
#[derive(Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub origin: Origin,
    pub reach: Reach,
    /// Its tools that run without asking: their names, or `*` for all.
    pub allow: Vec<String>,
    /// When the model is shown its tools.
    pub offered: Offered,
}

/// When the model is shown a server's tools, as your file says in
/// `"tools"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum Offered {
    /// As rein decides: held back until the model finds them once they'd
    /// take more than [`crate::HOLD_BACK_FROM`] tokens of every request.
    #[default]
    #[serde(skip)]
    BySize,
    /// From the start: `"upfront"`.
    #[serde(rename = "upfront")]
    UpFront,
    /// Once the model finds them with the tool search: `"search"`.
    #[serde(rename = "search")]
    OnSearch,
}

impl Offered {
    /// As your file says it; nothing for rein's own choice.
    pub const fn word(self) -> Option<&'static str> {
        match self {
            Offered::BySize => None,
            Offered::UpFront => Some("upfront"),
            Offered::OnSearch => Some("search"),
        }
    }
}

/// How a server is reached.
#[derive(Clone, PartialEq, Eq)]
pub enum Reach {
    /// A command on this computer, talked to over its standard input and
    /// output.
    Local(Local),
    /// A server at a URL, talked to over MCP's Streamable HTTP.
    Remote(Remote),
}

/// A server that's a command on this computer.
#[derive(Clone, PartialEq, Eq)]
pub struct Local {
    pub command: String,
    pub args: Vec<String>,
    /// As written: `${NAME}` stands for a variable of yours, filled in
    /// only as the server starts.
    pub env: BTreeMap<String, String>,
    /// The walls it runs inside.
    pub access: Walled,
}

/// The walls a server on this computer runs inside: none, unless it has
/// its own, whatever the mode and the sandbox. Codex and Claude Code run
/// every server outside their sandboxes, and walls broke what servers need
/// (a browser, the keychain, another app); one of someone else's waits for
/// your yes to what it runs instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Walled {
    /// None of its own: it runs outside the sandbox.
    #[default]
    Unset,
    /// Its own, set in your file or with `/mcp`.
    Set(Access),
}

impl Walled {
    /// Its walls: `outside` where it has none of its own.
    pub fn access(self) -> Access {
        match self {
            Walled::Unset => Access::Outside,
            Walled::Set(access) => access,
        }
    }
}

/// A server reached at a URL: none of its code runs here.
#[derive(Clone, PartialEq, Eq)]
pub struct Remote {
    /// As written, a `${NAME}` of yours filled in as it's reached.
    pub url: String,
    /// What's sent with each request, as written: a key in one is a
    /// `${NAME}` of yours, or written in as it is.
    pub headers: BTreeMap<String, String>,
    pub wire: Wire,
}

/// How a server at a URL is spoken to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wire {
    /// MCP's Streamable HTTP, and if the server turns that away, the HTTP
    /// with SSE before it, as MCP's spec says to try (`"type": "http"`).
    #[default]
    Http,
    /// Only the HTTP with SSE of MCP before Streamable HTTP (2024-11-05):
    /// a stream that's kept open for every answer (`"type": "sse"`).
    Sse,
}

/// Never what `env` or `headers` say, or an argument or a part of a URL
/// that may be a key: a file of yours may have one in it as it is.
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("reach", &self.reach)
            .field("allow", &self.allow)
            .field("offered", &self.offered)
            .finish()
    }
}

impl std::fmt::Debug for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reach::Local(local) => local.fmt(f),
            Reach::Remote(remote) => remote.fmt(f),
        }
    }
}

/// Its variables by name, and its arguments but those that may be keys.
impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args: Vec<&str> = self.args.iter().zip(self.keyed()).map(|(arg, keyed)| if keyed { "…" } else { arg.as_str() }).collect();
        f.debug_struct("Local").field("command", &self.command).field("args", &args).field("env", &self.env.keys().collect::<Vec<_>>()).field("access", &self.access).finish()
    }
}

/// Its headers by name, and its URL but what may be a key in it.
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").field("url", &shown_url(&self.url)).field("headers", &self.headers.keys().collect::<Vec<_>>()).field("wire", &self.wire).finish()
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
    #[serde(default)]
    headers: BTreeMap<String, String>,
    access: Option<Access>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    disabled: bool,
    tools: Option<Offered>,
}

/// How a server is reached, as a file says: `http` is Streamable HTTP,
/// `sse` the transport before it.
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
        let reach = match reach(&entry) {
            Ok(reach) => reach,
            Err(why) => {
                named.skipped.push(skip(why));
                continue;
            }
        };
        // A project doesn't decide what runs without asking you.
        let allow = if origin == Origin::Yours { entry.allow } else { Vec::new() };
        let server = Server { name, origin, reach, allow, offered: entry.tools.unwrap_or_default() };
        match (entry.disabled, origin) {
            (false, _) => named.servers.push(server),
            (true, Origin::Yours) => named.off.push(server),
            (true, Origin::Project) => named.skipped.push(Skipped { name: server.name, origin, why: "its .mcp.json turns it off".into() }),
        }
    }
    named
}

/// How `entry` is reached, or why rein can't reach it.
fn reach(entry: &Entry) -> Result<Reach, String> {
    if entry.url.is_some() || matches!(entry.kind, Some(Kind::Http | Kind::Sse)) {
        let url = entry.url.clone().ok_or("it's reached at a URL, and its entry has none")?;
        if !["https://", "http://", "${"].iter().any(|start| url.starts_with(start)) {
            return Err("its url isn't one rein can reach: it starts with https://".into());
        }
        let wire = if entry.kind == Some(Kind::Sse) { Wire::Sse } else { Wire::Http };
        return Ok(Reach::Remote(Remote { url, headers: entry.headers.clone(), wire }));
    }
    let command = entry.command.clone().ok_or("it has no command to start")?;
    Ok(Reach::Local(Local { command, args: entry.args.clone(), env: entry.env.clone(), access: entry.access.map_or(Walled::Unset, Walled::Set) }))
}

/// A server's name goes into its tools' names (`mcp__github__get_issue`),
/// so it's what a tool's name may hold, without the `__` that ends it.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let fits = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if fits && !name.contains("__") && !name.starts_with('_') && !name.ends_with('_') {
        Ok(())
    } else {
        Err("a server's name may have letters, digits, - and _, but not two _ in a row".into())
    }
}

impl Local {
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
}

impl Server {
    /// The command, if it's one on this computer.
    pub fn local(&self) -> Option<&Local> {
        match &self.reach {
            Reach::Local(local) => Some(local),
            Reach::Remote(_) => None,
        }
    }

    /// The walls it runs inside, as its file has them: none for one
    /// reached at a URL, whose code doesn't run here.
    pub fn walled(&self) -> Option<Walled> {
        self.local().map(|local| local.access)
    }

    /// What you say yes to for a project's server: all of what it is, so
    /// that a change to any of it, another command, an argument, a
    /// variable, a URL or a header, asks again.
    pub fn fingerprint(&self) -> String {
        match &self.reach {
            // None of its own is none: the same yes as `outside`, and
            // another than "as the mode has it", which a server with none
            // once had, so that one is asked about again.
            Reach::Local(local) => {
                json!({ "name": self.name, "command": local.command, "args": local.args, "env": local.env, "access": local.access.access().as_str() })
            }
            Reach::Remote(remote) if remote.wire == Wire::Sse => json!({ "name": self.name, "url": remote.url, "headers": remote.headers, "type": "sse" }),
            Reach::Remote(remote) => json!({ "name": self.name, "url": remote.url, "headers": remote.headers }),
        }
        .to_string()
    }

    /// As your file has it: what [`read`] reads back as this server.
    pub fn entry(&self) -> Value {
        let mut entry = serde_json::Map::new();
        match &self.reach {
            Reach::Local(local) => {
                entry.insert("command".into(), json!(local.command));
                if !local.args.is_empty() {
                    entry.insert("args".into(), json!(local.args));
                }
                if !local.env.is_empty() {
                    entry.insert("env".into(), json!(local.env));
                }
                if let Walled::Set(access) = local.access {
                    entry.insert("access".into(), json!(access.as_str()));
                }
            }
            Reach::Remote(remote) => {
                entry.insert("type".into(), json!(if remote.wire == Wire::Sse { "sse" } else { "http" }));
                entry.insert("url".into(), json!(remote.url));
                if !remote.headers.is_empty() {
                    entry.insert("headers".into(), json!(remote.headers));
                }
            }
        }
        if !self.allow.is_empty() {
            entry.insert("allow".into(), json!(self.allow));
        }
        if let Some(word) = self.offered.word() {
            entry.insert("tools".into(), json!(word));
        }
        Value::Object(entry)
    }

    /// Whether what it runs, as it was typed, may hold a key: a value
    /// written into its environment or a header as it is, not as a
    /// `${NAME}`, an argument named like one (`--api-key sk-…`), or a URL
    /// with one in it. The line it was typed in is then never shown again,
    /// or kept.
    pub fn may_hold_keys(&self) -> bool {
        match &self.reach {
            Reach::Local(local) => local.env.values().any(|value| written(value)) || local.keyed().any(|keyed| keyed),
            Reach::Remote(remote) => remote.headers.values().any(|value| written(value)) || shown_url(&remote.url) != remote.url,
        }
    }

    /// What it runs or where it's reached, for a list where a key it was
    /// given mustn't show: its command and arguments, one that may be a
    /// key left out, or its URL with what may be a key left out, and the
    /// names of its variables or headers, never their values.
    pub fn described(&self) -> String {
        let (line, names) = match &self.reach {
            Reach::Local(local) => {
                let args = local.args.iter().zip(local.keyed()).map(|(arg, keyed)| if keyed { "…".to_owned() } else { quote(arg) });
                (std::iter::once(quote(&local.command)).chain(args).collect::<Vec<_>>().join(" "), local.env.keys())
            }
            Reach::Remote(remote) => (shown_url(&remote.url), remote.headers.keys()),
        };
        let names: Vec<&str> = names.map(String::as_str).collect();
        if names.is_empty() { line } else { format!("{line} (with {})", names.join(", ")) }
    }

    /// What it runs as you'd type it in a shell, its environment first,
    /// since a variable can run code too (`NODE_OPTIONS`); or its URL and
    /// headers: for asking.
    pub fn command_line(&self) -> String {
        match &self.reach {
            Reach::Local(local) => {
                let settings = local.env.iter().map(|(name, value)| format!("{name}={}", quote(value)));
                settings.chain(std::iter::once(&local.command).chain(&local.args).map(|word| quote(word))).collect::<Vec<_>>().join(" ")
            }
            Reach::Remote(remote) => std::iter::once(remote.url.clone())
                .chain(remote.headers.iter().map(|(name, value)| format!("-H {}", quote(&format!("{name}: {value}")))))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// Every word of it as written: where its `${NAME}`s may be.
    fn words(&self) -> Vec<&String> {
        match &self.reach {
            Reach::Local(local) => std::iter::once(&local.command).chain(&local.args).chain(local.env.values()).collect(),
            Reach::Remote(remote) => std::iter::once(&remote.url).chain(remote.headers.values()).collect(),
        }
    }

    /// The variables of yours it gets, by name: those its `${NAME}`s name.
    pub fn takes(&self) -> Vec<String> {
        let mut names: Vec<String> = self.words().into_iter().flat_map(|text| references(text)).collect();
        names.sort();
        names.dedup();
        names
    }

    /// How it's reached with each `${NAME}` filled in from `var`; what's
    /// missing if one isn't set.
    pub fn filled(&self, var: &dyn Fn(&str) -> Option<String>) -> Result<Reach, Unfilled> {
        let map = |values: &BTreeMap<String, String>| values.iter().map(|(name, value)| Ok((name.clone(), fill(value, var)?))).collect::<Result<_, Unfilled>>();
        Ok(match &self.reach {
            Reach::Local(local) => Reach::Local(Local {
                command: fill(&local.command, var)?,
                args: local.args.iter().map(|arg| fill(arg, var)).collect::<Result<_, _>>()?,
                env: map(&local.env)?,
                access: local.access,
            }),
            Reach::Remote(remote) => Reach::Remote(Remote { url: fill(&remote.url, var)?, headers: map(&remote.headers)?, wire: remote.wire }),
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

    /// The values of its environment or headers that rein must hide
    /// wherever they show up: each filled in from a variable of yours, and
    /// each whose name or value says it's a key, as a header that
    /// authorizes does, with the token after its `Bearer` too. Those that
    /// can't be filled in aren't there to hide.
    pub fn secrets(&self, var: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        let (values, headers) = match &self.reach {
            Reach::Local(local) => (&local.env, false),
            Reach::Remote(remote) => (&remote.headers, true),
        };
        let secret = |name: &str, value: &str| !references(value).is_empty() || rein_sandbox::looks_secret(name, value) || (headers && name.eq_ignore_ascii_case("authorization"));
        let mut secrets: Vec<String> = values.iter().filter(|(name, value)| secret(name, value)).filter_map(|(_, value)| fill(value, var).ok()).collect();
        let tokens: Vec<String> = secrets.iter().filter_map(|value| value.split_once(' ').map(|(_, token)| token.trim().to_owned())).filter(|token| !token.is_empty()).collect();
        secrets.extend(tokens);
        secrets
    }
}

/// `url` with what may be a key in it left out: a password, and the
/// value of a query parameter named like a key (`?api_key=…`).
pub fn shown_url(url: &str) -> String {
    let (base, query) = url.split_once('?').map_or((url, None), |(base, query)| (base, Some(query)));
    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').map_or((rest, ""), |(authority, path)| (authority, path));
            let authority = match authority.rsplit_once('@') {
                Some((user, host)) if user.contains(':') => format!("{}:…@{host}", user.split_once(':').map_or(user, |(user, _)| user)),
                _ => authority.to_owned(),
            };
            if path.is_empty() && !rest.contains('/') { format!("{scheme}://{authority}") } else { format!("{scheme}://{authority}/{path}") }
        }
        None => base.to_owned(),
    };
    let Some(query) = query else { return base };
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) if written(value) && (rein_sandbox::looks_secret(name, value) || looks_like_a_key(value)) => format!("{name}=…"),
            _ => pair.to_owned(),
        })
        .collect();
    format!("{base}?{}", pairs.join("&"))
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

/// `text` with each `${NAME}` the value of the variable `NAME`, and each
/// `${NAME:-default}` that or `default`, as in a shell. A `$` that doesn't
/// start one stays as it is.
pub(crate) fn fill(text: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<String, Unfilled> {
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
pub(crate) fn written(value: &str) -> bool {
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
pub(crate) mod tests {
    use super::*;

    /// A command's server, for a test.
    pub(crate) fn local(name: &str, command: &str, args: &[&str]) -> Server {
        let local = Local { command: command.into(), args: args.iter().map(|arg| (*arg).to_owned()).collect(), env: BTreeMap::new(), access: Walled::Unset };
        Server { name: name.into(), origin: Origin::Yours, reach: Reach::Local(local), allow: Vec::new(), offered: Default::default() }
    }

    /// The command of `server`, which is one.
    pub(crate) fn command(server: &Server) -> &Local {
        server.local().unwrap_or_else(|| panic!("{} is reached at a URL", server.name))
    }

    /// `server` with its command changed by `change`.
    fn with(server: &Server, change: impl FnOnce(&mut Local)) -> Server {
        let mut server = server.clone();
        if let Reach::Local(local) = &mut server.reach {
            change(local);
        }
        server
    }

    const FILE: &str = r#"{ "mcpServers": {
        "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}", "MODE": "${GH_MODE:-fast}" }, "allow": ["get_issue"] },
        "docs": { "type": "stdio", "command": "uvx", "args": ["docs-mcp"], "access": "look" },
        "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
        "ctx": { "url": "https://mcp.context7.com/mcp", "headers": { "CONTEXT7_API_KEY": "${CONTEXT7_KEY}" } },
        "old": { "type": "sse", "url": "https://mcp.asana.com/sse" },
        "ftp": { "url": "ftp://example.com/mcp" },
        "bad name": { "command": "x" },
        "a__b": { "command": "x" },
        "typo": { "command": "x", "access": "net" },
        "nothing": { "args": ["x"] }
    } }"#;

    #[test]
    fn reads_what_claude_codes_files_say_and_skips_what_rein_cant_start() {
        let named = read(FILE, Origin::Yours).unwrap();
        let names: Vec<&str> = named.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["ctx", "docs", "github", "linear", "old"]);
        let github = &named.servers[2];
        assert_eq!((command(github).access, github.allow.as_slice()), (Walled::Unset, ["get_issue".to_owned()].as_slice()), "none of its own when not said");
        assert_eq!(command(&named.servers[1]).access, Walled::Set(Access::Look));
        assert_eq!(named.servers[3].reach, Reach::Remote(Remote { url: "https://mcp.linear.app/mcp".into(), headers: BTreeMap::new(), wire: Wire::Http }));
        assert_eq!(named.servers[0].walled(), None, "one at a URL has no walls: none of its code runs here");
        let skipped: Vec<(&str, &str)> = named.skipped.iter().map(|s| (s.name.as_str(), s.why.as_str())).collect();
        assert_eq!(skipped.len(), 5, "{skipped:?}");
        assert_eq!(
            named.servers[4].reach,
            Reach::Remote(Remote { url: "https://mcp.asana.com/sse".into(), headers: BTreeMap::new(), wire: Wire::Sse }),
            "the SSE before Streamable HTTP"
        );
        assert!(skipped.iter().any(|(name, why)| *name == "ftp" && why.contains("https://")));
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
        assert_eq!((github.len(), command(github[0]).command.as_str()), (1, "gh-mcp"));
        assert!(both.skipped.iter().any(|s| s.name == "github" && s.origin == Origin::Project));
    }

    #[test]
    fn a_server_turned_off_is_kept_and_not_started_and_still_wins_its_name() {
        let file = r#"{ "mcpServers": { "gh": { "command": "gh-mcp", "disabled": true }, "docs": { "command": "uvx", "disabled": false } } }"#;
        let yours = read(file, Origin::Yours).unwrap();
        assert_eq!((yours.servers.len(), yours.servers[0].name.as_str()), (1, "docs"));
        assert_eq!((yours.off.len(), command(&yours.off[0]).command.as_str()), (1, "gh-mcp"), "kept, to turn on again");
        let project = read(r#"{ "mcpServers": { "gh": { "command": "evil" }, "x": { "command": "x", "disabled": true } } }"#, Origin::Project).unwrap();
        assert!(project.off.is_empty() && project.skipped.iter().any(|s| s.name == "x" && s.why.contains("turns it off")), "a project's is only skipped");
        let both = yours.and(project);
        assert!(both.servers.iter().all(|s| s.local().is_none_or(|local| local.command != "evil")), "your gh, though off, is the one you meant");
        assert!(both.skipped.iter().any(|s| s.name == "gh" && s.origin == Origin::Project));
    }

    #[test]
    fn variables_are_filled_in_as_it_starts_and_what_they_hold_is_hidden() {
        let named = read(FILE, Origin::Yours).unwrap();
        let github = named.servers[2].clone();
        assert_eq!(github.takes(), ["GH_MODE", "GITHUB_TOKEN"]);
        let var = |name: &str| (name == "GITHUB_TOKEN").then(|| "FAKE-gh-token-123".to_owned());
        let Reach::Local(filled) = github.filled(&var).unwrap() else { panic!("a command") };
        assert_eq!(filled.env["GITHUB_PERSONAL_ACCESS_TOKEN"], "FAKE-gh-token-123");
        assert_eq!(filled.env["MODE"], "fast", "the default, as in a shell");
        assert_eq!(github.secrets(&var), ["FAKE-gh-token-123", "fast"], "what came from your variables");
        assert_eq!(github.filled(&|_| None).err(), Some(Unfilled::Needs("GITHUB_TOKEN".into())), "what to ask for, by name");
        assert_eq!(github.needs(&|_| None).as_deref(), Some("GITHUB_TOKEN"));
        assert_eq!(github.needs(&var), None, "GH_MODE has a default");
        assert_eq!(fill("cost: $5 ${A}", &|_| Some("x".into())).unwrap(), "cost: $5 x");
        assert_eq!(fill("${A", &|_| None), Err(Unfilled::Unended("${A".into())));
        let literal = with(&github, |local| local.env = [("API_KEY".to_owned(), "FAKE-literal-key".to_owned()), ("LEVEL".to_owned(), "verbose".to_owned())].into());
        assert_eq!(literal.secrets(&|_| None), ["FAKE-literal-key"], "a key written in as it is, by its name");
        assert!(!format!("{literal:?}").contains("FAKE-literal-key"));

        let ctx = &named.servers[0];
        assert_eq!(ctx.needs(&|_| None).as_deref(), Some("CONTEXT7_KEY"), "a header's variable is asked for as an environment's is");
        let key = |_: &str| Some("FAKE-ctx-key".to_owned());
        let Reach::Remote(filled) = ctx.filled(&key).unwrap() else { panic!("a URL") };
        assert_eq!(filled.headers["CONTEXT7_API_KEY"], "FAKE-ctx-key");
        let bearer = Server {
            reach: Reach::Remote(Remote { url: "https://x.example/mcp".into(), headers: [("Authorization".to_owned(), "Bearer FAKE-bearer".to_owned())].into(), wire: Wire::Http }),
            ..ctx.clone()
        };
        assert_eq!(bearer.secrets(&|_| None), ["Bearer FAKE-bearer", "FAKE-bearer"], "the token alone too, as it may show up alone");
        assert!(!format!("{bearer:?}").contains("FAKE"), "{bearer:?}");
    }

    #[test]
    fn a_change_to_anything_a_server_is_changes_what_you_said_yes_to() {
        let named = read(FILE, Origin::Project).unwrap();
        let server = named.servers[2].clone();
        let changed = [
            with(&server, |local| local.command = "npm".into()),
            with(&server, |local| local.args = vec!["-y".into(), "@evil/server-github".into()]),
            with(&server, |local| local.env = [("NODE_OPTIONS".to_owned(), "--require ./x.js".to_owned())].into()),
            with(&server, |local| local.access = Walled::Set(Access::Look)),
            with(&server, |local| local.access = Walled::Set(Access::Network)),
            Server { name: "gh".into(), ..server.clone() },
        ];
        for other in changed {
            assert_ne!(other.fingerprint(), server.fingerprint(), "{other:?}");
        }
        // None of its own is none: the same yes as `outside`.
        assert_eq!(server.walled(), Some(Walled::Unset));
        assert_eq!(with(&server, |local| local.access = Walled::Set(Access::Outside)).fingerprint(), server.fingerprint());
        assert!(server.fingerprint().contains(r#""access":"outside""#), "not what a yes to walls by mode was: {}", server.fingerprint());
        assert_eq!(server.fingerprint(), server.clone().fingerprint());
        assert_eq!(server.command_line(), "GITHUB_PERSONAL_ACCESS_TOKEN='${GITHUB_TOKEN}' MODE='${GH_MODE:-fast}' npx -y @modelcontextprotocol/server-github");
        assert_eq!(quote("it's here"), r"'it'\''s here'");
        let ctx = named.servers[0].clone();
        let remote = |url: &str, headers: &[(&str, &str)]| Server {
            reach: Reach::Remote(Remote { url: url.into(), headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(), wire: Wire::Http }),
            ..ctx.clone()
        };
        assert_ne!(remote("https://evil.example/mcp", &[("CONTEXT7_API_KEY", "${CONTEXT7_KEY}")]).fingerprint(), ctx.fingerprint(), "another URL");
        assert_ne!(remote("https://mcp.context7.com/mcp", &[("CONTEXT7_API_KEY", "${GITHUB_TOKEN}")]).fingerprint(), ctx.fingerprint(), "another of your variables");
        assert_eq!(ctx.command_line(), "https://mcp.context7.com/mcp -H 'CONTEXT7_API_KEY: ${CONTEXT7_KEY}'");
    }

    #[test]
    fn a_server_is_written_as_it_is_read_back() {
        let mut named = read(FILE, Origin::Yours).unwrap();
        let (remote, sse) = (named.servers.remove(0), named.servers.pop().unwrap());
        let mut http = sse.clone();
        if let Reach::Remote(remote) = &mut http.reach {
            remote.wire = Wire::Http;
        }
        assert_ne!(sse.fingerprint(), http.fingerprint(), "spoken to another way: asked about again");
        for server in [
            sse,
            local("github", "npx", &["-y", "gh"]),
            with(&local("x", "sh", &[]), |local| local.access = Walled::Set(Access::Outside)),
            with(&local("n", "sh", &[]), |local| local.access = Walled::Set(Access::Network)),
            Server { allow: vec!["*".into()], ..local("y", "sh", &[]) },
            Server { offered: Offered::OnSearch, ..local("z", "sh", &[]) },
            Server { offered: Offered::UpFront, ..local("w", "sh", &[]) },
            remote,
        ] {
            let file = json!({ "mcpServers": { &server.name: server.entry() } }).to_string();
            assert_eq!(read(&file, Origin::Yours).unwrap().servers, std::slice::from_ref(&server), "{file}");
        }
        assert_eq!(local("y", "sh", &[]).entry(), json!({ "command": "sh" }), "nothing that goes without saying");
    }

    #[test]
    fn a_file_says_when_a_servers_tools_are_shown() {
        let file = r#"{ "mcpServers": { "a": { "command": "x", "tools": "search" }, "b": { "command": "x", "tools": "upfront" }, "c": { "command": "x" }, "d": { "command": "x", "tools": "later" } } }"#;
        let named = read(file, Origin::Yours).unwrap();
        let offered: Vec<(&str, Offered)> = named.servers.iter().map(|s| (s.name.as_str(), s.offered)).collect();
        assert_eq!(offered, [("a", Offered::OnSearch), ("b", Offered::UpFront), ("c", Offered::BySize)]);
        assert_eq!(named.skipped[0].name, "d", "a word rein doesn't know is said, not guessed at");
        assert!(named.skipped[0].why.contains("upfront"), "{}", named.skipped[0].why);
    }

    #[test]
    fn what_may_be_a_key_in_a_url_is_left_out_where_its_shown() {
        assert_eq!(shown_url("https://x.example/mcp?api_key=FAKE123&region=eu"), "https://x.example/mcp?api_key=…&region=eu");
        assert_eq!(shown_url("https://me:FAKE-pw@x.example/mcp"), "https://me:…@x.example/mcp");
        assert_eq!(shown_url("https://x.example"), "https://x.example");
        assert_eq!(shown_url("https://x.example/mcp?key=${KEY}"), "https://x.example/mcp?key=${KEY}", "a variable of yours isn't a key written in");
        let server = |url: &str| Server { reach: Reach::Remote(Remote { url: url.into(), headers: BTreeMap::new(), wire: Wire::Http }), ..local("x", "sh", &[]) };
        assert!(server("https://x.example/mcp?api_key=FAKE123").may_hold_keys());
        assert!(!server("https://x.example/mcp").may_hold_keys());
        assert_eq!(server("https://x.example/mcp?token=FAKE123").described(), "https://x.example/mcp?token=…");
    }
}
