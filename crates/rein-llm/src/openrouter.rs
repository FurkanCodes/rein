//! Signing in to OpenRouter, the official way any app may: OAuth with PKCE.
//! The browser asks you, OpenRouter answers with an API key of your own for
//! rein, and that key is billed to your OpenRouter credits. You can see or
//! delete it in OpenRouter's settings like any other key.
//!
//! 1. Listen on a free port on this computer, and open the browser at
//!    OpenRouter's page with a PKCE challenge and a `state`.
//! 2. The browser comes back to `http://localhost:<port>/callback` with a
//!    one-time code, good for ten minutes.
//! 3. Trade the code and the verifier for the key.
//!
//! Spec: <https://openrouter.ai/docs/use-cases/oauth-pkce>

use reqwest::Url;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::client::check;
use crate::oauth::{callback, pkce_challenge, random_token};
use crate::{Error, Result};

pub const AUTH_URL: &str = "https://openrouter.ai/auth";
const CALLBACK_PATH: &str = "/callback";

/// A sign-in waiting for the browser to come back.
pub struct PendingSignIn {
    /// Open this in the browser.
    pub url: String,
    /// Where the browser comes back to: `localhost` may mean either
    /// address, so both are listened on where they can be.
    listeners: Vec<TcpListener>,
    state: String,
    verifier: String,
}

/// Starts a sign-in at `auth_url` (OpenRouter's, but for tests); `label`
/// names the key it makes.
pub async fn begin(auth_url: &str, label: &str) -> Result<PendingSignIn> {
    let v4 = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    if let Ok(v6) = TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    let (state, verifier) = (random_token()?, random_token()?);
    let callback = format!("http://localhost:{port}{CALLBACK_PATH}");
    let params = [("callback_url", callback.as_str()), ("code_challenge", &pkce_challenge(&verifier)), ("code_challenge_method", "S256"), ("state", &state), ("key_label", label)];
    let url = Url::parse_with_params(auth_url, &params).map_err(|e| Error::Auth(format!("bad sign-in URL: {e}")))?.to_string();
    Ok(PendingSignIn { url, listeners, state, verifier })
}

impl PendingSignIn {
    /// Waits for the browser, then trades the code for a key at `api_base`
    /// (`https://openrouter.ai/api/v1`).
    pub async fn finish(self, http: &reqwest::Client, api_base: &str) -> Result<String> {
        let pages = (
            "rein is connected to OpenRouter. You can close this tab and go back to the terminal.",
            "OpenRouter wasn't connected. You can close this tab and go back to the terminal.",
        );
        let code = callback(self.listeners, self.state, CALLBACK_PATH, pages).await?;
        let body = json!({ "code": code, "code_verifier": self.verifier, "code_challenge_method": "S256" });
        let response = http.post(format!("{api_base}/auth/keys")).json(&body).send().await?;
        let answer: Value = check(response).await?.json().await?;
        answer["key"].as_str().map(str::to_owned).ok_or_else(|| Error::Auth("OpenRouter's answer had no key".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// One HTTP request to `url`, like a browser's; the answer.
    async fn get(url: &str) -> String {
        let url = Url::parse(url).unwrap();
        let mut socket = TcpStream::connect(("127.0.0.1", url.port().unwrap())).await.unwrap();
        let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
        socket.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();
        let mut answer = String::new();
        socket.read_to_string(&mut answer).await.unwrap();
        answer
    }

    #[tokio::test]
    async fn trades_the_browsers_code_for_a_key() {
        let pending = begin(AUTH_URL, "rein").await.unwrap();
        let url = Url::parse(&pending.url).unwrap();
        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).unwrap().1.into_owned();
        assert_eq!((param("code_challenge_method"), param("key_label")), ("S256".into(), "rein".into()));
        let callback = param("callback_url");
        assert!(callback.starts_with("http://localhost:") && callback.ends_with("/callback"), "{callback}");
        let (state, challenge) = (param("state"), param("code_challenge"));

        // OpenRouter's key endpoint, which checks the verifier against the challenge.
        let keys = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let api_base = format!("http://127.0.0.1:{}/api/v1", keys.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            let (mut socket, _) = keys.accept().await.unwrap();
            let mut request = vec![0u8; 8192];
            let n = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]).into_owned();
            let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert!(request.starts_with("POST /api/v1/auth/keys "), "{request}");
            assert_eq!((body["code"].as_str(), body["code_challenge_method"].as_str()), (Some("c1"), Some("S256")));
            assert_eq!(pkce_challenge(body["code_verifier"].as_str().unwrap()), challenge, "the verifier matches");
            let answer = r#"{"key":"sk-or-v1-test","user_id":"u"}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{answer}", answer.len()).as_bytes()).await.unwrap();
        });

        let browser = async {
            // A tab from an earlier attempt is turned away; the real one is taken.
            assert!(get(&format!("{callback}?code=old&state=stale")).await.starts_with("HTTP/1.1 400"));
            assert!(get(&format!("{callback}?code=c1&state={state}")).await.starts_with("HTTP/1.1 200"));
        };
        let http = reqwest::Client::new();
        let (key, ()) = tokio::join!(pending.finish(&http, &api_base), browser);
        assert_eq!(key.unwrap(), "sk-or-v1-test");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_sign_in_given_up_on_stops_listening() {
        let pending = begin(AUTH_URL, "rein").await.unwrap();
        let url = Url::parse(&pending.url).unwrap();
        let callback = Url::parse(&url.query_pairs().find(|(k, _)| k == "callback_url").unwrap().1).unwrap();
        let port = callback.port().unwrap();
        let http = reqwest::Client::new();
        // Esc: the wait is dropped.
        let given_up = tokio::time::timeout(std::time::Duration::from_millis(100), pending.finish(&http, "http://127.0.0.1:9")).await;
        assert!(given_up.is_err());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err(), "the old tab can still reach rein");
    }

    #[tokio::test]
    async fn a_sign_in_turned_down_says_so() {
        let pending = begin(AUTH_URL, "rein").await.unwrap();
        let url = Url::parse(&pending.url).unwrap();
        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).unwrap().1.into_owned();
        let (callback, state) = (param("callback_url"), param("state"));
        let browser = async { get(&format!("{callback}?error=access_denied&state={state}")).await };
        let http = reqwest::Client::new();
        let (result, page) = tokio::join!(pending.finish(&http, "http://127.0.0.1:9"), browser);
        assert!(page.starts_with("HTTP/1.1 400"));
        assert!(matches!(result, Err(Error::Auth(message)) if message.contains("access_denied")));
    }
}
