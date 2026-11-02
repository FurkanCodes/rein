//! Optional, unofficial Claude subscription OAuth. This uses Claude Code's
//! public client identity; Anthropic does not authorize third-party apps to
//! offer this login. Callers must obtain explicit informed consent first.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::oauth::{pkce_challenge, random_token};
use crate::{Error, Result, store};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE: &str = "https://claude.ai/oauth/authorize";
const TOKEN: &str = "https://api.anthropic.com/v1/oauth/token";
pub(crate) const USER_AGENT: &str = "claude-cli/2.1.280 (external, cli)";
pub(crate) const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

impl Credentials {
    fn from_response(value: &Value, previous: Option<&Self>) -> Result<Self> {
        let access_token = value["access_token"].as_str().filter(|s| !s.is_empty());
        let refresh_token = value["refresh_token"].as_str().filter(|s| !s.is_empty()).or_else(|| previous.map(|p| p.refresh_token.as_str()));
        let expires = value["expires_in"].as_u64().filter(|&n| n > 0);
        match (access_token, refresh_token, expires) {
            (Some(access), Some(refresh), Some(expires)) => {
                Ok(Self { access_token: access.to_owned(), refresh_token: refresh.to_owned(), expires_at: now().saturating_add(expires) })
            }
            _ => Err(Error::Auth("Claude OAuth returned incomplete credentials; sign in again".into())),
        }
    }
}

pub struct Pending {
    pub url: String,
    listeners: Vec<TcpListener>,
    state: String,
    verifier: String,
    redirect: String,
}

pub async fn begin() -> Result<Pending> {
    // Claude Code's registered loopback callback port.
    begin_at(AUTHORIZE, 54545).await
}

async fn begin_at(authorize: &str, port: u16) -> Result<Pending> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://localhost:{port}/callback");
    let mut listeners = vec![listener];
    if let Ok(v6) = TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    let (state, verifier) = (random_token()?, random_token()?);
    let url = reqwest::Url::parse_with_params(
        authorize,
        &[
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("scope", "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"),
            ("code_challenge", &pkce_challenge(&verifier)),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("code", "true"),
        ],
    )
    .map_err(|_| Error::Auth("invalid Claude authorization URL".into()))?
    .to_string();
    Ok(Pending { url, listeners, state, verifier, redirect })
}

impl Pending {
    pub async fn finish(self, http: &reqwest::Client) -> Result<Credentials> {
        self.finish_at(http, TOKEN).await
    }

    async fn finish_at(self, http: &reqwest::Client, token_url: &str) -> Result<Credentials> {
        let code = crate::openrouter::wait_for_callback(self.listeners, self.state.clone(), "Claude OAuth (unofficial)").await?;
        let value = exchange(
            http,
            token_url,
            json!({
                "grant_type": "authorization_code", "client_id": CLIENT_ID, "code": code,
                "redirect_uri": self.redirect, "code_verifier": self.verifier, "state": self.state,
            }),
        )
        .await?;
        Credentials::from_response(&value, None)
    }
}

async fn exchange(http: &reqwest::Client, url: &str, body: Value) -> Result<Value> {
    let response = http.post(url).timeout(Duration::from_secs(30)).header("User-Agent", USER_AGENT).header("anthropic-beta", "oauth-2025-04-20").json(&body).send().await?;
    if !response.status().is_success() {
        // Token endpoints may echo secrets. Never include their body in errors.
        return Err(Error::Auth(format!("Claude OAuth token exchange failed (HTTP {}); run `rein login anthropic --oauth` again", response.status())));
    }
    Ok(response.json().await?)
}

/// Claude Code compatibility is wire-only: never change rein's saved
/// instructions or tool names, including when switching providers.
pub(crate) fn shape(body: &mut Value) {
    let identity = json!({ "type": "text", "text": IDENTITY });
    match body.get_mut("system").and_then(Value::as_array_mut) {
        Some(system) => system.insert(0, identity),
        None => body["system"] = json!([identity]),
    }
    if let Some(tools) = body["tools"].as_array_mut() {
        for tool in tools {
            if let Some(name) = tool["name"].as_str() {
                tool["name"] = json!(format!("_{name}"));
            }
        }
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    if block["type"] == "tool_use"
                        && let Some(name) = block["name"].as_str()
                    {
                        block["name"] = json!(format!("_{name}"));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn request(listener: &TcpListener, status: u16, reply: Value) -> Value {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let split = loop {
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buf[..n]);
            if let Some(n) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break n + 4;
            }
        };
        let head = String::from_utf8_lossy(&bytes[..split]);
        let length: usize = head.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length: ").map(|s| s.parse().unwrap())).unwrap();
        while bytes.len() < split + length {
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buf[..n]);
        }
        let body = serde_json::from_slice(&bytes[split..split + length]).unwrap();
        let reply = reply.to_string();
        socket
            .write_all(format!("HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
            .await
            .unwrap();
        body
    }

    #[tokio::test]
    async fn claude_oauth_checks_state_and_exchanges_pkce() {
        let http = reqwest::Client::new();
        let pending = begin_at("https://example.invalid/authorize", 0).await.unwrap();
        let port = pending.listeners[0].local_addr().unwrap().port();
        let url = reqwest::Url::parse(&pending.url).unwrap();
        let param = |key: &str| url.query_pairs().find(|(k, _)| k == key).unwrap().1.into_owned();
        assert_eq!(param("client_id"), CLIENT_ID);
        assert_eq!(param("code_challenge_method"), "S256");
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let browser = async {
            let callback = format!("http://127.0.0.1:{port}/callback");
            let wrong = http.get(reqwest::Url::parse_with_params(&callback, &[("code", "bad"), ("state", "wrong")]).unwrap()).send().await.unwrap();
            assert_eq!(wrong.status(), 400);
            assert!(http.get(reqwest::Url::parse_with_params(&callback, &[("code", "good"), ("state", &param("state"))]).unwrap()).send().await.unwrap().status().is_success());
        };
        let server = request(&listener, 200, json!({ "access_token": "access-secret", "refresh_token": "refresh-secret", "expires_in": 3600 }));
        let (credentials, (), sent) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(pending.finish_at(&http, &endpoint), browser, server) }).await.unwrap();
        let credentials = credentials.unwrap();
        assert_eq!(sent["code"], "good");
        assert_eq!(sent["redirect_uri"], param("redirect_uri"));
        assert_eq!(pkce_challenge(sent["code_verifier"].as_str().unwrap()), param("code_challenge"));
        assert_eq!(sent["state"], param("state"));
        assert!(!format!("{credentials:?}").contains("secret"));
        let rebound = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        drop(rebound);
    }

    #[tokio::test]
    async fn claude_oauth_refresh_persists_rotation_and_honors_logout() {
        let dir = std::env::temp_dir().join(format!("rein-claude-oauth-{}-{}", std::process::id(), random_token().unwrap()));
        let path = dir.join("credentials.json");
        let old = Credentials { access_token: "old-access".into(), refresh_token: "old-refresh".into(), expires_at: 0 };
        store::write_json_private(&path, &old).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut session = Session::new(path.clone());
        session.token_url = format!("http://{}/token", listener.local_addr().unwrap());
        let http = reqwest::Client::new();
        let server = request(&listener, 200, json!({ "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600 }));
        let (token, sent) = tokio::join!(session.token(&http), server);
        assert_eq!(token.unwrap(), "new-access");
        assert_eq!(sent["grant_type"], "refresh_token");
        assert_eq!(sent["refresh_token"], "old-refresh");
        let saved: Credentials = store::read_json(&path).unwrap().unwrap();
        assert_eq!(saved.refresh_token, "new-refresh");
        assert_eq!(session.token(&http).await.unwrap(), "new-access", "fresh credentials need no exchange");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_file(&path).unwrap();
        assert!(session.token(&http).await.is_err());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn claude_oauth_token_errors_do_not_echo_secrets() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let http = reqwest::Client::new();
        let server = request(&listener, 400, json!({ "error": "secret-refresh-token" }));
        let (result, _) = tokio::join!(exchange(&http, &endpoint, json!({})), server);
        let shown = result.unwrap_err().to_string();
        assert!(!shown.contains("secret"));
        assert!(shown.contains("sign") || shown.contains("login"));
    }

    #[tokio::test]
    async fn claude_oauth_dropping_login_releases_callback() {
        let pending = begin_at("https://example.invalid/authorize", 0).await.unwrap();
        let address = pending.listeners[0].local_addr().unwrap();
        drop(pending);
        assert!(TcpListener::bind(address).await.is_ok());
    }

    #[test]
    fn claude_oauth_rejects_incomplete_tokens() {
        for value in [json!({}), json!({ "access_token": "a", "expires_in": 3600 }), json!({ "access_token": "", "refresh_token": "r", "expires_in": 0 })] {
            assert!(Credentials::from_response(&value, None).is_err());
        }
    }
}

/// Shared by client clones. Reloading from disk honors logout and rotated
/// credentials, and the async lock serializes refreshes within one client.
#[derive(Debug)]
pub(crate) struct Session {
    path: PathBuf,
    lock: tokio::sync::Mutex<()>,
    token_url: String,
}

impl Session {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, lock: tokio::sync::Mutex::new(()), token_url: TOKEN.into() }
    }

    pub(crate) async fn token(&self, http: &reqwest::Client) -> Result<String> {
        let _guard = self.lock.lock().await;
        let mut credentials: Credentials = store::read_json(&self.path)?.ok_or_else(|| Error::Auth("Claude OAuth is signed out; run `rein login anthropic --oauth`".into()))?;
        if credentials.expires_at <= now().saturating_add(300) {
            let value = exchange(
                http,
                &self.token_url,
                json!({
                    "grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": credentials.refresh_token,
                }),
            )
            .await?;
            credentials = Credentials::from_response(&value, Some(&credentials))?;
            // Do not recreate credentials if the user signed out while refreshing.
            if !self.path.exists() {
                return Err(Error::Auth("Claude OAuth was signed out during refresh".into()));
            }
            store::write_json_private(&self.path, &credentials)?;
        }
        Ok(credentials.access_token)
    }
}
