//! Who serves a model: a provider is a server, the format it speaks, and
//! where its key comes from. rein knows the common ones; the model's name
//! says which: `anthropic/claude-opus-5-5`, or just `claude-opus-5-5`,
//! since a Claude model's own name says where it lives.

use crate::Api;

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
    /// The header the key goes in; `None` for `Authorization: Bearer`.
    pub key_header: Option<&'static str>,
    /// An environment variable that points it at another server, like a proxy.
    pub url_env: Option<String>,
    /// A model to use when none is named and only this provider has a key.
    pub default_model: Option<String>,
    /// Where to get a key, for when there's none.
    pub keys_url: Option<String>,
}

/// The providers rein knows without being told.
pub fn builtin() -> Vec<Provider> {
    vec![
        Provider {
            name: "openai".into(),
            title: "OpenAI".into(),
            api: Api::Responses,
            base_url: crate::openai::API_BASE.into(),
            key_env: vec!["OPENAI_API_KEY".into()],
            key_header: None,
            url_env: Some("OPENAI_BASE_URL".into()),
            default_model: Some("gpt-6.1-sol".into()),
            keys_url: Some("https://platform.openai.com/api-keys".into()),
        },
        Provider {
            name: "anthropic".into(),
            title: "Anthropic".into(),
            api: Api::Anthropic,
            base_url: crate::anthropic::API_BASE.into(),
            key_env: vec!["ANTHROPIC_API_KEY".into()],
            key_header: Some("x-api-key"),
            url_env: Some("ANTHROPIC_BASE_URL".into()),
            default_model: Some("claude-opus-5-5".into()),
            keys_url: Some("https://platform.claude.com/settings/keys".into()),
        },
    ]
}

/// The provider a model's own name points to, when it's one with a
/// family name of its own.
pub fn guess(model: &str) -> Option<&'static str> {
    let families: [(&[&str], &str); 2] = [(&["claude-"], "anthropic"), (&["gpt-", "o1", "o3", "o4", "codex-", "chatgpt-"], "openai")];
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
    if guess(model).unwrap_or("openai") == provider.name { model.to_owned() } else { spec(provider, model) }
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
        // Unknown: OpenAI's, as before, for servers behind OPENAI_BASE_URL.
        assert_eq!(resolved("test-model"), Some(("openai".into(), "test-model".into())));
        assert_eq!(resolved("qwen/qwen3-coder"), Some(("openai".into(), "qwen/qwen3-coder".into())), "no provider called qwen");
    }

    #[test]
    fn shows_the_provider_only_when_the_name_doesnt() {
        let providers = builtin();
        let (anthropic, openai) = (&providers[1], &providers[0]);
        assert_eq!(shown(anthropic, "claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(shown(openai, "gpt-6.1-sol"), "gpt-6.1-sol");
        assert_eq!(shown(anthropic, "my-gateway-model"), "anthropic/my-gateway-model");
        assert_eq!(spec(anthropic, "claude-opus-5-5"), "anthropic/claude-opus-5-5");
    }
}
