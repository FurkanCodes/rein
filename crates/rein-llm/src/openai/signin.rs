//! Sign in with ChatGPT for open-source agents.
//!
//! This is OpenAI's official way for an open-source tool running on the
//! user's machine to use their ChatGPT plan. It's an OAuth 2.0
//! authorization-code flow with PKCE:
//!
//! 1. Listen on a loopback port and open the browser at OpenAI's authorize
//!    page. The first sign-in registers rein as an agent on the account.
//! 2. The browser comes back to `http://127.0.0.1:1455/auth/callback` with a
//!    one-time code (and, on first sign-in, the client ID OpenAI issued).
//! 3. Exchange the code for tokens. The access token lasts an hour and works
//!    as a bearer token for `POST /v1/responses`; the refresh token renews it.
//!
//! Spec: <https://developers.openai.com/siwc/token-sharing-open-source/sign-in>

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::oauth::{new_uuid, pkce_challenge, random_token, read_request_target, respond};
use crate::{Error, Result, store};

const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const ISSUER: &str = "https://auth.openai.com";
/// The API the tokens are for.
const RESOURCE: &str = "https://api.openai.com/v1";
/// First sign-ins start with this placeholder; OpenAI answers with the real,
/// issued client ID, which later sign-ins reuse.
const REGISTRATION_CLIENT_ID: &str = "dynamic_agent_client";
/// The scope that lets requests use the ChatGPT plan. Signing in without it
/// still works, but can't pay for anything.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
/// Refresh this long before expiry, so no request starts with a token that
/// expires mid-stream.
const REFRESH_MARGIN_SECS: u64 = 180;
/// How long a token request may take.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a window waits for another one renewing the sign-in.
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// Everything kept after signing in. Store it so only the user can read it.
/// Its `Debug` leaves the tokens out.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub email: Option<String>,
    /// The account's stable ID, from the ID token.
    pub subject: String,
    /// Issued on the first sign-in to this account; reused on later ones.
    pub client_id: String,
    pub ext_agent_host_id: String,
    /// Kept so a later sign-in can skip the account picker.
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub scopes: Vec<String>,
    /// When `access_token` expires, in Unix seconds.
    pub expires_at: u64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("email", &self.email)
            .field("subject", &self.subject)
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Whether the user allowed rein to use their ChatGPT plan.
    pub fn plan_enabled(&self) -> bool {
        self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }

    fn needs_refresh(&self) -> bool {
        now() + REFRESH_MARGIN_SECS >= self.expires_at
    }
}

/// A ChatGPT sign-in that renews itself, shared by a client's clones.
///
/// The access token lasts an hour, and a conversation can go on for longer.
/// So every request asks for the token, which is read from the file each
/// time, so that `rein login`, `/logout` and other windows' renewals all
/// count. It's renewed when it's about to run out. A refresh token works only
/// once, so renewing takes a lock that every rein window shares, and reads
/// the file again under it: two windows spending the same refresh token
/// would sign you out.
#[derive(Debug)]
pub struct Session {
    path: PathBuf,
    /// One renewal at a time within this window.
    lock: tokio::sync::Mutex<()>,
    token_url: String,
}

impl Session {
    /// The sign-in saved at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self::at(path, TOKEN_URL)
    }

    /// `new`, renewing at `token_url`: OpenAI's for real, a fake for tests.
    pub(crate) fn at(path: PathBuf, token_url: &str) -> Self {
        Self { path, lock: tokio::sync::Mutex::new(()), token_url: token_url.to_owned() }
    }

    /// An access token good for a request starting now.
    pub async fn token(&self, http: &reqwest::Client) -> Result<String> {
        self.fresh(http, None).await
    }

    /// A new access token after the server turned `rejected` away: the one
    /// on file if another window has renewed it since, or a renewed one.
    pub async fn renew(&self, http: &reqwest::Client, rejected: &str) -> Result<String> {
        self.fresh(http, Some(rejected)).await
    }

    async fn fresh(&self, http: &reqwest::Client, rejected: Option<&str>) -> Result<String> {
        let stale = |c: &Credentials| c.needs_refresh() || rejected == Some(c.access_token.as_str());
        let _this_window = self.lock.lock().await;
        let credentials = self.load()?;
        if !stale(&credentials) {
            return Ok(credentials.access_token);
        }
        let _every_window = lock_file(&self.path.with_extension("lock")).await?;
        // Another window may have renewed it while this one waited.
        let credentials = self.load()?;
        if !stale(&credentials) {
            return Ok(credentials.access_token);
        }
        let renewed = match refresh(http, &self.token_url, &credentials).await {
            // OpenAI turned the refresh token down. Network trouble and
            // server errors pass through as they are, to be tried again.
            Err(Error::Auth(why)) => return Err(Error::Auth(format!("couldn't renew your ChatGPT sign-in ({why}); run `rein login` to sign in again"))),
            renewed => renewed?,
        };
        store::write_json_private(&self.path, &renewed)?;
        Ok(renewed.access_token)
    }

    fn load(&self) -> Result<Credentials> {
        store::read_json(&self.path)?.ok_or_else(|| auth_error("you're not signed in with ChatGPT any more; run `rein login`"))
    }
}

/// Takes the lock on renewing that every rein window shares, waiting a while
/// for a window that holds it. It's let go when the file is dropped, or when
/// its process ends however it ends.
async fn lock_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(std::fs::TryLockError::WouldBlock) => return Err(auth_error("another rein window is still renewing your ChatGPT sign-in; try again in a moment")),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// A new random ID for this installation ("agent host"). Create it once and
/// keep it: OpenAI uses it to tell a user's machines apart.
pub fn new_host_id() -> Result<String> {
    Ok(format!("urn:uuid:{}", new_uuid()?))
}

/// A sign-in waiting for the browser to come back.
pub struct PendingSignIn {
    /// Open this in the user's browser. When signing in again it contains the
    /// old ID token as a hint, so don't write it to logs.
    pub url: String,
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
    host_id: String,
    /// The saved client ID when signing in again; `None` on first sign-in.
    client_id: Option<String>,
}

/// Starts a sign-in. Pass this account's previous credentials, if any, so
/// OpenAI reuses the existing registration instead of creating a new one.
pub async fn begin(host_id: &str, agent_name: &str, previous: Option<&Credentials>) -> Result<PendingSignIn> {
    begin_on(CALLBACK_PORT, host_id, agent_name, previous).await
}

/// `begin`, with the callback on `port`: OpenAI's registered one for real
/// sign-ins, any free one (0) for tests, so they can't collide with one.
async fn begin_on(port: u16, host_id: &str, agent_name: &str, previous: Option<&Credentials>) -> Result<PendingSignIn> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        Error::Auth(format!(
            "can't listen on 127.0.0.1:{port} for the sign-in callback ({e}). Another sign-in, \
             maybe from the Codex CLI, is using it; finish or cancel that one and try again."
        ))
    })?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let state = random_token()?;
    let nonce = random_token()?;
    let verifier = random_token()?;
    let challenge = pkce_challenge(&verifier);

    let mut params = vec![
        ("response_type", "code"),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", SCOPES),
        ("resource", RESOURCE),
        ("state", state.as_str()),
        ("nonce", nonce.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("ext_agent_host_id", host_id),
    ];
    match previous {
        Some(p) => {
            params.push(("client_id", p.client_id.as_str()));
            params.push(("id_token_hint", p.id_token.as_str()));
            if let Some(email) = &p.email {
                params.push(("login_hint", email.as_str()));
            }
            if !p.plan_enabled() {
                // They declined plan usage last time; show the consent screen again.
                params.push(("prompt", "consent"));
            }
        }
        None => {
            params.push(("client_id", REGISTRATION_CLIENT_ID));
            params.push(("agent_name_hint", agent_name));
        }
    }
    let url = Url::parse_with_params(AUTHORIZE_URL, &params).expect("authorize URL is valid").to_string();
    Ok(PendingSignIn { url, listener, redirect_uri, state, nonce, verifier, host_id: host_id.to_owned(), client_id: previous.map(|p| p.client_id.clone()) })
}

impl PendingSignIn {
    /// Waits for the browser callback, then trades the code for tokens.
    pub async fn finish(self, http: &reqwest::Client) -> Result<Credentials> {
        let (code, client_id) = self.wait_for_callback().await?;
        let token = request_token(
            http,
            TOKEN_URL,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", client_id.as_str()),
                ("code", code.as_str()),
                ("code_verifier", self.verifier.as_str()),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("resource", RESOURCE),
            ],
        )
        .await?;
        let id_token = token.id_token.ok_or_else(|| auth_error("the token response had no ID token"))?;
        let claims = id_token_claims(&id_token, &client_id, &self.nonce)?;
        Ok(Credentials {
            email: claims.email,
            subject: claims.subject,
            client_id,
            ext_agent_host_id: self.host_id,
            id_token,
            access_token: token.access_token,
            refresh_token: token.refresh_token.ok_or_else(|| auth_error("the token response had no refresh token"))?,
            scopes: token.scope.unwrap_or_default().split_whitespace().map(str::to_owned).collect(),
            expires_at: now() + token.expires_in,
        })
    }

    /// Serves the loopback callback until the real one arrives. Each
    /// connection gets its own task, because browsers open spare connections
    /// that may never send a request.
    async fn wait_for_callback(&self) -> Result<(String, String)> {
        let (tx, mut rx) = mpsc::channel(1);
        // Dropped, as when the sign-in is given up on, it stops them all.
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (socket, _) = accepted?;
                    let (tx, state, saved) = (tx.clone(), self.state.clone(), self.client_id.clone());
                    connections.spawn(async move {
                        if let Some(result) = handle_callback(socket, &state, saved.as_deref()).await {
                            let _ = tx.send(result).await;
                        }
                    });
                }
                Some(result) = rx.recv() => return result,
            }
        }
    }
}

/// Handles one browser request. Returns `None` for requests that aren't the
/// callback we're waiting for.
async fn handle_callback(mut socket: TcpStream, state: &str, saved_client_id: Option<&str>) -> Option<Result<(String, String)>> {
    let target = read_request_target(&mut socket).await?;
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != CALLBACK_PATH {
        respond(&mut socket, 404, "Not found.").await;
        return None;
    }
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if query.get("state").map(String::as_str) != Some(state) {
        // Probably a tab left over from an earlier attempt. Keep waiting.
        respond(&mut socket, 400, "This sign-in link has expired. Start again from rein.").await;
        return None;
    }
    let (status, page, result) = match callback_result(&query, saved_client_id) {
        Ok(ok) => (200, "rein is connected to ChatGPT. You can close this tab and go back to the terminal.", Ok(ok)),
        Err(e) => (400, "ChatGPT wasn't connected. You can close this tab and go back to the terminal.", Err(e)),
    };
    respond(&mut socket, status, page).await;
    Some(result)
}

/// Picks the authorization code and the client ID out of a callback whose
/// `state` already matched.
fn callback_result(query: &HashMap<String, String>, saved_client_id: Option<&str>) -> Result<(String, String)> {
    if let Some(error) = query.get("error") {
        return Err(auth_error(&format!("sign-in wasn't completed ({error})")));
    }
    let code = query.get("code").ok_or_else(|| auth_error("the callback had no authorization code"))?;
    let client_id = match (saved_client_id, query.get("client_id")) {
        (Some(saved), Some(returned)) if saved != returned => {
            return Err(auth_error("the callback returned a different client ID than this account's registration"));
        }
        (Some(saved), _) => saved.to_owned(),
        (None, Some(issued)) if issued != REGISTRATION_CLIENT_ID => issued.clone(),
        (None, _) => return Err(auth_error("registration didn't return a client ID; try signing in again")),
    };
    Ok((code.clone(), client_id))
}

/// Gets a new access token at `token_url`. Refresh tokens rotate: the old one
/// stops working, so save the returned credentials right away.
async fn refresh(http: &reqwest::Client, token_url: &str, credentials: &Credentials) -> Result<Credentials> {
    let token = request_token(
        http,
        token_url,
        &[("grant_type", "refresh_token"), ("client_id", credentials.client_id.as_str()), ("refresh_token", credentials.refresh_token.as_str()), ("resource", RESOURCE)],
    )
    .await?;
    let mut next = credentials.clone();
    next.access_token = token.access_token;
    if let Some(refresh_token) = token.refresh_token {
        next.refresh_token = refresh_token;
    }
    if let Some(scope) = token.scope {
        next.scopes = scope.split_whitespace().map(str::to_owned).collect();
    }
    if let Some(id_token) = token.id_token {
        next.id_token = id_token;
    }
    next.expires_at = now() + token.expires_in;
    Ok(next)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

async fn request_token(http: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = http.post(url).header(reqwest::header::ACCEPT, "application/json").form(form).timeout(TOKEN_TIMEOUT).send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        // OAuth errors look like {"error": "invalid_grant", "error_description": "..."}.
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let code = v["error"].as_str().unwrap_or("unknown_error");
        let description = v["error_description"].as_str().unwrap_or_default();
        let message = format!("token request failed (HTTP {status}): {code} {description}");
        // A server having trouble hasn't turned anything down: tried again.
        if status.is_server_error() {
            return Err(Error::api(Some(status.as_u16()), Some(code.to_owned()), message.trim_end().to_owned(), None));
        }
        return Err(auth_error(message.trim_end()));
    }
    Ok(serde_json::from_str(&text)?)
}

struct IdClaims {
    subject: String,
    email: Option<String>,
}

/// Checks the ID token's issuer, audience, expiry and nonce, and reads who
/// signed in.
///
/// The signature isn't checked yet. The token came straight from OpenAI's
/// token endpoint over TLS, which OpenID Connect accepts instead of a
/// signature check (Core 1.0, section 3.1.3.7). OpenAI's guide still asks
/// for verification against its published keys, so that's a follow-up.
fn id_token_claims(id_token: &str, client_id: &str, nonce: &str) -> Result<IdClaims> {
    let payload = id_token.split('.').nth(1).ok_or_else(|| auth_error("the ID token isn't a JWT"))?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).map_err(|_| auth_error("the ID token isn't valid base64url"))?;
    let claims: Value = serde_json::from_slice(&bytes)?;
    if claims["iss"] != ISSUER {
        return Err(auth_error("the ID token has the wrong issuer"));
    }
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(auds) => auds.iter().any(|aud| aud == client_id),
        _ => false,
    };
    if !audience_ok {
        return Err(auth_error("the ID token is for a different client"));
    }
    if claims["exp"].as_u64().is_none_or(|exp| exp <= now()) {
        return Err(auth_error("the ID token has expired"));
    }
    if claims["nonce"] != nonce {
        return Err(auth_error("the ID token doesn't belong to this sign-in (nonce mismatch)"));
    }
    let subject = claims["sub"].as_str().ok_or_else(|| auth_error("the ID token has no subject"))?;
    Ok(IdClaims { subject: subject.to_owned(), email: claims["email"].as_str().map(str::to_owned) })
}

fn auth_error(message: &str) -> Error {
    Error::Auth(message.to_owned())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A server on a free port that answers the requests it gets in turn with
    /// `replies` (status and JSON), the last one again once they run out, a
    /// little slowly, as a real one would. It keeps each request's head and
    /// body, as they came.
    pub(crate) async fn serve(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for n in 0.. {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 4096];
                let split = loop {
                    let read = socket.read(&mut buf).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buf[..read]);
                    if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let head = String::from_utf8_lossy(&bytes[..split]).to_lowercase();
                let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length: ").map(|n| n.parse().unwrap())).unwrap_or(0);
                while bytes.len() < split + length {
                    let read = socket.read(&mut buf).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buf[..read]);
                }
                log.lock().unwrap().push(String::from_utf8_lossy(&bytes).into_owned());
                let (status, body) = replies[n.min(replies.len() - 1)];
                tokio::time::sleep(Duration::from_millis(100)).await;
                let reply = format!("HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (url, seen)
    }

    pub(crate) const RENEWED: &str = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#;
    const USED_UP: &str = r#"{"error":"invalid_grant","error_description":"refresh token already used"}"#;

    fn credentials(access: &str, refresh: &str, expires_at: u64) -> Credentials {
        Credentials {
            email: None,
            subject: "user-1".into(),
            client_id: "oaiapp_1".into(),
            ext_agent_host_id: "urn:uuid:x".into(),
            id_token: "id".into(),
            access_token: access.into(),
            refresh_token: refresh.into(),
            scopes: vec![PLAN_SCOPE.into()],
            expires_at,
        }
    }

    /// A sign-in saved in a folder of its own, whose access token is
    /// `access` and lasts until `expires_at`.
    pub(crate) fn signed_in(name: &str, access: &str, expires_at: u64) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-chatgpt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("chatgpt.json");
        store::write_json_private(&path, &credentials(access, "old-refresh", expires_at)).unwrap();
        path
    }

    pub(crate) fn in_an_hour() -> u64 {
        now() + 3600
    }

    #[tokio::test]
    async fn a_sign_in_renews_itself_as_its_token_runs_out() {
        let path = signed_in("renews", "old-access", now() + 60);
        let (url, seen) = serve(vec![(200, RENEWED)]).await;
        let session = Session::at(path.clone(), &format!("{url}/token"));
        let http = reqwest::Client::new();
        assert_eq!(session.token(&http).await.unwrap(), "new-access", "a minute left is too little for a request to start with");
        let sent = seen.lock().unwrap()[0].clone();
        assert!(sent.contains("grant_type=refresh_token") && sent.contains("refresh_token=old-refresh"), "{sent}");
        let saved: Credentials = store::read_json(&path).unwrap().unwrap();
        assert_eq!(saved.refresh_token, "new-refresh", "a refresh token works once: the new one is kept");
        assert_eq!(session.token(&http).await.unwrap(), "new-access");
        assert_eq!(seen.lock().unwrap().len(), 1, "a token with time left is used as it is");
    }

    #[tokio::test]
    async fn two_windows_whose_token_runs_out_together_renew_it_once() {
        let path = signed_in("two-windows", "old-access", 0);
        // As OpenAI does, a refresh token spent twice is turned down.
        let (url, seen) = serve(vec![(200, RENEWED), (400, USED_UP)]).await;
        let token_url = format!("{url}/token");
        let (one, other) = (Session::at(path.clone(), &token_url), Session::at(path, &token_url));
        let http = reqwest::Client::new();
        let (a, b) = tokio::join!(one.token(&http), other.token(&http));
        assert_eq!((a.unwrap(), b.unwrap()), ("new-access".to_owned(), "new-access".to_owned()));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_token_turned_away_is_renewed_unless_another_window_already_has() {
        let path = signed_in("turned-away", "first", in_an_hour());
        let (url, seen) = serve(vec![(200, RENEWED)]).await;
        let session = Session::at(path.clone(), &format!("{url}/token"));
        let http = reqwest::Client::new();
        assert_eq!(session.token(&http).await.unwrap(), "first");
        assert_eq!(session.renew(&http, "first").await.unwrap(), "new-access");
        store::write_json_private(&path, &credentials("from-another-window", "newer-refresh", in_an_hour())).unwrap();
        assert_eq!(session.renew(&http, "new-access").await.unwrap(), "from-another-window");
        assert_eq!(seen.lock().unwrap().len(), 1, "what another window renewed isn't renewed again");
    }

    #[tokio::test]
    async fn a_sign_in_that_cant_be_renewed_says_so_and_trouble_is_tried_again() {
        let path = signed_in("dead", "old-access", 0);
        let (url, _) = serve(vec![(400, USED_UP), (503, r#"{"error":"temporarily_unavailable"}"#)]).await;
        let session = Session::at(path.clone(), &format!("{url}/token"));
        let http = reqwest::Client::new();
        let dead = session.token(&http).await.unwrap_err();
        assert!(matches!(dead, Error::Auth(_)) && dead.to_string().contains("rein login") && !dead.to_string().contains("old-refresh"), "{dead}");
        let busy = session.token(&http).await.unwrap_err();
        assert!(busy.retryable(), "a server in trouble hasn't turned anything down: {busy}");
        std::fs::remove_file(&path).unwrap();
        let signed_out = session.token(&http).await.unwrap_err();
        assert!(signed_out.to_string().contains("rein login"), "{signed_out}");
    }

    #[test]
    fn tokens_never_show_in_debug_output() {
        let credentials = Credentials {
            email: None,
            subject: "user-1".into(),
            client_id: "oaiapp_1".into(),
            ext_agent_host_id: "urn:uuid:x".into(),
            id_token: "id-secret".into(),
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            scopes: vec![],
            expires_at: 0,
        };
        let shown = format!("{credentials:?}");
        assert!(!shown.contains("secret") && shown.contains("user-1"), "{shown}");
    }

    #[test]
    fn host_id_is_a_v4_uuid_urn() {
        let id = new_host_id().unwrap();
        let uuid = id.strip_prefix("urn:uuid:").unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert_ne!(new_host_id().unwrap(), id);
    }

    fn jwt(claims: Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn checks_id_token_claims() {
        let good = serde_json::json!({
            "iss": ISSUER, "aud": "oaiapp_1", "exp": now() + 600,
            "nonce": "n1", "sub": "user-1", "email": "a@example.com",
        });
        let claims = id_token_claims(&jwt(good.clone()), "oaiapp_1", "n1").unwrap();
        assert_eq!((claims.subject.as_str(), claims.email.as_deref()), ("user-1", Some("a@example.com")));

        assert!(id_token_claims(&jwt(good.clone()), "oaiapp_2", "n1").is_err(), "wrong audience");
        assert!(id_token_claims(&jwt(good.clone()), "oaiapp_1", "n2").is_err(), "wrong nonce");
        let mut expired = good;
        expired["exp"] = serde_json::json!(now() - 1);
        assert!(id_token_claims(&jwt(expired), "oaiapp_1", "n1").is_err(), "expired");
    }

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn first_sign_in_needs_an_issued_client_id() {
        let ok = query(&[("code", "c"), ("client_id", "oaiapp_1")]);
        assert_eq!(callback_result(&ok, None).unwrap(), ("c".into(), "oaiapp_1".into()));
        assert!(callback_result(&query(&[("code", "c")]), None).is_err());
        let placeholder = query(&[("code", "c"), ("client_id", REGISTRATION_CLIENT_ID)]);
        assert!(callback_result(&placeholder, None).is_err());
    }

    /// Sends one HTTP GET to the callback port, like a browser would.
    async fn browser_get(port: u16, target: &str) -> String {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        socket.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn callback_server_survives_idle_and_stale_connections() {
        // Any free port: a real sign-in on 1455 can't get in the way.
        let pending = begin_on(0, "urn:uuid:00000000-0000-4000-8000-000000000000", "rein-test", None).await.unwrap();
        let port = pending.listener.local_addr().unwrap().port();
        let state = Url::parse(&pending.url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();

        let browser = async {
            // A spare connection that never sends a request mustn't block the server...
            let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            // ...a tab left over from an earlier attempt gets turned away...
            let stale = browser_get(port, &format!("{CALLBACK_PATH}?code=old&state=stale&client_id=oaiapp_0")).await;
            assert!(stale.starts_with("HTTP/1.1 400"), "{stale}");
            // ...and the real callback is accepted.
            let ok = browser_get(port, &format!("{CALLBACK_PATH}?code=c1&state={state}&client_id=oaiapp_1")).await;
            assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        };
        let (result, ()) = tokio::join!(pending.wait_for_callback(), browser);
        assert_eq!(result.unwrap(), ("c1".to_owned(), "oaiapp_1".to_owned()));
    }

    #[test]
    fn sign_in_again_keeps_the_saved_client_id() {
        assert_eq!(callback_result(&query(&[("code", "c")]), Some("oaiapp_1")).unwrap().1, "oaiapp_1");
        let other = query(&[("code", "c"), ("client_id", "oaiapp_2")]);
        assert!(callback_result(&other, Some("oaiapp_1")).is_err());
        assert!(callback_result(&query(&[("error", "access_denied")]), Some("oaiapp_1")).is_err());
    }
}
