//! Who serves a model: a provider is a server, the format it speaks, and
//! where its key comes from. rein knows the common ones, and you can add
//! your own, or point one of rein's elsewhere, in `~/.config/rein/providers`:
//!
//! ```text
//! # A server on this computer that speaks Chat Completions.
//! [local]
//! api = chat
//! url = http://localhost:8080/v1
//! context = 131072
//!
//! # OpenRouter, through a proxy, with the key in another variable.
//! [openrouter]
//! url = https://proxy.example.com/openrouter/v1
//! key_env = MY_OPENROUTER_KEY
//! ```
//!
//! The model's name says which provider serves it: `openrouter/qwen/qwen3-coder`,
//! `ollama/qwen3:8b`, or just `claude-opus-5-5`, since some models' own
//! names say where they live.

use crate::Api;
use crate::chat::Quirks;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// What rein calls it, before the `/` in a model's name.
    pub name: String,
    /// The name people know it by, for messages.
    pub title: String,
    pub api: Api,
    pub base_url: String,
    /// The environment variables its key may be in, the first set one winning.
    pub key_env: Vec<String>,
    /// A variable for a gateway's own token, sent as a bearer token instead
    /// of a key, as `ANTHROPIC_AUTH_TOKEN` is.
    pub token_env: Option<String>,
    /// It can be paid for with a ChatGPT plan (`rein login`), which comes
    /// first when you've signed in.
    pub chatgpt: bool,
    /// What to `GET`, after the base URL, to tell whether a key works: a
    /// path the server only answers for a good key.
    pub key_check: String,
    /// Where its official browser sign-in starts, for a provider that
    /// makes you a key that way (OpenRouter's OAuth with PKCE).
    pub sign_in_url: Option<String>,
    /// Whether it needs a key at all: a server on your own computer doesn't.
    pub needs_key: bool,
    /// The header the key goes in; `None` for `Authorization: Bearer`.
    pub key_header: Option<&'static str>,
    /// An environment variable that points it at another server, like a proxy.
    pub url_env: Option<String>,
    /// A model to use when none is named and only this provider has a key.
    pub default_model: Option<String>,
    /// Where to get a key, for when there's none.
    pub keys_url: Option<String>,
    /// How many tokens its models take, when its model list doesn't say.
    pub context: Option<u64>,
    pub quirks: Quirks,
    /// Set up or changed in your providers file.
    pub yours: bool,
}

impl Provider {
    /// A provider that speaks `api` at `base_url`, with a key in `key_env`.
    fn new(name: &str, title: &str, api: Api, base_url: &str, key_env: &[&str]) -> Provider {
        Provider {
            name: name.into(),
            title: title.into(),
            api,
            base_url: base_url.into(),
            key_env: key_env.iter().map(|v| (*v).to_owned()).collect(),
            token_env: None,
            chatgpt: false,
            key_check: "models".into(),
            sign_in_url: None,
            needs_key: !key_env.is_empty(),
            key_header: key_header(api),
            url_env: None,
            default_model: None,
            keys_url: None,
            context: None,
            quirks: Quirks::default(),
            yours: false,
        }
    }

    fn keys_at(mut self, url: &str) -> Provider {
        self.keys_url = Some(url.into());
        self
    }
}

/// Where each format's own servers want the key.
fn key_header(api: Api) -> Option<&'static str> {
    match api {
        Api::Anthropic => Some("x-api-key"),
        Api::Gemini => Some("x-goog-api-key"),
        Api::Responses | Api::Chat => None,
    }
}

/// The providers rein knows without being told.
pub fn builtin() -> Vec<Provider> {
    let chat = |name, title, url, key| Provider::new(name, title, Api::Chat, url, &[key]);
    let local = |name, title, url| Provider::new(name, title, Api::Chat, url, &[]);
    vec![
        Provider {
            url_env: Some("OPENAI_BASE_URL".into()),
            chatgpt: true,
            default_model: Some("gpt-6.1-sol".into()),
            ..Provider::new("openai", "OpenAI", Api::Responses, crate::openai::API_BASE, &["OPENAI_API_KEY"]).keys_at("https://platform.openai.com/api-keys")
        },
        Provider {
            url_env: Some("ANTHROPIC_BASE_URL".into()),
            token_env: Some("ANTHROPIC_AUTH_TOKEN".into()),
            default_model: Some("claude-opus-5-5".into()),
            ..Provider::new("anthropic", "Anthropic", Api::Anthropic, crate::anthropic::API_BASE, &["ANTHROPIC_API_KEY"]).keys_at("https://platform.claude.com/settings/keys")
        },
        Provider { default_model: Some("sonnet".into()), needs_key: true, ..Provider::new("claude-code", "Claude Code CLI", Api::Anthropic, "claude-code", &[]) },
        Provider {
            url_env: Some("GOOGLE_GEMINI_BASE_URL".into()),
            default_model: Some("gemini-3.8-flash".into()),
            ..Provider::new("gemini", "Google Gemini", Api::Gemini, crate::gemini::API_BASE, &["GEMINI_API_KEY", "GOOGLE_API_KEY"]).keys_at("https://aistudio.google.com/apikey")
        },
        Provider {
            quirks: Quirks { effort_object: true, ..Quirks::default() },
            // Its model list is public: only this says whether a key works.
            key_check: "key".into(),
            sign_in_url: Some(crate::openrouter::AUTH_URL.into()),
            ..chat("openrouter", "OpenRouter", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY").keys_at("https://openrouter.ai/keys")
        },
        chat("groq", "Groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY").keys_at("https://console.groq.com/keys"),
        chat("deepseek", "DeepSeek", "https://api.deepseek.com", "DEEPSEEK_API_KEY").keys_at("https://platform.deepseek.com/api_keys"),
        chat("xai", "xAI", "https://api.x.ai/v1", "XAI_API_KEY").keys_at("https://console.x.ai"),
        Provider {
            quirks: Quirks { short_ids: true, tool_names: true, ..Quirks::default() },
            ..chat("mistral", "Mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY").keys_at("https://console.mistral.ai/api-keys")
        },
        chat("cerebras", "Cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY").keys_at("https://cloud.cerebras.ai"),
        chat("together", "Together AI", "https://api.together.ai/v1", "TOGETHER_API_KEY").keys_at("https://api.together.ai/settings/api-keys"),
        chat("fireworks", "Fireworks AI", "https://api.fireworks.ai/inference/v1", "FIREWORKS_API_KEY").keys_at("https://fireworks.ai/account/api-keys"),
        chat("moonshot", "Moonshot AI", "https://api.moonshot.ai/v1", "MOONSHOT_API_KEY").keys_at("https://platform.moonshot.ai/console/api-keys"),
        chat("zai", "Z.ai", "https://api.z.ai/api/paas/v4", "ZAI_API_KEY").keys_at("https://z.ai/manage-apikey/apikey-list"),
        local("ollama", "Ollama", "http://localhost:11434/v1"),
        local("lmstudio", "LM Studio", "http://localhost:1234/v1"),
    ]
}

/// rein's providers, with yours from a providers file (`text`) added and
/// changed. A line that doesn't make sense is an error, not a surprise later.
pub fn with_file(text: &str) -> Result<Vec<Provider>, String> {
    let mut providers = builtin();
    let mut current: Option<usize> = None;
    let mut new: Vec<usize> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let at = |message: String| format!("line {}: {message}", n + 1);
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            if name.is_empty() || name.contains('/') || name.contains(char::is_whitespace) {
                return Err(at(format!("[{name}] isn't a name rein can use: one word, no slashes")));
            }
            current = Some(match providers.iter().position(|p| p.name == name) {
                Some(i) => i,
                None => {
                    providers.push(Provider::new(name, name, Api::Chat, "", &[]));
                    new.push(providers.len() - 1);
                    providers.len() - 1
                }
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { return Err(at("write it as key = value, under a [provider]".into())) };
        let Some(i) = current else { return Err(at("settings go under a [provider] line".into())) };
        let (key, value) = (key.trim(), value.trim().trim_matches('"').trim());
        let provider = &mut providers[i];
        provider.yours = true;
        match key {
            "api" => {
                provider.api = Api::parse(value).ok_or_else(|| at(format!("no API called {value}; use responses, anthropic, chat or gemini")))?;
                provider.key_header = key_header(provider.api);
            }
            "url" => provider.base_url = value.trim_end_matches('/').to_owned(),
            "key_env" => {
                provider.key_env = value.split([',', ' ']).filter(|v| !v.is_empty()).map(str::to_owned).collect();
                provider.needs_key = !provider.key_env.is_empty();
            }
            "title" => provider.title = value.to_owned(),
            "model" => provider.default_model = Some(value.to_owned()),
            "context" => provider.context = Some(value.parse().map_err(|_| at(format!("context should be a number of tokens, not {value}")))?),
            other => return Err(at(format!("unknown setting {other}; use api, url, key_env, title, model or context"))),
        }
    }
    for i in new {
        if providers[i].base_url.is_empty() {
            return Err(format!("[{}] needs a url", providers[i].name));
        }
    }
    Ok(providers)
}

/// The provider a model's own name points to, when it's one with a
/// family name of its own.
pub fn guess(model: &str) -> Option<&'static str> {
    let families: &[(&[&str], &str)] = &[
        (&["claude-"], "anthropic"),
        (&["gpt-", "o1", "o3", "o4", "codex-", "chatgpt-"], "openai"),
        (&["gemini-", "gemma-"], "gemini"),
        (&["grok-"], "xai"),
        (&["deepseek-"], "deepseek"),
        (&["mistral-", "codestral", "devstral", "magistral"], "mistral"),
        (&["kimi-"], "moonshot"),
        (&["glm-"], "zai"),
    ];
    families.iter().find(|(prefixes, _)| prefixes.iter().any(|p| model.starts_with(p))).map(|(_, provider)| *provider)
}

/// Which provider serves `spec`, and the model's ID there. `spec` is
/// `provider/model`, or a model whose name says its provider; anything else
/// is OpenAI's, as it was before rein spoke to anyone else (and as servers
/// behind `OPENAI_BASE_URL` expect).
pub fn resolve<'a>(spec: &str, providers: &'a [Provider]) -> Option<(&'a Provider, String)> {
    let spec = spec.trim();
    if let Some((name, model)) = spec.split_once('/')
        && let Some(provider) = providers.iter().find(|p| p.name == name)
        && !model.is_empty()
    {
        return Some((provider, model.to_owned()));
    }
    let name = guess(spec).unwrap_or("openai");
    providers.iter().find(|p| p.name == name).map(|provider| (provider, spec.to_owned()))
}

/// `provider/model`, the way rein saves which model a conversation uses.
pub fn spec(provider: &Provider, model: &str) -> String {
    format!("{}/{model}", provider.name)
}

/// How to show `model` of `provider`: its name alone when that already
/// says the provider, `provider/model` otherwise.
pub fn shown(provider: &Provider, model: &str) -> String {
    if guess(model).unwrap_or("openai") == provider.name && !model.contains('/') { model.to_owned() } else { spec(provider, model) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_models_name_says_who_serves_it() {
        let providers = builtin();
        let resolved = |spec: &str| resolve(spec, &providers).map(|(p, m)| (p.name.clone(), m));
        assert_eq!(resolved("claude-opus-5-5"), Some(("anthropic".into(), "claude-opus-5-5".into())));
        assert_eq!(resolved("anthropic/claude-sonnet-5-5"), Some(("anthropic".into(), "claude-sonnet-5-5".into())));
        assert_eq!(resolved("gpt-6.1-sol"), Some(("openai".into(), "gpt-6.1-sol".into())));
        assert_eq!(resolved("openai/gpt-6.1-terra"), Some(("openai".into(), "gpt-6.1-terra".into())));
        assert_eq!(resolved("openrouter/qwen/qwen3-coder"), Some(("openrouter".into(), "qwen/qwen3-coder".into())), "the rest is the model, slashes and all");
        assert_eq!(resolved("ollama/qwen3:8b"), Some(("ollama".into(), "qwen3:8b".into())));
        assert_eq!(resolved("deepseek-chat"), Some(("deepseek".into(), "deepseek-chat".into())));
        assert_eq!(resolved("grok-5"), Some(("xai".into(), "grok-5".into())));
        assert_eq!(resolved("gemini-3.8-flash"), Some(("gemini".into(), "gemini-3.8-flash".into())));
        // Unknown: OpenAI's, as before, for servers behind OPENAI_BASE_URL.
        assert_eq!(resolved("test-model"), Some(("openai".into(), "test-model".into())));
        assert_eq!(resolved("qwen/qwen3-coder"), Some(("openai".into(), "qwen/qwen3-coder".into())), "no provider called qwen");
    }

    #[test]
    fn shows_the_provider_only_when_the_name_doesnt() {
        let providers = builtin();
        let find = |name: &str| providers.iter().find(|p| p.name == name).unwrap();
        assert_eq!(shown(find("anthropic"), "claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(shown(find("openai"), "gpt-6.1-sol"), "gpt-6.1-sol");
        assert_eq!(shown(find("anthropic"), "my-gateway-model"), "anthropic/my-gateway-model");
        assert_eq!(shown(find("openrouter"), "anthropic/claude-opus-5-5"), "openrouter/anthropic/claude-opus-5-5");
        assert_eq!(shown(find("ollama"), "qwen3:8b"), "ollama/qwen3:8b");
        assert_eq!(spec(find("anthropic"), "claude-opus-5-5"), "anthropic/claude-opus-5-5");
    }

    #[test]
    fn local_servers_need_no_key_and_each_format_its_header() {
        let providers = builtin();
        let find = |name: &str| providers.iter().find(|p| p.name == name).unwrap();
        assert!(!find("ollama").needs_key && !find("lmstudio").needs_key);
        assert!(find("groq").needs_key);
        assert_eq!(find("anthropic").key_header, Some("x-api-key"));
        assert_eq!(find("openrouter").key_header, None);
        assert_eq!(find("gemini").key_header, Some("x-goog-api-key"));
        assert!(find("openrouter").quirks.effort_object && find("mistral").quirks.short_ids);
        let mut names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        names.dedup();
        assert_eq!(names.len(), providers.len(), "every name once");
    }

    #[test]
    fn a_providers_file_adds_and_changes() {
        let file = "# mine\n[local]\napi = chat\nurl = http://localhost:8080/v1/\ncontext = 131072\nmodel = qwen3-coder\n\n[openrouter]\nurl = https://proxy.example/v1\nkey_env = MY_KEY, OTHER_KEY\n\n[gateway]\napi = anthropic\nurl = https://gw.example/v1\nkey_env = GW_TOKEN\n";
        let providers = with_file(file).unwrap();
        let find = |name: &str| providers.iter().find(|p| p.name == name).unwrap();
        let local = find("local");
        assert_eq!((local.api, local.base_url.as_str(), local.context, local.needs_key), (Api::Chat, "http://localhost:8080/v1", Some(131_072), false));
        assert_eq!(local.default_model.as_deref(), Some("qwen3-coder"));
        let openrouter = find("openrouter");
        assert_eq!((openrouter.base_url.as_str(), openrouter.key_env.clone()), ("https://proxy.example/v1", vec!["MY_KEY".to_owned(), "OTHER_KEY".to_owned()]));
        assert!(openrouter.quirks.effort_object, "what it needs stays");
        assert!(openrouter.yours && local.yours && !find("groq").yours);
        assert_eq!((find("gateway").api, find("gateway").key_header), (Api::Anthropic, Some("x-api-key")));
        assert_eq!(resolve("local/qwen3-coder", &providers).map(|(p, m)| (p.name.clone(), m)), Some(("local".into(), "qwen3-coder".into())));
    }

    #[test]
    fn a_providers_file_says_what_is_wrong() {
        assert_eq!(with_file("api = chat").unwrap_err(), "line 1: settings go under a [provider] line");
        assert_eq!(with_file("[x]\napi = soap").unwrap_err(), "line 2: no API called soap; use responses, anthropic, chat or gemini");
        assert_eq!(with_file("[x]\napi = chat").unwrap_err(), "[x] needs a url");
        assert_eq!(with_file("[a/b]").unwrap_err(), "line 1: [a/b] isn't a name rein can use: one word, no slashes");
        assert_eq!(with_file("[x]\ncolour = blue").unwrap_err(), "line 2: unknown setting colour; use api, url, key_env, title, model or context");
        assert_eq!(with_file("[x]\nurl = u\ncontext = big").unwrap_err(), "line 3: context should be a number of tokens, not big");
    }
}
