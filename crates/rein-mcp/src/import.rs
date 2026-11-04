//! Servers you set up in other apps, to bring into rein (`/mcp import`):
//! those Claude Code, Claude Desktop, Cursor, Codex, VS Code, Windsurf and
//! Gemini CLI keep for you. Not a project's own files, which may be
//! anyone's: rein asks about a project's `.mcp.json` as it is.
//!
//! Each app writes a variable of yours its own way (`${env:NAME}`,
//! `$NAME`, `${input:id}`); they're read as rein's `${NAME}`, filled in as
//! rein starts the server, from your environment or the keys you gave rein.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{Named, Origin, Server, Skipped};

/// An app whose servers rein can bring in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Codex,
    VsCode,
    Windsurf,
    GeminiCli,
}

impl App {
    pub const ALL: [App; 7] = [App::ClaudeCode, App::ClaudeDesktop, App::Cursor, App::Codex, App::VsCode, App::Windsurf, App::GeminiCli];

    pub const fn title(self) -> &'static str {
        match self {
            App::ClaudeCode => "Claude Code",
            App::ClaudeDesktop => "Claude Desktop",
            App::Cursor => "Cursor",
            App::Codex => "Codex",
            App::VsCode => "VS Code",
            App::Windsurf => "Windsurf",
            App::GeminiCli => "Gemini CLI",
        }
    }

    /// Where it keeps your servers, in `home`; Codex's in `codex_home` if
    /// you set `$CODEX_HOME`.
    pub fn file(self, home: &Path, codex_home: Option<&Path>) -> PathBuf {
        // Where an app keeps its settings on a Mac, and elsewhere.
        let settings = |app: &str| if cfg!(target_os = "macos") { home.join("Library/Application Support").join(app) } else { home.join(".config").join(app) };
        match self {
            App::ClaudeCode => home.join(".claude.json"),
            App::ClaudeDesktop => settings("Claude").join("claude_desktop_config.json"),
            App::Cursor => home.join(".cursor/mcp.json"),
            App::Codex => codex_home.map_or_else(|| home.join(".codex"), Path::to_path_buf).join("config.toml"),
            App::VsCode => settings("Code/User").join("mcp.json"),
            App::Windsurf => home.join(".codeium/windsurf/mcp_config.json"),
            App::GeminiCli => home.join(".gemini/settings.json"),
        }
    }

    /// The servers in `text`, its file, as yours: for Claude Code, those it
    /// starts everywhere and then this project's, at `root`, which it starts
    /// over those of the same name. `home` is what `${userHome}` is.
    pub fn read(self, text: &str, home: &Path, root: &Path) -> Result<Named, String> {
        let (entries, skipped) = match self {
            App::Codex => codex(text)?,
            _ => {
                let file: Value = serde_json::from_str(text).map_err(|e| format!("isn't JSON rein can read: {e}"))?;
                let mut entries = by_name(&file, if self == App::VsCode { "servers" } else { "mcpServers" });
                if self == App::ClaudeCode
                    && let Some(here) = file.get("projects").and_then(|projects| projects.get(root.display().to_string()))
                {
                    entries.extend(by_name(here, "mcpServers"));
                }
                (entries, Vec::new())
            }
        };
        let entries = entries.into_iter().map(|(name, entry)| (name, self.translated(entry, home))).collect();
        let mut named = crate::config::servers(entries, Origin::Yours);
        named.skipped.extend(skipped);
        // A command that's a path inside the app's own folder is found
        // from there, which rein can't tell: it would never start.
        for list in [&mut named.servers, &mut named.off] {
            let (inside, kept): (Vec<Server>, Vec<Server>) =
                std::mem::take(list).into_iter().partition(|server| server.local().is_some_and(|local| inside_the_app(&local.command)));
            *list = kept;
            named.skipped.extend(inside.into_iter().map(|server| {
                let command = server.local().map(|local| local.command.clone()).unwrap_or_default();
                Skipped { name: server.name, origin: Origin::Yours, why: format!("its command, {command}, is a path inside {}'s own folder, which rein can't tell", self.title()) }
            }));
        }
        Ok(named)
    }

    /// `entry` with what this app writes its own way written rein's way,
    /// and a command in your home folder (`~/bin/x`) where it is: rein
    /// doesn't start a server through a shell, which would say.
    fn translated(self, entry: Value, home: &Path) -> Value {
        let mut entry = strings(entry, &|text| self.words(text, home));
        if let Some(command) = entry.get_mut("command")
            && let Some(rest) = command.as_str().and_then(|command| command.strip_prefix("~/"))
        {
            *command = Value::String(home.join(rest).display().to_string());
        }
        // Gemini CLI's server reached at a URL says so with `httpUrl`.
        if self == App::GeminiCli
            && let Some(entry) = entry.as_object_mut()
            && let Some(url) = entry.remove("httpUrl")
        {
            entry.entry("url").or_insert(url);
        }
        entry
    }

    /// `text`, a word of a server's, with what this app writes its own way
    /// written rein's way: a variable of yours as `${NAME}`.
    fn words(self, text: &str, home: &Path) -> String {
        match self {
            App::Cursor | App::VsCode | App::Windsurf => editors(text, home),
            App::GeminiCli => bare_variables(text),
            App::ClaudeCode | App::ClaudeDesktop | App::Codex => text.to_owned(),
        }
    }
}

/// `value` with each string in it changed by `change`.
fn strings(value: Value, change: &dyn Fn(&str) -> String) -> Value {
    match value {
        Value::String(text) => Value::String(change(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(|item| strings(item, change)).collect()),
        Value::Object(fields) => Value::Object(fields.into_iter().map(|(name, value)| (name, strings(value, change))).collect()),
        other => other,
    }
}

/// The servers by name in `file`'s `key`.
fn by_name(file: &Value, key: &str) -> BTreeMap<String, Value> {
    file.get(key).and_then(Value::as_object).map(|servers| servers.iter().map(|(name, entry)| (name.clone(), entry.clone())).collect()).unwrap_or_default()
}

/// Whether `command` is a path from a folder of the app's own: `./x`, or
/// `x/y`, not on your PATH, nor from the root, nor a variable of yours.
fn inside_the_app(command: &str) -> bool {
    command.contains('/') && !command.starts_with('/') && !command.starts_with('$')
}

/// What Cursor, VS Code and Windsurf write in a server's words: `${env:NAME}`
/// for a variable of yours, `${input:id}` for what VS Code asks you for,
/// `${userHome}`, and `${workspaceFolder}`, the project, where rein starts
/// servers.
fn editors(text: &str, home: &Path) -> String {
    let (mut out, mut rest) = (String::new(), text);
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let inside = &rest[at + 2..];
        let Some(end) = inside.find('}') else {
            out.push_str(&rest[at..]);
            return out;
        };
        let name = &inside[..end];
        match name {
            "userHome" => out.push_str(&home.display().to_string()),
            "workspaceFolder" => out.push('.'),
            "pathSeparator" | "/" => out.push('/'),
            _ => {
                let name = name.strip_prefix("env:").map(str::to_owned).or_else(|| name.strip_prefix("input:").map(variable)).unwrap_or_else(|| name.to_owned());
                out.push_str(&format!("${{{name}}}"));
            }
        }
        rest = &inside[end + 1..];
    }
    out.push_str(rest);
    out
}

/// What VS Code asks you for, `github-token`, as a variable: `GITHUB_TOKEN`.
fn variable(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect()
}

/// Gemini CLI's `$NAME` as `${NAME}`; a `${NAME}` stays as it is.
fn bare_variables(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '$' && chars.peek().is_some_and(|c| c.is_ascii_alphabetic() || *c == '_') {
            out.push('{');
            while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
                out.push(c);
            }
            out.push('}');
        }
    }
    out
}

/// One server as Codex's `config.toml` has it, under `[mcp_servers.<name>]`.
#[derive(Deserialize)]
struct CodexServer {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Your variables it's given, by name.
    #[serde(default)]
    env_vars: Vec<String>,
    url: Option<String>,
    /// The variable of yours whose value is its bearer token.
    bearer_token_env_var: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    /// Headers, each given a variable of yours by name.
    #[serde(default)]
    env_http_headers: BTreeMap<String, String>,
    enabled: Option<bool>,
}

/// The servers in Codex's `config.toml`, as `mcpServers` entries, and
/// those it can't read, with why: one that can't be read leaves out only
/// itself.
fn codex(text: &str) -> Result<(BTreeMap<String, Value>, Vec<Skipped>), String> {
    let file: toml::Table = toml::from_str(text).map_err(|e| format!("isn't TOML rein can read: {}", e.to_string().lines().next().unwrap_or_default()))?;
    let (mut entries, mut skipped) = (BTreeMap::new(), Vec::new());
    let servers = file.get("mcp_servers").and_then(toml::Value::as_table).cloned().unwrap_or_default();
    for (name, server) in servers {
        let server: CodexServer = match server.try_into() {
            Ok(server) => server,
            Err(e) => {
                skipped.push(Skipped { name, origin: Origin::Yours, why: format!("can't be read: {}", e.to_string().lines().next().unwrap_or_default()) });
                continue;
            }
        };
        let mut env = server.env;
        for variable in server.env_vars {
            env.entry(variable.clone()).or_insert_with(|| format!("${{{variable}}}"));
        }
        let mut entry = json!({ "args": server.args, "env": env, "disabled": server.enabled == Some(false) });
        if let Some(command) = server.command {
            entry["command"] = json!(command);
        }
        if let Some(url) = server.url {
            let mut headers = server.http_headers;
            for (header, variable) in server.env_http_headers {
                headers.insert(header, format!("${{{variable}}}"));
            }
            if let Some(variable) = server.bearer_token_env_var {
                headers.insert("Authorization".into(), format!("Bearer ${{{variable}}}"));
            }
            entry["url"] = json!(url);
            entry["headers"] = json!(headers);
        }
        entries.insert(name, entry);
    }
    Ok((entries, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::command;
    use crate::config::{Reach, Remote};

    fn names(named: &Named) -> Vec<&str> {
        named.servers.iter().map(|server| server.name.as_str()).collect()
    }

    #[test]
    fn each_apps_servers_are_read_with_their_variables_written_reins_way() {
        let (home, root) = (Path::new("/home/me"), Path::new("/work/app"));
        let claude = r#"{ "numStartups": 3, "mcpServers": { "gh": { "type": "stdio", "command": "npx", "args": ["gh"], "env": { "T": "${GITHUB_TOKEN}" } },
                          "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" } },
                          "projects": { "/work/app": { "mcpServers": { "gh": { "command": "gh-local" }, "db": { "command": "uvx", "args": ["db-mcp"] } } },
                                        "/work/other": { "mcpServers": { "theirs": { "command": "x" } } } } }"#;
        let named = App::ClaudeCode.read(claude, home, root).unwrap();
        assert_eq!(names(&named), ["db", "gh", "linear"], "this project's, not another's");
        assert_eq!(command(&named.servers[1]).command, "gh-local", "this project's own wins its name, as in Claude Code");
        assert_eq!(named.servers[2].reach, Reach::Remote(Remote { url: "https://mcp.linear.app/mcp".into(), headers: BTreeMap::new() }));

        let cursor = r#"{ "mcpServers": { "fs": { "command": "npx", "args": ["fs", "${workspaceFolder}", "${userHome}/notes"], "env": { "K": "${env:FS_KEY}", "L": "${unknown}" } },
                                          "mine": { "command": "~/bin/tool" } } }"#;
        let named = App::Cursor.read(cursor, home, root).unwrap();
        assert_eq!(command(&named.servers[1]).command, "/home/me/bin/tool", "your home, where a shell would have put it");
        let fs = command(&named.servers[0]);
        assert_eq!(fs.args, ["fs", ".", "/home/me/notes"], "the project is where rein starts it");
        assert_eq!((fs.env["K"].as_str(), fs.env["L"].as_str()), ("${FS_KEY}", "${unknown}"));

        let vscode = r#"{ "inputs": [{ "id": "github-token", "type": "promptString", "password": true }],
                          "servers": { "gh": { "type": "stdio", "command": "docker", "args": ["run"], "env": { "T": "${input:github-token}" } } } }"#;
        assert_eq!(command(&App::VsCode.read(vscode, home, root).unwrap().servers[0]).env["T"], "${GITHUB_TOKEN}", "what VS Code asks for, rein asks for in dots");

        let gemini = r#"{ "theme": "x", "mcpServers": { "g": { "command": "node", "args": ["s.js"], "env": { "K": "$API_KEY", "M": "${MODE}", "P": "a$" } },
                          "web": { "httpUrl": "https://x.example/mcp" } } }"#;
        let named = App::GeminiCli.read(gemini, home, root).unwrap();
        let env = &command(&named.servers[0]).env;
        assert_eq!((env["K"].as_str(), env["M"].as_str(), env["P"].as_str()), ("${API_KEY}", "${MODE}", "a$"));
        assert_eq!(named.servers[1].reach, Reach::Remote(Remote { url: "https://x.example/mcp".into(), headers: BTreeMap::new() }), "httpUrl is its URL");

        let desktop = r#"{ "mcpServers": { "d": { "command": "uvx", "args": ["d"], "disabled": true } } }"#;
        assert_eq!(App::ClaudeDesktop.read(desktop, home, root).unwrap().off.len(), 1);
        assert!(App::Cursor.read("not json", home, root).is_err());
        assert_eq!(App::Windsurf.read("{}", home, root).unwrap(), Named::default());
    }

    #[test]
    fn codexs_servers_are_read_from_its_toml() {
        let (home, root) = (Path::new("/home/me"), Path::new("/work/app"));
        let toml = r#"
model = "gpt-6.1-sol"

[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
env_vars = ["CONTEXT7_API_KEY"]

[mcp_servers.docs]
command = 'uvx'
args = ["docs-mcp"]
enabled = false

[mcp_servers.docs.env]
LEVEL = "info"

[mcp_servers.figma]
url = "https://mcp.figma.com/mcp"
bearer_token_env_var = "FIGMA_TOKEN"
env_http_headers = { "X-Team" = "FIGMA_TEAM" }

[mcp_servers.broken]
command = 3

[mcp_servers.computer-use]
command = "./Codex Computer Use.app/Contents/MacOS/client"
enabled = false
"#;
        let named = App::Codex.read(toml, home, root).unwrap();
        assert_eq!(names(&named), ["context7", "figma"]);
        assert_eq!(command(&named.servers[0]).env["CONTEXT7_API_KEY"], "${CONTEXT7_API_KEY}", "a variable it's given by name is yours");
        assert_eq!((named.off[0].name.as_str(), command(&named.off[0]).env["LEVEL"].as_str()), ("docs", "info"), "enabled = false is off");
        let figma = BTreeMap::from([("Authorization".to_owned(), "Bearer ${FIGMA_TOKEN}".to_owned()), ("X-Team".to_owned(), "${FIGMA_TEAM}".to_owned())]);
        assert_eq!(named.servers[1].reach, Reach::Remote(Remote { url: "https://mcp.figma.com/mcp".into(), headers: figma }), "its token, a variable of yours");
        assert!(named.skipped.iter().any(|skipped| skipped.name == "broken" && skipped.why.starts_with("can't be read")), "{:?}", named.skipped);
        let inside = named.skipped.iter().find(|skipped| skipped.name == "computer-use").expect("a path inside Codex's folder is left out, though off");
        assert!(inside.why.contains("inside Codex's own folder"), "{}", inside.why);
        assert!(named.off.iter().all(|server| server.name != "computer-use"));
        assert!(App::Codex.read("[mcp_servers", home, root).unwrap_err().starts_with("isn't TOML"));
    }

    #[test]
    fn each_app_is_looked_for_where_it_keeps_your_servers() {
        let home = Path::new("/home/me");
        assert_eq!(App::Cursor.file(home, None), Path::new("/home/me/.cursor/mcp.json"));
        assert_eq!(App::Codex.file(home, Some(Path::new("/elsewhere"))), Path::new("/elsewhere/config.toml"));
        assert_eq!(App::ClaudeCode.file(home, None), Path::new("/home/me/.claude.json"));
        let desktop = App::ClaudeDesktop.file(home, None);
        assert!(desktop.ends_with("Claude/claude_desktop_config.json") && desktop.starts_with(home), "{}", desktop.display());
    }
}
