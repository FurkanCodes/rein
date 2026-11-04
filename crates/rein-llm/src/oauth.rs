//! What OAuth sign-ins share: random tokens, PKCE, and the page on this
//! computer the browser comes back to.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::{Error, Result};

/// A random version 4 UUID, like `0f8fad5b-d9cb-469f-a165-70867728950e`.
pub(crate) fn new_uuid() -> Result<String> {
    let mut b = [0u8; 16];
    fill_random(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40; // UUID version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

/// 32 random bytes as base64url: 43 characters, valid as a PKCE verifier.
pub(crate) fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|e| Error::Auth(format!("no secure random numbers available: {e}")))
}

/// The PKCE challenge: base64url(SHA-256(verifier)). Only we know the
/// verifier, so a stolen authorization code is useless to anyone else.
pub(crate) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Reads an HTTP request head and returns its target, like
/// `/auth/callback?code=...`.
pub(crate) async fn read_request_target(socket: &mut TcpStream) -> Option<String> {
    // A browser's spare connection may never send anything: it isn't kept.
    tokio::time::timeout(std::time::Duration::from_secs(10), read_head(socket)).await.ok().flatten()
}

async fn read_head(socket: &mut TcpStream) -> Option<String> {
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

pub(crate) async fn respond(socket: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>rein</title>\
         <body style=\"font:16px system-ui,sans-serif;max-width:32em;margin:4em auto\"><p>{message}</p>"
    );
    let response = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// Serves the callback, at `path`, on every address in `listeners` until
/// the real one arrives with `state`: the code it brings. `pages` are what
/// the browser shows once it has, signed in and not. Each connection gets
/// its own task: browsers open spare ones that never send a request. All
/// of them live in `JoinSet`s, which stop every task they hold when
/// dropped: given up on (Esc, a timeout), nothing keeps listening.
pub(crate) async fn callback(listeners: Vec<TcpListener>, state: String, path: &'static str, pages: (&str, &str)) -> Result<String> {
    let (tx, mut rx) = mpsc::channel(1);
    let pages = (pages.0.to_owned(), pages.1.to_owned());
    let mut serving = JoinSet::new();
    for listener in listeners {
        let (tx, state, pages) = (tx.clone(), state.clone(), pages.clone());
        serving.spawn(async move {
            let mut connections = JoinSet::new();
            while let Ok((socket, _)) = listener.accept().await {
                let (tx, state, pages) = (tx.clone(), state.clone(), pages.clone());
                connections.spawn(async move {
                    if let Some(result) = handle_callback(socket, &state, path, &pages).await {
                        let _ = tx.send(result).await;
                    }
                });
            }
        });
    }
    drop(tx);
    rx.recv().await.unwrap_or_else(|| Err(Error::Auth("nothing could wait for the browser".into())))
}

/// Handles one browser request; `None` for ones that aren't the callback.
async fn handle_callback(mut socket: TcpStream, state: &str, path: &str, pages: &(String, String)) -> Option<Result<String>> {
    let target = read_request_target(&mut socket).await?;
    let url = Url::parse(&format!("http://localhost{target}")).ok()?;
    if url.path() != path {
        respond(&mut socket, 404, "Not found.").await;
        return None;
    }
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if query.get("state").map(String::as_str) != Some(state) {
        respond(&mut socket, 400, "This sign-in link has expired. Start again from rein.").await;
        return None;
    }
    let result = match (query.get("code"), query.get("error")) {
        (Some(code), None) => Ok(code.clone()),
        (_, error) => Err(Error::Auth(format!("sign-in wasn't completed ({})", error.map_or("no code came back", String::as_str)))),
    };
    let page = if result.is_ok() { &pages.0 } else { &pages.1 };
    respond(&mut socket, if result.is_ok() { 200 } else { 400 }, page).await;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc_7636_example() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
