//! Slash commands: what you can type in a conversation besides a message.
//! The same names as Claude Code and Codex where they have one.

use rein_agent::permissions::Mode;
use rein_llm::Effort;

/// Every command: its name, what it takes, and what it does.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("/help", "", "list these commands"),
    ("/mode", "[trust|ask|edits|auto|read-only]", "show or change what runs without asking"),
    ("/sandbox", "[on|off]", "run commands in the sandbox, or as you'd run them, in any mode"),
    ("/permissions", "[add|remove rule]", "what asks first, or is refused: your rules, for every project, and this one's"),
    ("/model", "[id]", "list the models, or switch to one"),
    ("/login", "[provider]", "sign in to a provider, or see what pays for each"),
    ("/logout", "[provider]", "forget a provider's key (chatgpt: sign out)"),
    ("/effort", "[level|default]", "change how hard the model thinks"),
    ("/search", "[auto|off|model]", "choose which model searches the web for the model"),
    ("/skills", "", "your skills, your plugins' and the project's: pick one to use, turn off, keep for you to call, or turn on; /name calls one"),
    ("/hooks", "", "your hooks, rein's and Claude Code's: what each runs on and how it last did; pick one to turn it off or on"),
    ("/status", "", "the model, mode, context and conversation"),
    ("/mcp", "[add|import|remove|allow …]", "the MCP servers: add one by name from the MCP Registry, bring yours from other apps, or change one"),
    ("/compact", "[focus]", "make room in the conversation now"),
    ("/copy", "", "copy rein's last answer, as it wrote it"),
    ("/undo", "", "put back what rein did to the project's files since a turn you pick, commands' changes too"),
    ("/new", "", "start a fresh conversation (also /clear)"),
    ("/resume", "", "continue a saved conversation from this folder"),
    ("/peers", "", "the reins in your other terminals, and what each is doing"),
    ("/send", "[name] [message]", "write to another rein"),
    ("/delegate", "[name] [task]", "hand another rein a task; its result comes back here"),
    ("/spawn", "[name] [-m model] [--edits] [task]", "start a sub-agent in a new tab; its result comes back here"),
    ("/inbox", "", "what other reins sent that waits to be let in"),
    ("/name", "[name]", "what other reins call this one"),
    ("/workspace", "[name]", "the group of reins this one works with"),
    ("/exit", "", "leave (also /quit, Ctrl-D)"),
];

/// Other names for the same commands, as other harnesses spell them.
const ALIASES: &[(&str, &str)] = &[("/clear", "/new"), ("/quit", "/exit")];

#[derive(Debug, PartialEq, Eq)]
pub enum Slash {
    Help,
    /// `None` shows the mode.
    Mode(Option<Mode>),
    /// On or off; `None` asks which.
    Sandbox(Option<bool>),
    Permissions(Rules),
    Model(Option<String>),
    /// `None` lists the providers and what pays for each.
    Login(Option<String>),
    /// `/login openrouter <key>`: that key, for that provider. The line
    /// it came in is never shown again or kept.
    LoginWith(String, Key),
    /// `None` asks which.
    Logout(Option<String>),
    /// `None` asks which.
    Effort(Option<EffortSetting>),
    /// Who searches the web; `None` asks.
    Search(Option<crate::search::SearchWith>),
    /// The skills, to change how one's offered.
    Skills,
    /// The hooks, to turn one off or on.
    Hooks,
    Status,
    Mcp(Mcp),
    Compact(Option<String>),
    Copy,
    Undo,
    New,
    Resume,
    Peers,
    /// To whom, and what; either may still be asked for.
    Send(Option<String>, Option<String>),
    Delegate(Option<String>, Option<String>),
    Inbox,
    Spawn(Spawn),
    /// `None` says what it is.
    Name(Option<String>),
    Workspace(Option<String>),
    Exit,
}

/// What `/mcp` does.
#[derive(Debug, PartialEq, Eq)]
pub enum Mcp {
    /// Says how each server is doing.
    Show,
    /// `/mcp add <word>`: a server looked up in the MCP Registry, or one
    /// asked for when `None`.
    Find(Option<String>),
    /// `/mcp import`: your servers from other apps' settings.
    Import,
    /// `/mcp add`: these servers, as yours, and those that can't be, with
    /// why.
    Add(rein_mcp::Named),
    /// `/mcp remove <name>`.
    Remove(String),
    /// `/mcp allow <name> <tool …>`: these of its tools, or `*` for all,
    /// run without asking.
    Allow(String, Vec<String>),
}

impl Mcp {
    /// `add github npx -y …`, `remove github`, `allow github get_issue`.
    fn parse(rest: &str) -> Result<Mcp, Refused> {
        let (word, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let words: Vec<String> = rest.split_whitespace().map(str::to_owned).collect();
        match (word, words.as_slice()) {
            ("", _) => Ok(Mcp::Show),
            ("add", []) => Ok(Mcp::Find(None)),
            // A URL alone is a server reached there, named for its host.
            ("add", [url]) if url.starts_with("http://") || url.starts_with("https://") => {
                rein_mcp::config::added(rest).map(Mcp::Add).map_err(|e| Refused::MayHoldKey(format!("can't add that: {e}")))
            }
            // A word that may be a key isn't sent to the registry to look up.
            ("add", [word]) if rein_mcp::config::looks_like_a_key(word) => {
                Err(Refused::MayHoldKey("that looks like a key, not a server's name, so rein doesn't look it up".into()))
            }
            ("add", [word]) if !word.starts_with(['{', '"']) && !word.contains('=') => Ok(Mcp::Find(Some(word.clone()))),
            // What follows may hold a key, even when it can't be read.
            ("add", _) => rein_mcp::config::added(rest).map(Mcp::Add).map_err(|e| Refused::MayHoldKey(format!("can't add that: {e}"))),
            ("import", []) => Ok(Mcp::Import),
            ("import", _) => Err(Refused::Said("/mcp import takes nothing more: it looks for your servers in other apps' settings".into())),
            ("remove", [name]) => Ok(Mcp::Remove(name.clone())),
            ("remove", _) => Err(Refused::Said("/mcp remove takes a server's name".into())),
            ("allow", [name, tools @ ..]) if !tools.is_empty() => Ok(Mcp::Allow(name.clone(), tools.to_vec())),
            ("allow", _) => Err(Refused::Said("/mcp allow takes a server's name and its tools to run without asking, or * for all of them".into())),
            (other, _) => Err(Refused::Said(format!("/mcp takes add, import, remove or allow, not {other}; alone, it lists the servers"))),
        }
    }
}

/// What `/permissions` does.
#[derive(Debug, PartialEq, Eq)]
pub enum Rules {
    /// Lists every rule to pick one from, or add one.
    Show,
    /// `/permissions add ask git push`: one more of yours, as written.
    Add(String),
    /// `/permissions remove ask rm`: one of yours out.
    Remove(String),
}

impl Rules {
    fn parse(rest: &str) -> Result<Rules, Refused> {
        let (word, rule) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(word, rule)| (word, rule.trim()));
        match (word, rule) {
            ("", _) => Ok(Rules::Show),
            ("add", "") | ("remove", "") => Err(Refused::Said(format!("/permissions {word} takes a rule, like ask git push"))),
            ("add", rule) => Ok(Rules::Add(rule.to_owned())),
            ("remove", rule) => Ok(Rules::Remove(rule.to_owned())),
            // A rule alone is one to add.
            ("allow" | "ask" | "deny", _) => Ok(Rules::Add(rest.to_owned())),
            (other, _) => Err(Refused::Said(format!("/permissions takes add or remove and a rule, not {other}; alone, it lists the rules"))),
        }
    }
}

/// What `/spawn` asks for: what's not given is asked for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Spawn {
    pub role: Option<String>,
    pub task: Option<String>,
    pub model: Option<String>,
    /// It changes files: a worktree of its own.
    pub edits: bool,
}

impl Spawn {
    /// `tests -m claude-opus-5-5 --edits run the suite`: a name first, then
    /// any of the options, then the task.
    fn parse(rest: &str) -> Result<Spawn, String> {
        let mut spawn = Spawn::default();
        let mut words = rest.split_whitespace().peekable();
        if let Some(role) = words.next_if(|word| !word.starts_with('-')) {
            spawn.role = Some(role.trim_start_matches('@').to_owned());
        }
        while let Some(&word) = words.peek() {
            match word {
                "-m" | "--model" => {
                    words.next();
                    spawn.model = Some(words.next().ok_or("-m needs a model")?.to_owned());
                }
                "--edits" => {
                    words.next();
                    spawn.edits = true;
                }
                _ => break,
            }
        }
        let task = words.collect::<Vec<_>>().join(" ");
        spawn.task = (!task.is_empty()).then_some(task);
        Ok(spawn)
    }
}

/// Why a command can't be done as typed.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// `/login` with more than a provider's name and a key after it, or
    /// `/logout` with more than a name. What follows may be a key: the
    /// line isn't shown again or kept for ↑, and this doesn't say it back.
    TooManyWords(Account),
    /// Said in words, of a line that may hold a key, like `/mcp add`'s:
    /// it isn't shown again or kept, and this doesn't say what's in it.
    MayHoldKey(String),
    /// Anything else, said in words.
    Said(String),
}

/// Which of the commands about a provider's account it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    Login,
    Logout,
}

impl Refused {
    /// The line it was typed in may hold a secret, so it's never shown
    /// again or kept.
    pub fn hides_the_line(&self) -> bool {
        matches!(self, Self::TooManyWords(_) | Self::MayHoldKey(_))
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyWords(Account::Login) => f.write_str("/login takes a provider's name and, if you like, its key"),
            Self::TooManyWords(Account::Logout) => f.write_str("/logout takes only a provider's name"),
            Self::Said(words) | Self::MayHoldKey(words) => f.write_str(words),
        }
    }
}

/// A key typed after `/login`'s provider. It's never shown, `Debug`
/// included, which could put it in a log or a panic message.
#[derive(PartialEq, Eq)]
pub struct Key(String);

impl Key {
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(hidden)")
    }
}

/// What `/effort` sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSetting {
    /// Whatever the model does when not told.
    Default,
    Level(Effort),
}

impl EffortSetting {
    pub fn level(self) -> Option<Effort> {
        match self {
            Self::Default => None,
            Self::Level(effort) => Some(effort),
        }
    }
}

/// What `line` asks for: `None` when it's a message, not a command. A
/// line counts as a command when its first word looks like one (`/mode`),
/// so a message that starts with a path, like `/usr/bin is…`, stays a
/// message.
pub fn parse(line: &str) -> Option<Result<Slash, Refused>> {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let name = word.strip_prefix('/')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
        return None;
    }
    let word = ALIASES.iter().find(|(alias, _)| *alias == word).map_or(word, |(_, name)| *name);
    let rest = rest.trim();
    let arg = (!rest.is_empty()).then(|| rest.to_owned());
    // A provider's name is one word: whatever comes after it may be a key.
    let words: Vec<&str> = rest.split_whitespace().collect();
    Some(match word {
        "/help" => Ok(Slash::Help),
        "/mode" => match &arg {
            None => Ok(Slash::Mode(None)),
            Some(name) => name.parse().map(|m| Slash::Mode(Some(m))).map_err(Refused::Said),
        },
        "/sandbox" => match arg.as_deref() {
            None => Ok(Slash::Sandbox(None)),
            Some("on") => Ok(Slash::Sandbox(Some(true))),
            Some("off") => Ok(Slash::Sandbox(Some(false))),
            Some(other) => Err(Refused::Said(format!("/sandbox is on or off, not {other}"))),
        },
        "/permissions" => Rules::parse(rest).map(Slash::Permissions),
        "/model" => Ok(Slash::Model(arg)),
        "/login" => match words.as_slice() {
            [] => Ok(Slash::Login(None)),
            [provider] => Ok(Slash::Login(Some((*provider).to_owned()))),
            [provider, key] | [provider, "--key", key] => Ok(Slash::LoginWith((*provider).to_owned(), Key((*key).to_owned()))),
            _ => Err(Refused::TooManyWords(Account::Login)),
        },
        "/logout" => match words.as_slice() {
            [] => Ok(Slash::Logout(None)),
            [provider] => Ok(Slash::Logout(Some((*provider).to_owned()))),
            _ => Err(Refused::TooManyWords(Account::Logout)),
        },
        "/effort" => match arg.as_deref() {
            None => Ok(Slash::Effort(None)),
            Some("default") => Ok(Slash::Effort(Some(EffortSetting::Default))),
            Some(level) => level.parse().map(|e| Slash::Effort(Some(EffortSetting::Level(e)))).map_err(|e| Refused::Said(format!("{e}, or default"))),
        },
        "/skills" => Ok(Slash::Skills),
        "/hooks" => Ok(Slash::Hooks),
        "/search" => match arg.as_deref() {
            None => Ok(Slash::Search(None)),
            Some(with) => with.parse().map(|with| Slash::Search(Some(with))).map_err(Refused::Said),
        },
        "/status" => Ok(Slash::Status),
        "/mcp" => Mcp::parse(rest).map(Slash::Mcp),
        "/compact" => Ok(Slash::Compact(arg)),
        "/copy" => Ok(Slash::Copy),
        "/undo" => Ok(Slash::Undo),
        "/new" => Ok(Slash::New),
        "/resume" => Ok(Slash::Resume),
        "/peers" => Ok(Slash::Peers),
        "/send" => {
            let (to, text) = to_and_rest(rest);
            Ok(Slash::Send(to, text))
        }
        "/delegate" => {
            let (to, task) = to_and_rest(rest);
            Ok(Slash::Delegate(to, task))
        }
        "/inbox" => Ok(Slash::Inbox),
        "/spawn" => Spawn::parse(rest).map(Slash::Spawn).map_err(Refused::Said),
        "/name" => Ok(Slash::Name(arg)),
        "/workspace" => Ok(Slash::Workspace(arg)),
        "/exit" => Ok(Slash::Exit),
        other => Err(Refused::Said(format!("there's no {other} command; /help lists them"))),
    })
}

/// Whether `line` may hold a secret, like a key typed after `/login`'s
/// provider, or a server's added with its key: then it's never shown
/// again, or kept for ↑.
pub fn hides(line: &str) -> bool {
    match parse(line) {
        Some(Ok(Slash::LoginWith(..))) => true,
        // A server rein can't start may hold one where rein didn't look.
        Some(Ok(Slash::Mcp(Mcp::Add(named)))) => !named.skipped.is_empty() || named.servers.iter().any(rein_mcp::Server::may_hold_keys),
        Some(Err(refused)) => refused.hides_the_line(),
        _ => false,
    }
}

/// `line` as it may be shown while it waits for the end of a turn: with
/// the key it holds, if any, left out.
pub fn shown(line: &str) -> std::borrow::Cow<'_, str> {
    match parse(line) {
        Some(Ok(Slash::LoginWith(provider, _))) => format!("/login {provider} (key hidden)").into(),
        _ if hides(line) => format!("{} … (what may be a key hidden)", line.split_whitespace().take(2).collect::<Vec<_>>().join(" ")).into(),
        _ => line.into(),
    }
}

/// `api fix the tests` → the rein it's for, and what to tell it.
fn to_and_rest(rest: &str) -> (Option<String>, Option<String>) {
    let (to, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let to = to.trim_start_matches('@');
    ((!to.is_empty()).then(|| to.to_owned()), (!text.trim().is_empty()).then(|| text.trim().to_owned()))
}

/// The commands that start with what's typed so far, for the list under
/// the box. Only while the first word is still being typed.
pub fn matching(typed: &str) -> Vec<(&'static str, &'static str, &'static str)> {
    if !typed.starts_with('/') || typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS.iter().copied().filter(|(name, _, _)| name.starts_with(typed)).collect()
}

/// `/help`'s text.
pub fn help() -> Vec<String> {
    let width = COMMANDS.iter().map(|(name, args, _)| name.len() + 1 + args.len()).max().unwrap_or(0);
    COMMANDS
        .iter()
        .map(|(name, args, what)| {
            let usage = if args.is_empty() { (*name).to_owned() } else { format!("{name} {args}") };
            format!("{usage:<width$}  {what}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_commands_and_their_arguments() {
        assert_eq!(parse("/mode edits"), Some(Ok(Slash::Mode(Some(Mode::Edits)))));
        assert_eq!(parse("/sandbox"), Some(Ok(Slash::Sandbox(None))));
        assert_eq!(parse("/sandbox on"), Some(Ok(Slash::Sandbox(Some(true)))));
        assert_eq!(parse("/sandbox off"), Some(Ok(Slash::Sandbox(Some(false)))));
        assert_eq!(parse("/sandbox maybe"), Some(Err(Refused::Said("/sandbox is on or off, not maybe".into()))));
        assert_eq!(parse("  /permissions  "), Some(Ok(Slash::Permissions(Rules::Show))));
        assert_eq!(parse("/permissions add ask git  push"), Some(Ok(Slash::Permissions(Rules::Add("ask git  push".into())))));
        assert_eq!(parse("/permissions deny curl"), Some(Ok(Slash::Permissions(Rules::Add("deny curl".into())))));
        assert_eq!(parse("/permissions remove ask rm"), Some(Ok(Slash::Permissions(Rules::Remove("ask rm".into())))));
        assert!(matches!(parse("/permissions remove"), Some(Err(Refused::Said(_)))));
        assert!(matches!(parse("/permissions forget rm"), Some(Err(Refused::Said(_)))));
        assert_eq!(parse("/model gpt-6.1-terra"), Some(Ok(Slash::Model(Some("gpt-6.1-terra".into())))));
        assert_eq!(parse("/effort default"), Some(Ok(Slash::Effort(Some(EffortSetting::Default)))));
        assert_eq!(parse("/effort xhigh"), Some(Ok(Slash::Effort(Some(EffortSetting::Level(Effort::XHigh))))));
        assert_eq!(parse("/effort extreme"), Some(Err(Refused::Said("no effort called extreme; use off, minimal, low, medium, high, xhigh or max, or default".into()))));
        assert_eq!(parse("/compact keep the test output"), Some(Ok(Slash::Compact(Some("keep the test output".into())))));
        assert_eq!(parse("/clear"), Some(Ok(Slash::New)));
        assert_eq!(parse("/quit"), Some(Ok(Slash::Exit)));
        assert_eq!(parse("/login"), Some(Ok(Slash::Login(None))));
        assert_eq!(parse("/login anthropic"), Some(Ok(Slash::Login(Some("anthropic".into())))));
        assert_eq!(parse("/logout openrouter"), Some(Ok(Slash::Logout(Some("openrouter".into())))));
        assert_eq!(parse("/logout"), Some(Ok(Slash::Logout(None))));
        assert_eq!(parse("/send api run the tests"), Some(Ok(Slash::Send(Some("api".into()), Some("run the tests".into())))));
        assert_eq!(parse("/delegate @web"), Some(Ok(Slash::Delegate(Some("web".into()), None))));
        assert_eq!(parse("/send"), Some(Ok(Slash::Send(None, None))));
        assert_eq!(parse("/name review"), Some(Ok(Slash::Name(Some("review".into())))));
        let spawn = |role: Option<&str>, model: Option<&str>, edits, task: Option<&str>| {
            Some(Ok(Slash::Spawn(Spawn { role: role.map(Into::into), task: task.map(Into::into), model: model.map(Into::into), edits })))
        };
        assert_eq!(parse("/spawn tests run the suite"), spawn(Some("tests"), None, false, Some("run the suite")));
        assert_eq!(parse("/spawn review -m claude-opus-5-5 --edits fix it"), spawn(Some("review"), Some("claude-opus-5-5"), true, Some("fix it")));
        assert_eq!(parse("/spawn"), spawn(None, None, false, None));
        assert_eq!(parse("/spawn docs"), spawn(Some("docs"), None, false, None));
        assert_eq!(parse("/spawn x -m"), Some(Err(Refused::Said("-m needs a model".into()))));
    }

    #[test]
    fn says_what_went_wrong() {
        assert_eq!(parse("/mode yolo"), Some(Err(Refused::Said("no mode called yolo; use trust, ask, edits, auto or read-only".into()))));
        assert_eq!(parse("/frobnicate"), Some(Err(Refused::Said("there's no /frobnicate command; /help lists them".into()))));
    }

    #[test]
    fn a_key_after_logins_provider_is_taken_and_never_shown() {
        for line in ["/login openrouter sk-or-v1-FAKE", "/login  anthropic --key sk-ant-FAKE "] {
            let Some(Ok(Slash::LoginWith(provider, key))) = parse(line) else { panic!("{line} is a key for a provider") };
            assert!(["openrouter", "anthropic"].contains(&provider.as_str()) && key.into_inner().contains("FAKE"));
            assert!(hides(line), "{line} is neither shown again nor kept");
            assert!(!shown(line).contains("FAKE") && shown(line).ends_with("(key hidden)"), "{}", shown(line));
            assert!(!format!("{:?}", parse(line)).contains("FAKE"), "nor in Debug");
        }
        for (line, account) in [("/login openrouter sk-or-v1-FAKE and more", Account::Login), ("/logout openrouter sk-or-v1-FAKE", Account::Logout)] {
            let refused = parse(line).and_then(Result::err).unwrap_or_else(|| panic!("{line} is turned down"));
            assert_eq!(refused, Refused::TooManyWords(account));
            assert!(!refused.to_string().contains("FAKE"), "{refused}");
            assert!(hides(line), "{line} is neither shown again nor kept");
        }
        for line in ["/login openrouter", "/login", "/logout anthropic", "/model gpt-6.1-terra extra", "/frobnicate sk-FAKE", "fix it"] {
            assert!(!hides(line), "{line} is shown and kept as usual");
        }
    }

    #[test]
    fn mcp_adds_removes_and_allows() {
        let Some(Ok(Slash::Mcp(Mcp::Add(named)))) = parse("/mcp add github GITHUB_TOKEN=$GITHUB_TOKEN npx -y gh") else { panic!("a server to add") };
        let args = named.servers[0].local().map(|local| local.args.clone()).unwrap_or_default();
        assert_eq!((named.servers[0].name.as_str(), args.as_slice()), ("github", ["-y".to_owned(), "gh".to_owned()].as_slice()));
        assert_eq!(parse("/mcp"), Some(Ok(Slash::Mcp(Mcp::Show))));
        assert_eq!(parse("/mcp remove github"), Some(Ok(Slash::Mcp(Mcp::Remove("github".into())))));
        assert_eq!(parse("/mcp allow github get_issue *"), Some(Ok(Slash::Mcp(Mcp::Allow("github".into(), vec!["get_issue".into(), "*".into()])))));
        assert_eq!(parse("/mcp add playwright"), Some(Ok(Slash::Mcp(Mcp::Find(Some("playwright".into()))))), "one word is looked up");
        assert_eq!(parse("/mcp add"), Some(Ok(Slash::Mcp(Mcp::Find(None)))), "nothing: asked what to look up");
        assert_eq!(parse("/mcp import"), Some(Ok(Slash::Mcp(Mcp::Import))));
        assert!(
            matches!(parse("/mcp add https://mcp.linear.app/mcp"), Some(Ok(Slash::Mcp(Mcp::Add(named)))) if named.servers[0].name == "linear"),
            "a URL alone, named for its host"
        );
        assert!(matches!(parse(r#"/mcp add {"mcpServers":{"x":{"command":"x"}}}"#), Some(Ok(Slash::Mcp(Mcp::Add(_))))), "JSON in one word is still JSON");
        let key = parse("/mcp add ghp_FAKEaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").and_then(Result::err).expect("a key isn't looked up");
        assert!(key.hides_the_line() && !key.to_string().contains("FAKE"), "{key}");
        for (line, why) in [("/mcp import x", "takes nothing"), ("/mcp remove", "a server's name"), ("/mcp allow github", "its tools"), ("/mcp start x", "not start")] {
            let refused = parse(line).and_then(Result::err).unwrap_or_else(|| panic!("{line} is turned down"));
            assert!(refused.to_string().contains(why), "{line}: {refused}");
        }
    }

    #[test]
    fn a_server_added_with_its_key_is_neither_shown_again_nor_kept() {
        for line in [
            "/mcp add github GITHUB_TOKEN=ghp_FAKE npx -y gh",
            "/mcp add s npx s --api-key sk-FAKE",
            r#"/mcp add {"mcpServers": {"gh": {"command": "npx", "env": {"GITHUB_TOKEN": "ghp_FAKE"}}}}"#,
            r#"/mcp add {"mcpServers": {"linear": {"url": "https://x.dev/mcp", "headers": {"Authorization": "Bearer FAKE"}}}}"#,
            "/mcp add gh GITHUB_TOKEN='ghp_FAKE",
        ] {
            assert!(hides(line), "{line}");
            assert!(!shown(line).contains("FAKE"), "{}", shown(line));
            assert!(!format!("{:?}", parse(line)).contains("FAKE"), "{:?}", parse(line));
            if let Some(Err(refused)) = parse(line) {
                assert!(!refused.to_string().contains("FAKE"), "{refused}");
            }
        }
        for line in ["/mcp add github GITHUB_TOKEN=$GITHUB_TOKEN npx -y gh", "/mcp add docs uvx docs-mcp", "/mcp remove github", "/mcp"] {
            assert!(!hides(line), "{line} is shown and kept as usual");
        }
    }

    #[test]
    fn messages_that_start_with_a_slash_stay_messages() {
        assert_eq!(parse("/usr/bin/env is missing"), None);
        assert_eq!(parse("fix /mode please"), None);
        assert_eq!(parse("/"), None);
    }

    #[test]
    fn suggests_while_the_name_is_typed() {
        let names = |typed| matching(typed).into_iter().map(|(name, _, _)| name).collect::<Vec<_>>();
        assert_eq!(names("/mo"), ["/mode", "/model"]);
        assert_eq!(names("/lo"), ["/login", "/logout"]);
        assert_eq!(names("/co"), ["/compact", "/copy"]);
        assert_eq!(names("/"), COMMANDS.iter().map(|c| c.0).collect::<Vec<_>>());
        assert!(names("/mode ").is_empty());
        assert!(names("hello").is_empty());
        assert!(help()[1].starts_with("/mode [trust|ask|edits|auto|read-only]"));
    }
}
