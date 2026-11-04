//! Signing in to an MCP server reached at a URL, as MCP's authorization
//! spec (2025-11-25) has it, and keeping the sign-in.
//!
//! 1. The server's 401 names its metadata (OAuth 2.0 Protected Resource
//!    Metadata), or rein looks where the spec says it may be: it names the
//!    sign-in server, and what the token is for (`resource`).
//! 2. The sign-in server's own metadata says where to sign in, and that it
//!    takes PKCE, without which rein doesn't go on.
//! 3. rein registers itself there as rein (Dynamic Client Registration),
//!    once per sign-in server, or uses a client ID you registered yourself:
//!    never another app's.
//! 4. You sign in in the browser, which comes back to rein on this computer
//!    with a code, traded for tokens, for this server's URL alone.
//!
//! Tokens are kept in a file only you can read, by the server's URL. One
//! that's about to run out is renewed before it's sent, under a lock every
//! rein window shares, reading the file again under it: a refresh token
//! works once, and two windows spending the same one would sign you out.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::http::may_carry_a_key;
use crate::oauth::{callback, pkce_challenge, random_token};
use crate::{Error, Result, store};

/// Where the browser comes back to.
const CALLBACK_PATH: &str = "/callback";

/// How long each request to do with signing in may take.
const WAIT: Duration = Duration::from_secs(20);

/// A token this close to running out, in seconds, is renewed before it's
/// sent.
const EARLY: u64 = 60;

/// What rein keeps of your sign-ins to MCP servers.
#[derive(Serialize, Deserialize, Default)]
struct File {
    /// By each server's URL.
    #[serde(default)]
    servers: BTreeMap<String, Signed>,
    /// The client rein has with each sign-in server, by its issuer.
    #[serde(default)]
    clients: BTreeMap<String, Client>,
}

/// A sign-in to one server.
#[derive(Serialize, Deserialize, Clone)]
struct Signed {
    /// What the token is for, as rein asked for it.
    resource: String,
    token_endpoint: String,
    client: Client,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// When the access token runs out, in seconds since 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
}

/// A client rein has with a sign-in server: one it registered itself as,
/// or yours.
#[derive(Serialize, Deserialize, Clone)]
struct Client {
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    /// How it proves it's itself at the token endpoint, if it has a secret:
    /// `client_secret_post`, or else in a Basic header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_method: Option<String>,
    /// Where the browser comes back to, as it was registered.
    redirect_uri: String,
}

/// Never the tokens or the secret.
impl std::fmt::Debug for Signed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signed").field("resource", &self.resource).field("client", &self.client).field("expires_at", &self.expires_at).finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("client_id", &self.client_id).field("redirect_uri", &self.redirect_uri).finish_non_exhaustive()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// Your sign-ins to MCP servers, kept at a path.
pub struct Tokens {
    path: PathBuf,
    /// One renewal at a time within this window.
    lock: tokio::sync::Mutex<()>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Tokens {
    /// The sign-ins kept at `path`, renewed over `http`.
    pub fn new(path: PathBuf, http: reqwest::Client) -> Tokens {
        Tokens { path, lock: tokio::sync::Mutex::new(()), http }
    }

    fn load(&self) -> Result<File> {
        Ok(store::read_json(&self.path)?.unwrap_or_default())
    }

    fn save(&self, file: &File) -> Result<()> {
        store::write_json_private(&self.path, file)
    }

    /// The lock every rein window shares on changing the file.
    async fn every_window(&self) -> Result<std::fs::File> {
        store::lock_file(&self.path.with_extension("lock"), "another rein window is still renewing a sign-in to an MCP server; try again in a moment").await
    }

    /// Whether you're signed in to the server at `url`.
    pub fn signed_in(&self, url: &str) -> bool {
        self.load().is_ok_and(|file| file.servers.contains_key(url))
    }

    /// Forgets your sign-in to the server at `url`: whether there was one.
    pub async fn sign_out(&self, url: &str) -> Result<bool> {
        let _this_window = self.lock.lock().await;
        let _every_window = self.every_window().await?;
        let mut file = self.load()?;
        let had = file.servers.remove(url).is_some();
        if had {
            self.save(&file)?;
        }
        Ok(had)
    }

    /// The access token for the server at `url` now, renewed first if it's
    /// about to run out: `None` if you aren't signed in to it.
    pub async fn token(&self, url: &str) -> Result<Option<String>> {
        self.fresh(url, None).await
    }

    /// The server at `url` turned `rejected` away: the token on file if
    /// another window has renewed it since, or a renewed one; `None` if
    /// you need to sign in again.
    pub async fn renew(&self, url: &str, rejected: &str) -> Result<Option<String>> {
        self.fresh(url, Some(rejected)).await
    }

    async fn fresh(&self, url: &str, rejected: Option<&str>) -> Result<Option<String>> {
        let stale = |signed: &Signed| rejected == Some(signed.access_token.as_str()) || signed.expires_at.is_some_and(|at| at <= now() + EARLY);
        let _this_window = self.lock.lock().await;
        let Some(signed) = self.load()?.servers.remove(url) else { return Ok(None) };
        if !stale(&signed) {
            return Ok(Some(signed.access_token));
        }
        let _every_window = self.every_window().await?;
        // Another window may have renewed it while this one waited.
        let mut file = self.load()?;
        let Some(signed) = file.servers.get(url).cloned() else { return Ok(None) };
        if !stale(&signed) {
            return Ok(Some(signed.access_token));
        }
        let renewed = match &signed.refresh_token {
            Some(refresh) => match refresh_tokens(&self.http, &signed, refresh).await {
                Ok(renewed) => Some(renewed),
                // Turned down: signing in again is what's left.
                Err(Error::Auth(_)) => None,
                Err(e) => return Err(e),
            },
            None => None,
        };
        let token = renewed.as_ref().map(|renewed| renewed.access_token.clone());
        match renewed {
            Some(renewed) => file.servers.insert(url.to_owned(), renewed),
            None => file.servers.remove(url),
        };
        self.save(&file)?;
        Ok(token)
    }

    /// Starts signing in to the server at `url`, `found`: registers rein
    /// with its sign-in server, unless it has before or you give `yours`
    /// (a client ID you registered, and its secret if it has one), and
    /// listens for the browser to come back.
    pub async fn begin(&self, url: &str, found: &Found, yours: Option<(String, Option<String>)>) -> Result<Pending> {
        let kept = self.load()?.clients.get(&found.issuer).cloned();
        let (listener, client) = if let Some((client_id, client_secret)) = yours {
            let (listener, redirect_uri) = listen(0).await?;
            (listener, Client { client_id, client_secret, auth_method: None, redirect_uri })
        } else if let Some(kept) = kept
            // Where the browser comes back to was registered: the same port,
            // if nothing else has it now.
            && let Ok((listener, _)) = listen(port(&kept.redirect_uri)).await
        {
            (listener, kept)
        } else {
            let registration =
                found.registration.as_deref().ok_or_else(|| Error::Auth(format!("{} doesn't let rein register: give it a client ID you registered with it", found.issuer)))?;
            let (listener, redirect_uri) = listen(0).await?;
            (listener, register(&self.http, registration, redirect_uri).await?)
        };
        // Kept for next time: registering again would be another client.
        {
            let _this_window = self.lock.lock().await;
            let _every_window = self.every_window().await?;
            let mut file = self.load()?;
            file.clients.insert(found.issuer.clone(), client.clone());
            self.save(&file)?;
        }
        let (state, verifier) = (random_token()?, random_token()?);
        let challenge = pkce_challenge(&verifier);
        let mut params = vec![
            ("response_type", "code"),
            ("client_id", client.client_id.as_str()),
            ("redirect_uri", client.redirect_uri.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("resource", found.resource.as_str()),
        ];
        if let Some(scope) = &found.scope {
            params.push(("scope", scope));
        }
        let link = Url::parse_with_params(&found.authorization_endpoint, &params).map_err(|e| Error::Auth(format!("its sign-in page isn't a URL: {e}")))?.to_string();
        Ok(Pending { link, listener, state, verifier, client, url: url.to_owned(), resource: found.resource.clone(), token_endpoint: found.token_endpoint.clone() })
    }

    /// Keeps a sign-in to the server at `url`.
    async fn keep(&self, url: &str, signed: Signed) -> Result<()> {
        let _this_window = self.lock.lock().await;
        let _every_window = self.every_window().await?;
        let mut file = self.load()?;
        file.servers.insert(url.to_owned(), signed);
        self.save(&file)
    }
}

/// A sign-in waiting for the browser to come back.
pub struct Pending {
    /// Open this in the browser.
    pub link: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    client: Client,
    url: String,
    resource: String,
    token_endpoint: String,
}

/// Never the verifier, or what the link holds.
impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending").field("url", &self.url).finish_non_exhaustive()
    }
}

impl Pending {
    /// Waits for the browser, trades its code for tokens, and keeps them
    /// in `tokens`.
    pub async fn finish(self, tokens: &Tokens) -> Result<()> {
        let pages = ("rein is signed in. You can close this tab and go back to the terminal.", "rein wasn't signed in. You can close this tab and go back to the terminal.");
        let code = callback(vec![self.listener], self.state, CALLBACK_PATH, pages).await?;
        let form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code),
            ("redirect_uri", self.client.redirect_uri.clone()),
            ("code_verifier", self.verifier),
            ("resource", self.resource.clone()),
        ];
        let answer = token_request(&tokens.http, &self.token_endpoint, &self.client, form).await?;
        let signed = signed(&answer, None, &self.resource, &self.token_endpoint, &self.client)?;
        tokens.keep(&self.url, signed).await
    }
}

/// Listens for the browser on `port` of this computer's own address (any
/// free one for 0): the listener, and the address to come back to.
async fn listen(port: u16) -> Result<(TcpListener, String)> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, format!("http://127.0.0.1:{port}{CALLBACK_PATH}")))
}

/// The port of `redirect_uri`, or 0 for any.
fn port(redirect_uri: &str) -> u16 {
    Url::parse(redirect_uri).ok().and_then(|url| url.port()).unwrap_or(0)
}

/// Registers rein at `endpoint` as a client of its own, coming back to
/// `redirect_uri`.
async fn register(http: &reqwest::Client, endpoint: &str, redirect_uri: String) -> Result<Client> {
    let body = json!({
        "client_name": "rein",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = http.post(endpoint).json(&body).timeout(WAIT).send().await?;
    let status = response.status();
    let answer: Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::Auth(format!("rein couldn't register to sign in ({status}{})", said(&answer))));
    }
    let client_id = answer.get("client_id").and_then(Value::as_str).ok_or_else(|| Error::Auth("its answer to rein registering had no client ID".into()))?.to_owned();
    let text = |name: &str| answer.get(name).and_then(Value::as_str).map(str::to_owned);
    Ok(Client { client_id, client_secret: text("client_secret"), auth_method: text("token_endpoint_auth_method"), redirect_uri })
}

/// A sign-in renewed with `refresh`: a refresh token that isn't given back
/// stays as it was.
async fn refresh_tokens(http: &reqwest::Client, signed: &Signed, refresh: &str) -> Result<Signed> {
    let form = vec![("grant_type", "refresh_token".to_owned()), ("refresh_token", refresh.to_owned()), ("resource", signed.resource.clone())];
    let answer = token_request(http, &signed.token_endpoint, &signed.client, form).await?;
    self::signed(&answer, Some(refresh), &signed.resource, &signed.token_endpoint, &signed.client)
}

/// Asks `endpoint` for tokens with `form`, as `client`: a turned-down
/// request is `Error::Auth`.
async fn token_request(http: &reqwest::Client, endpoint: &str, client: &Client, mut form: Vec<(&'static str, String)>) -> Result<Value> {
    form.push(("client_id", client.client_id.clone()));
    let mut request = http.post(endpoint).timeout(WAIT).header("Accept", "application/json");
    match (&client.client_secret, client.auth_method.as_deref()) {
        (Some(secret), Some("client_secret_post")) => form.push(("client_secret", secret.clone())),
        (Some(secret), _) => request = request.header("Authorization", format!("Basic {}", STANDARD.encode(format!("{}:{secret}", client.client_id)))),
        (None, _) => {}
    }
    let response = request.form(&form).send().await?;
    let status = response.status();
    let answer: Value = response.json().await.unwrap_or_default();
    if status.is_client_error() {
        return Err(Error::Auth(format!("the sign-in was turned down ({status}{})", said(&answer))));
    }
    if !status.is_success() {
        let message = format!("the sign-in server answered {status}{}", said(&answer));
        return Err(Error::Api { status: Some(status.as_u16()), kind: crate::ErrorKind::Other, code: None, message, request_id: None, param: None });
    }
    Ok(answer)
}

/// The sign-in `answer` gives.
fn signed(answer: &Value, refresh: Option<&str>, resource: &str, token_endpoint: &str, client: &Client) -> Result<Signed> {
    let access_token =
        answer.get("access_token").and_then(Value::as_str).filter(|token| !token.is_empty()).ok_or_else(|| Error::Auth("the sign-in gave no access token".into()))?.to_owned();
    let refresh_token = answer.get("refresh_token").and_then(Value::as_str).map(str::to_owned).or_else(|| refresh.map(str::to_owned));
    let expires_at = answer.get("expires_in").and_then(Value::as_u64).map(|seconds| now().saturating_add(seconds));
    Ok(Signed { resource: resource.to_owned(), token_endpoint: token_endpoint.to_owned(), client: client.clone(), access_token, refresh_token, expires_at })
}

/// What an OAuth error answer says, for an error of rein's.
fn said(answer: &Value) -> String {
    let error = answer.get("error").and_then(Value::as_str).unwrap_or_default();
    let description = answer.get("error_description").and_then(Value::as_str).unwrap_or_default();
    match (error, description) {
        ("", "") => String::new(),
        (error, "") => format!(": {error}"),
        ("", description) => format!(": {description}"),
        (error, description) => format!(": {error}, {description}"),
    }
}

/// Where and how to sign in for a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The sign-in server, as it calls itself.
    pub issuer: String,
    /// What the token is for: the server's URL, or what its metadata says.
    pub resource: String,
    authorization_endpoint: String,
    token_endpoint: String,
    /// Where rein may register itself, if it may.
    registration: Option<String>,
    scope: Option<String>,
    /// rein has registered with it before.
    registered: bool,
}

impl Found {
    /// Whether rein can sign in without a client ID from you: it may
    /// register itself there, or has.
    pub fn registers(&self) -> bool {
        self.registered || self.registration.is_some()
    }
}

/// What a server's `WWW-Authenticate` says: where its metadata is, and
/// what to ask for.
fn challenge(said: &str) -> (Option<String>, Option<String>) {
    let mut params = BTreeMap::new();
    let rest = said.trim().strip_prefix("Bearer").or_else(|| said.trim().strip_prefix("bearer")).unwrap_or(said);
    let mut chars = rest.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        let name: String = std::iter::from_fn(|| chars.next_if(|c| *c != '=' && *c != ',' && !c.is_whitespace())).collect();
        if name.is_empty() {
            break;
        }
        if chars.next_if_eq(&'=').is_none() {
            continue;
        }
        let value: String = if chars.next_if_eq(&'"').is_some() {
            let mut value = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => value.extend(chars.next()),
                    c => value.push(c),
                }
            }
            value
        } else {
            std::iter::from_fn(|| chars.next_if(|c| *c != ',' && !c.is_whitespace())).collect()
        };
        params.insert(name.to_ascii_lowercase(), value);
    }
    (params.remove("resource_metadata"), params.remove("scope"))
}

/// Finds where to sign in for the server at `url`, from what its 401 said
/// (`www_authenticate`), and whether `tokens` has a client there already.
pub async fn discover(http: &reqwest::Client, tokens: &Tokens, url: &str, www_authenticate: &str) -> Result<Found> {
    let server = Url::parse(url).map_err(|e| Error::Auth(format!("its URL isn't one: {e}")))?;
    let (named, challenged_scope) = challenge(www_authenticate);
    let mut places = Vec::new();
    places.extend(named);
    let path = server.path().trim_end_matches('/');
    let origin = server.origin().ascii_serialization();
    if !path.is_empty() {
        places.push(format!("{origin}/.well-known/oauth-protected-resource{path}"));
    }
    places.push(format!("{origin}/.well-known/oauth-protected-resource"));
    let metadata = first_json(http, &places, |value| value.get("authorization_servers").and_then(Value::as_array).is_some_and(|servers| !servers.is_empty())).await?;
    let (issuer, resource, scopes) = match &metadata {
        Some(metadata) => {
            let issuer = metadata["authorization_servers"][0].as_str().ok_or_else(|| Error::Auth("its metadata names no sign-in server".into()))?.to_owned();
            let resource = match metadata.get("resource").and_then(Value::as_str) {
                Some(resource) if covers(resource, &server) => resource.to_owned(),
                Some(_) => return Err(Error::Auth("its metadata is for another address than the server's, so rein doesn't sign in there".into())),
                None => canonical(&server),
            };
            let scopes = metadata.get("scopes_supported").and_then(Value::as_array).map(|scopes| scopes.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "));
            (issuer, resource, scopes)
        }
        // An older server that says nothing of itself signs in at its own
        // address, as MCP's spec before this one had it.
        None => (origin.clone(), canonical(&server), None),
    };
    let issuer_url = Url::parse(&issuer).map_err(|e| Error::Auth(format!("its sign-in server isn't a URL: {e}")))?;
    if !may_carry_a_key(&issuer) {
        return Err(Error::Auth(format!("its sign-in server, {issuer}, isn't over https")));
    }
    let as_origin = issuer_url.origin().ascii_serialization();
    let as_path = issuer_url.path().trim_end_matches('/');
    let places = if as_path.is_empty() {
        vec![format!("{as_origin}/.well-known/oauth-authorization-server"), format!("{as_origin}/.well-known/openid-configuration")]
    } else {
        vec![
            format!("{as_origin}/.well-known/oauth-authorization-server{as_path}"),
            format!("{as_origin}/.well-known/openid-configuration{as_path}"),
            format!("{as_origin}{as_path}/.well-known/openid-configuration"),
        ]
    };
    let signs_in = |value: &Value| value.get("authorization_endpoint").is_some() && value.get("token_endpoint").is_some();
    let metadata = first_json(http, &places, signs_in).await?.ok_or_else(|| Error::Auth(format!("rein found no sign-in metadata at {issuer}")))?;
    let text = |name: &str| metadata.get(name).and_then(Value::as_str).map(str::to_owned);
    let pkce = metadata.get("code_challenge_methods_supported").and_then(Value::as_array).is_some_and(|methods| methods.iter().any(|method| method == "S256"));
    if !pkce {
        return Err(Error::Auth(format!("{issuer} doesn't say it takes PKCE, which rein needs to sign in safely")));
    }
    let (authorization_endpoint, token_endpoint) = (text("authorization_endpoint").unwrap_or_default(), text("token_endpoint").unwrap_or_default());
    let registration = text("registration_endpoint");
    for endpoint in [Some(&authorization_endpoint), Some(&token_endpoint), registration.as_ref()].into_iter().flatten() {
        if !may_carry_a_key(endpoint) {
            return Err(Error::Auth(format!("its sign-in goes to {endpoint}, which isn't over https")));
        }
    }
    let registered = tokens.load()?.clients.contains_key(&issuer);
    let scope = challenged_scope.or(scopes).filter(|scope| !scope.is_empty());
    Ok(Found { issuer, resource, authorization_endpoint, token_endpoint, registration, scope, registered })
}

/// Whether `resource`, a protected resource's own name, covers the server
/// at `server`: the same origin, and a path the server's is under.
fn covers(resource: &str, server: &Url) -> bool {
    let Ok(resource) = Url::parse(resource) else { return false };
    let under = server.path().trim_end_matches('/').starts_with(resource.path().trim_end_matches('/'));
    resource.origin() == server.origin() && under
}

/// A server's URL as a token is asked for it: no fragment, and no `/` at
/// the end.
fn canonical(server: &Url) -> String {
    let mut url = server.clone();
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').to_owned()
}

/// The first of `places` that answers with JSON `fits` takes: `None` if
/// none does.
async fn first_json(http: &reqwest::Client, places: &[String], fits: impl Fn(&Value) -> bool) -> Result<Option<Value>> {
    for place in places {
        if !may_carry_a_key(place) {
            continue;
        }
        let Ok(response) = http.get(place).header("Accept", "application/json").timeout(WAIT).send().await else { continue };
        if !response.status().is_success() {
            continue;
        }
        if let Ok(value) = response.json::<Value>().await
            && fits(&value)
        {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
