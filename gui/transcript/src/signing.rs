//! The ways to sign in, as `rein serve` lists them when a new conversation
//! would find you signed in to nothing that runs its model.

use serde_json::Value;

/// What `session/new` is turned down with when you're signed in to nothing:
/// a code of its own, so nothing reads the words to tell.
pub const SIGNED_OUT: i64 = -32002;

/// A way to sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    Chatgpt,
    ClaudeCode,
    Key,
    Local,
}

impl Way {
    /// What `rein serve` calls it.
    pub const fn id(self) -> &'static str {
        match self {
            Way::Chatgpt => "chatgpt",
            Way::ClaudeCode => "claude-code",
            Way::Key => "key",
            Way::Local => "local",
        }
    }

    fn of(id: &str) -> Option<Way> {
        [Way::Chatgpt, Way::ClaudeCode, Way::Key, Way::Local].into_iter().find(|way| way.id() == id)
    }
}

/// A way to sign in, worded for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub way: Way,
    pub name: &'static str,
    pub says: String,
    /// What its button says.
    pub action: &'static str,
}

/// The ways in an error's `data`, in the order given. One rein doesn't
/// know is left out: there's nothing here to say about it.
pub fn ways(data: &Value) -> Vec<Offered> {
    let Some(ways) = data["ways"].as_array() else { return Vec::new() };
    ways.iter().filter_map(|one| Some(worded(Way::of(one["way"].as_str()?)?, one))).collect()
}

fn worded(way: Way, one: &Value) -> Offered {
    let names: Vec<&str> = one["providers"].as_array().map(Vec::as_slice).unwrap_or_default().iter().filter_map(Value::as_str).collect();
    match way {
        Way::Chatgpt if one["signedIn"] == true => {
            Offered { way, name: "Your ChatGPT plan", says: "Signed in, with no model it can run here yet".to_owned(), action: "Sign in again" }
        }
        Way::Chatgpt => Offered { way, name: "Your ChatGPT plan", says: "OpenAI’s own sign-in for open-source apps".to_owned(), action: "Sign in" },
        Way::ClaudeCode => Offered { way, name: "Your Claude plan", says: "Through Anthropic’s own CLI, once it’s signed in".to_owned(), action: "Use Claude Code" },
        Way::Key => Offered { way, name: "An API key", says: listed(&names, 3), action: "Add a key" },
        Way::Local => Offered { way, name: "On this computer", says: format!("{}, no key needed", listed(&names, 2)), action: "Look for them" },
    }
}

/// The first `shown` of `names`, and how many more: `Anthropic, Gemini,
/// OpenRouter and 10 more`.
fn listed(names: &[&str], shown: usize) -> String {
    match names.len() {
        0 => "Any provider rein knows".to_owned(),
        count if count <= shown => match names {
            [one] => (*one).to_owned(),
            [first @ .., last] => format!("{} and {last}", first.join(", ")),
            [] => String::new(),
        },
        count => format!("{} and {} more", names[..shown].join(", "), count - shown),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_ways_are_worded_in_the_order_given_and_one_unknown_is_left_out() {
        let data = json!({ "ways": [
            { "way": "chatgpt", "signedIn": false },
            { "way": "claude-code" },
            { "way": "key", "providers": ["Anthropic", "Gemini", "OpenRouter", "Groq", "Mistral"] },
            { "way": "local", "providers": ["Ollama", "LM Studio"] },
            { "way": "carrier-pigeon" },
        ] });
        let offered = ways(&data);
        assert_eq!(offered.iter().map(|one| one.way).collect::<Vec<_>>(), [Way::Chatgpt, Way::ClaudeCode, Way::Key, Way::Local]);
        assert_eq!(offered[2].says, "Anthropic, Gemini, OpenRouter and 2 more");
        assert_eq!(offered[3].says, "Ollama and LM Studio, no key needed");
        assert_eq!(offered[0].action, "Sign in");
        assert!(ways(&json!(null)).is_empty());
    }

    #[test]
    fn a_short_list_is_said_whole() {
        assert_eq!(listed(&["Anthropic"], 3), "Anthropic");
        assert_eq!(listed(&["Anthropic", "Gemini", "Groq"], 3), "Anthropic, Gemini and Groq");
        assert_eq!(listed(&[], 3), "Any provider rein knows");
    }
}
