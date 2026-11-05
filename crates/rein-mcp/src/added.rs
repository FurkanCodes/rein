//! A server as `/mcp add` is given it: as you'd start it in a shell, its URL,
//! or the JSON a server's README gives.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::config::{Named, Origin, servers};

/// The servers `/mcp add` was given, as yours. Either the snippet a
/// server's README gives, as it is (`{"mcpServers": {…}}`, or only
/// `"github": {…}`, or a name and then `{"command": …}`), or a server as
/// you'd type it in a shell, its name first:
///
/// `github GITHUB_PERSONAL_ACCESS_TOKEN=$GITHUB_TOKEN npx -y @modelcontextprotocol/server-github`
///
/// or a server reached at a URL, its name first or taken from its host,
/// its headers before it:
///
/// `linear https://mcp.linear.app/mcp`, `--header "X-Key: $KEY" https://x.example/mcp`
///
/// `--access outside` before the command gives it other walls. `-e
/// NAME=value`, `--transport http` and a `--` before the command work too,
/// as Claude Code's and Codex's `mcp add` take them.
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
    const USAGE: &str = "/mcp add takes a name, then the command that starts the server or the URL it's at: /mcp add github npx -y @modelcontextprotocol/server-github";
    let mut words = words(text)?.into_iter();
    let (mut name, mut access, mut env, mut headers, mut sse) = (None, None, BTreeMap::new(), BTreeMap::new(), false);
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
            "-H" | "--header" => {
                let given = words.next().unwrap_or_default();
                let (key, value) = header(&given).ok_or_else(|| format!("{word} takes 'Name: value'"))?;
                headers.insert(key, value);
            }
            "-t" | "--transport" => match words.next().as_deref() {
                Some("http" | "stdio") => sse = false,
                Some("sse") => sse = true,
                _ => return Err(format!("{word} takes http, sse or stdio")),
            },
            "--" => break words.next().ok_or(USAGE)?,
            _ if word.starts_with('-') => {
                return Err(format!("/mcp add doesn't know {word}: before the command it takes --access and -e NAME=value, and before a URL --header 'Name: value'"));
            }
            // A URL first is named for its host.
            _ if name.is_none() && is_url(&word) => break word,
            _ if name.is_none() => name = Some(word),
            _ => match setting(&word) {
                Some((key, value)) => {
                    env.insert(key, value);
                }
                None => break word,
            },
        }
    };
    let rest: Vec<String> = words.collect();
    if !is_url(&command) {
        if !headers.is_empty() || sse {
            return Err("--header and --transport sse are for a server reached at a URL; a command's takes -e NAME=value".into());
        }
        let name = name.ok_or(USAGE)?;
        let entry = json!({ "command": command, "args": rest, "env": env, "access": access });
        return Ok(servers(BTreeMap::from([(name, entry)]), Origin::Yours));
    }
    if !rest.is_empty() {
        return Err("a URL takes nothing after it: its headers go before it, as --header 'Name: value'".into());
    }
    if !env.is_empty() || access.is_some() {
        return Err("a server reached at a URL runs none of its code here: it takes headers (--header 'Name: value'), not -e or --access".into());
    }
    let name = name.unwrap_or_else(|| name_of(&command));
    let entry = json!({ "type": if sse { "sse" } else { "http" }, "url": command, "headers": headers });
    Ok(servers(BTreeMap::from([(name, entry)]), Origin::Yours))
}

/// Whether `word` is a URL a server may be reached at.
fn is_url(word: &str) -> bool {
    word.starts_with("https://") || word.starts_with("http://")
}

/// `Name: value`, as a header is written.
fn header(text: &str) -> Option<(String, String)> {
    let (name, value) = text.split_once(':')?;
    let name = name.trim();
    let fits = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c));
    fits.then(|| (name.to_owned(), value.trim().to_owned()))
}

/// What to call a server at `url`, from its host: `mcp.linear.app` is
/// `linear`, the name before the last part.
fn name_of(url: &str) -> String {
    let host = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = host.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let host = host.split(':').next().unwrap_or_default().to_lowercase();
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    let numbers = labels.iter().all(|label| label.chars().all(|c| c.is_ascii_digit()));
    let name = match labels.as_slice() {
        _ if numbers || host == "localhost" => "local",
        [.., name, _top] => name,
        [only] => only,
        [] => "server",
    };
    let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let name = name.trim_matches('-').to_owned();
    if crate::config::check_name(&name).is_ok() { name } else { "server".into() }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::command;
    use crate::config::{Reach, Remote, Server, Walled};
    use rein_sandbox::Access;

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
        assert_eq!((github.name.as_str(), command(&github).command.as_str(), github.origin), ("github", "npx", Origin::Yours));
        assert_eq!(command(&github).args, ["-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(command(&github).env["GITHUB_PERSONAL_ACCESS_TOKEN"], "${GITHUB_TOKEN}", "a shell's $NAME is filled in as it starts");
        assert_eq!(command(&github).access, Walled::ByMode, "as the mode has it");
        let quoted = one(r#"docs --access look -e "ROOT=my docs" -- uvx docs-mcp --title 'it'\''s $HOME' "a \"b\"" c\ d ${X:-y}"#);
        assert_eq!(command(&quoted).access, Walled::Set(Access::Look));
        assert_eq!(command(&quoted).env["ROOT"], "my docs");
        assert_eq!(command(&quoted).args, ["docs-mcp", "--title", "it's $HOME", r#"a "b""#, "c d", "${X:-y}"], "single quotes leave a $ as it is");
        assert_eq!(command(&one("x --access=outside A=1 B= sh -c 'exec x'")).env.len(), 2);
        assert_eq!(command(&one("x --access=outside sh")).access, Walled::Set(Access::Outside));
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
        assert_eq!((named.servers.len(), named.servers[0].name.as_str(), named.servers[1].name.as_str()), (2, "github", "linear"));
        assert_eq!(command(&one(r#""github": { "command": "npx", "args": ["-y", "gh"] },"#)).args, ["-y", "gh"], "the part to paste in, a comma after it");
        assert_eq!(command(&one(r#"{ "github": { "command": "npx" } }"#)).command, "npx");
        assert_eq!(one(r#"{ "servers": { "gh": { "type": "stdio", "command": "npx" } } }"#).name, "gh", "VS Code's shape");
        assert_eq!(command(&one("gh {\n  \"command\": \"npx\",\n  \"access\": \"look\"\n}")).access, Walled::Set(Access::Look));
        assert!(added(r#"{ "command": "npx" }"#).unwrap_err().contains("say what to call it"));
        assert!(added("{ not json").unwrap_err().contains("isn't JSON"));
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

    #[test]
    fn a_server_at_a_url_is_added_by_its_url_named_for_its_host_its_headers_before_it() {
        let remote = |server: &Server| match &server.reach {
            Reach::Remote(remote) => remote.clone(),
            Reach::Local(_) => panic!("{} is a command", server.name),
        };
        let old = one("asana --transport sse https://mcp.asana.com/sse");
        assert_eq!((old.name.as_str(), remote(&old).wire), ("asana", crate::config::Wire::Sse), "the SSE before Streamable HTTP");
        let linear = one("https://mcp.linear.app/mcp");
        assert_eq!(
            (linear.name.as_str(), remote(&linear)),
            ("linear", Remote { url: "https://mcp.linear.app/mcp".into(), headers: BTreeMap::new(), wire: crate::config::Wire::Http })
        );
        let keyed = one(r#"ctx --transport http --header "CONTEXT7_API_KEY: $CONTEXT7_KEY" -H 'X-Mode:fast' https://mcp.context7.com/mcp"#);
        assert_eq!(keyed.name, "ctx");
        assert_eq!(remote(&keyed).headers, BTreeMap::from([("CONTEXT7_API_KEY".to_owned(), "${CONTEXT7_KEY}".to_owned()), ("X-Mode".to_owned(), "fast".to_owned())]));
        assert!(keyed.may_hold_keys(), "a value written in may be a key, as in a command's environment");
        assert!(!one(r#"ctx -H "K: $KEY" https://x.example/mcp"#).may_hold_keys(), "a variable of yours isn't");
        assert!(one("x -H 'Authorization: Bearer FAKE-token' https://x.example/mcp").may_hold_keys(), "a key written in");
        for (url, name) in [
            ("http://localhost:3000/mcp", "local"),
            ("http://127.0.0.1:8080", "local"),
            ("https://api.githubcopilot.com/mcp/", "githubcopilot"),
            ("https://me:pw@mcp.notion.com/mcp", "notion"),
        ] {
            assert_eq!(one(url).name, name, "{url}");
        }
        assert_eq!(
            one(r#"{ "mcpServers": { "l": { "url": "https://mcp.linear.app/mcp", "headers": { "A": "b" } } } }"#).entry(),
            serde_json::json!({ "type": "http", "url": "https://mcp.linear.app/mcp", "headers": { "A": "b" } })
        );
        for (text, why) in [
            ("x https://x.example/mcp extra", "takes nothing after it"),
            ("x -e A=1 https://x.example/mcp", "runs none of its code here"),
            ("x --access outside https://x.example/mcp", "runs none of its code here"),
            ("x -H nope https://x.example/mcp", "takes 'Name: value'"),
            ("x -H 'A: b' npx s", "--header and --transport sse are for a server reached at a URL"),
            ("x --transport sse npx s", "for a server reached at a URL"),
        ] {
            let e = added(text).err().unwrap_or_else(|| panic!("{text} isn't a server"));
            assert!(e.contains(why), "{text}: {e}");
        }
    }
}
