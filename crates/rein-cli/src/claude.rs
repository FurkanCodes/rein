//! Claude subscription setup: the official CLI subprocess path, and the
//! separate explicit opt-in to unofficial OAuth. API-key login stays separate.

use crate::{CliResult, config_dir, describe};
use rein_llm::{anthropic::oauth, store};
use std::time::Duration;

pub(crate) const NAME: &str = "anthropic --oauth";
pub(crate) const FILE: &str = "claude-oauth.json";
pub(crate) const CODE_FILE: &str = "claude-code.json";

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CodeCredentials {
    pub(crate) token: Option<String>,
}

pub(crate) fn save_code(token: String, tell: &dyn Fn(&str)) -> CliResult {
    let token = token.trim();
    let credentials = CodeCredentials { token: (!token.is_empty()).then(|| token.to_owned()) };
    store::write_json_private(&config_dir()?.join(CODE_FILE), &credentials).map_err(describe)?;
    tell("Claude Code enabled. Use `/model claude-code/sonnet` or `rein -m claude-code/sonnet`. The CLI must be installed and signed in, or supplied a setup-token token.");
    Ok(())
}

pub(crate) const WARNING: &str = "Warning: Claude OAuth is unofficial and not authorized by Anthropic for third-party apps.\n\
It impersonates Claude Code, including its client ID, headers and system identity.\n\
Anthropic may block access or restrict your account without notice. Your subscription's limits apply.\n\
API keys remain the supported alternative. Continue only if you accept these risks.\n\
https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use";

pub(crate) async fn login(http: &rein_llm::reqwest::Client, tell: &dyn Fn(&str)) -> CliResult {
    let pending = oauth::begin().await.map_err(describe)?;
    tell("Waiting for Claude OAuth in your browser (unofficial)...");
    if !crate::open_browser(&pending.url).await {
        tell(&format!("Open this link on this computer to continue:\n\n{}\n", pending.url));
    }
    let credentials =
        tokio::time::timeout(Duration::from_secs(600), pending.finish(http)).await.map_err(|_| "Claude OAuth sign-in timed out after 10 minutes".to_owned())?.map_err(describe)?;
    store::write_json_private(&config_dir()?.join(FILE), &credentials).map_err(describe)?;
    tell("Signed in to Claude OAuth (unofficial). API keys and gateway tokens take precedence if configured.");
    Ok(())
}

pub(crate) async fn login_cli(http: &rein_llm::reqwest::Client) -> CliResult {
    say!("{WARNING}");
    let answer = tokio::task::spawn_blocking(|| crate::connect::read_secret("Type 'I accept' to continue (anything else cancels): "))
        .await
        .map_err(|e| format!("couldn't read consent: {e}"))??;
    if answer != "I accept" {
        say!("Cancelled; nothing was saved.");
        return Ok(());
    }
    tokio::select! {
        result = login(http, &|line| say!("{line}")) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
