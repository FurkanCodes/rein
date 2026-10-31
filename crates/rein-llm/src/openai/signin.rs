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
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::{Error, Result};

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

/// Everything kept after signing in. Store it so only the user can read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Credentials {
    /// Whether the user allowed rein to use their ChatGPT plan.
    pub fn plan_enabled(&self) -> bool {
        self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }

    pub fn needs_refresh(&self) -> bool {
        now() + REFRESH_MARGIN_SECS >= self.expires_at
    }
}

/// A new random ID for this installation ("agent host"). Create it once and
/// keep it: OpenAI uses it to tell a user's machines apart.
pub fn new_host_id() -> Result<String> {
    Ok(format!("urn:uuid:{}", new_uuid()?))
}

/// A random version 4 UUID, like `0f8fad5b-d9cb-469f-a165-70867728950e`.
pub(crate) fn new_uuid() -> Result<String> {
    let mut b = [0u8; 16];
    fill_random(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40; // UUID version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
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
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await.map_err(|e| {
        Error::Auth(format!(
            "can't listen on 127.0.0.1:{CALLBACK_PORT} for the sign-in callback ({e}). Another sign-in, \
             maybe from the Codex CLI, is using it; finish or cancel that one and try again."
        ))
    })?;
    let redirect_uri = format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}");
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
    Ok(PendingSignIn {
        url,
        listener,
        redirect_uri,
        state,
        nonce,
        verifier,
        host_id: host_id.to_owned(),
        client_id: previous.map(|p| p.client_id.clone()),
    })
}

impl PendingSignIn {
    /// Waits for the browser callback, then trades the code for tokens.
    pub async fn finish(self, http: &reqwest::Client) -> Result<Credentials> {
        let (code, client_id) = self.wait_for_callback().await?;
        let token = request_token(
            http,
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
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (socket, _) = accepted?;
                    let (tx, state, saved) = (tx.clone(), self.state.clone(), self.client_id.clone());
                    tokio::spawn(async move {
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

/// Reads an HTTP request head and returns its target, like
/// `/auth/callback?code=...`.
async fn read_request_target(socket: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return None;
        }
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut request_line = head.lines().next()?.split_whitespace();
    if request_line.next()? != "GET" {
        return None;
    }
    request_line.next().map(str::to_owned)
}

async fn respond(socket: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>rein</title>\
         <body style=\"font:16px system-ui,sans-serif;max-width:32em;margin:4em auto\"><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// Gets a new access token. Refresh tokens rotate: the old one stops working,
/// so save the returned credentials right away.
pub async fn refresh(http: &reqwest::Client, credentials: &Credentials) -> Result<Credentials> {
    let token = request_token(
        http,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", credentials.client_id.as_str()),
            ("refresh_token", credentials.refresh_token.as_str()),
            ("resource", RESOURCE),
        ],
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

async fn request_token(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<TokenResponse> {
    let response = http.post(TOKEN_URL).header(reqwest::header::ACCEPT, "application/json").form(form).send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        // OAuth errors look like {"error": "invalid_grant", "error_description": "..."}.
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let code = v["error"].as_str().unwrap_or("unknown_error");
        let description = v["error_description"].as_str().unwrap_or_default();
        return Err(auth_error(format!("token request failed (HTTP {status}): {code} {description}").trim_end()));
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
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| auth_error("the ID token isn't valid base64url"))?;
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

/// The PKCE challenge: base64url(SHA-256(verifier)). Only we know the
/// verifier, so a stolen authorization code is useless to anyone else.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 32 random bytes as base64url: 43 characters, valid as a PKCE verifier.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|e| auth_error(&format!("no secure random numbers available: {e}")))
}

fn auth_error(message: &str) -> Error {
    Error::Auth(message.to_owned())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
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
    async fn browser_get(target: &str) -> String {
        let mut socket = TcpStream::connect(("127.0.0.1", CALLBACK_PORT)).await.unwrap();
        socket.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn callback_server_survives_idle_and_stale_connections() {
        let pending = begin("urn:uuid:00000000-0000-4000-8000-000000000000", "rein-test", None).await.unwrap();
        let state = Url::parse(&pending.url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();

        let browser = async {
            // A spare connection that never sends a request mustn't block the server...
            let _idle = TcpStream::connect(("127.0.0.1", CALLBACK_PORT)).await.unwrap();
            // ...a tab left over from an earlier attempt gets turned away...
            let stale = browser_get(&format!("{CALLBACK_PATH}?code=old&state=stale&client_id=oaiapp_0")).await;
            assert!(stale.starts_with("HTTP/1.1 400"), "{stale}");
            // ...and the real callback is accepted.
            let ok = browser_get(&format!("{CALLBACK_PATH}?code=c1&state={state}&client_id=oaiapp_1")).await;
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
