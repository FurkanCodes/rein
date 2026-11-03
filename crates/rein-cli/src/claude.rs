//! Claude subscription setup through Anthropic's own CLI (`claude-code`).
//! API-key login stays separate.

use crate::{CliResult, config_dir, describe};
use rein_llm::store;
use std::path::Path;

pub(crate) const CODE_FILE: &str = "claude-code.json";

/// Where an earlier rein kept a Claude sign-in it made as Claude Code,
/// with Claude Code's client ID, headers and system prompt. rein no longer
/// signs in as another app, so the tokens it left are deleted on sight.
const BORROWED_FILE: &str = "claude-oauth.json";

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

/// Deletes the Claude sign-in an earlier rein made as Claude Code, if it's
/// in `dir`, and says so with `tell`. Once it's gone, there's nothing to say.
pub(crate) fn forget_borrowed_sign_in(dir: &Path, tell: &dyn Fn(&str)) {
    let path = dir.join(BORROWED_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => tell(&format!(
            "Deleted the Claude sign-in an earlier rein made as Claude Code ({}): rein no longer signs in as another app.\n\
             For Claude, use an API key (`rein login anthropic`) or Claude Code itself (`rein login claude-code`).",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tell(&format!("couldn't delete {}, an old Claude sign-in rein no longer uses: {e}", path.display())),
    }
}
