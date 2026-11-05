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

use std::time::{Duration, SystemTime};

use crate::client::check;
use crate::meter::{Meter, Reading, Standing};
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

const DAY: u64 = 86_400;

/// What OpenRouter's `GET /key` (`key`) says of this key, as meters: the
/// budget that turns requests away first, or else the key's own cap, or
/// else, with no cap at all, what it spent today; and on a free model,
/// today's free requests. What's left on the account isn't for a key to
/// ask (`/credits` takes a management key), so it isn't shown.
/// Spec: <https://openrouter.ai/docs/api/reference/limits>
pub(crate) fn meters(key: &Value, model: &str, now: SystemTime) -> Vec<Meter> {
    let data = &key["data"];
    let budget = &data["effective_budget"];
    let left = |cap: &Value, left: &Value| match (cap.as_f64(), left.as_f64()) {
        (Some(cap), Some(left)) if cap.is_finite() && left.is_finite() => Some(Reading::Left { dollars: left.max(0.0), cap }),
        _ => None,
    };
    let mut meters = Vec::new();
    if let Some(reading) = left(&budget["limit_usd"], &budget["remaining_usd"]) {
        let resets = budget["resets_at"].as_str().and_then(utc);
        meters.push(Meter { id: "budget".into(), only: None, span: span(&budget["reset_interval"]), resets, reading, standing: Standing::of(reading) });
    } else if let Some(reading) = left(&data["limit"], &data["limit_remaining"]) {
        meters.push(Meter { id: "key_limit".into(), only: None, span: span(&data["limit_reset"]), resets: None, reading, standing: Standing::of(reading) });
    } else if let Some(today) = data["usage_daily"].as_f64().filter(|spent| spent.is_finite()) {
        let reading = Reading::Spent(today.max(0.0));
        meters.push(Meter { id: "spent_today".into(), only: None, span: Some(Duration::from_secs(DAY)), resets: None, reading, standing: Standing::Fine });
    }
    let free = &data["free_model_daily_requests"];
    if model.ends_with(":free")
        && let (Some(made), Some(cap)) = (free["used"].as_u64(), free["limit"].as_u64())
    {
        let reading = Reading::Requests { made, cap };
        let midnight = now.duration_since(SystemTime::UNIX_EPOCH).ok().map(|since| Duration::from_secs((since.as_secs() / DAY).saturating_add(1).saturating_mul(DAY)));
        let resets = midnight.and_then(|at| SystemTime::UNIX_EPOCH.checked_add(at));
        meters.push(Meter { id: "free_requests".into(), only: Some("free".into()), span: Some(Duration::from_secs(DAY)), resets, reading, standing: Standing::of(reading) });
    }
    meters
}

/// How long a budget runs, from how OpenRouter says it resets.
fn span(interval: &Value) -> Option<Duration> {
    let days = match interval.as_str()? {
        "daily" => 1,
        "weekly" => 7,
        "monthly" => 30,
        _ => return None,
    };
    Some(Duration::from_secs(days * DAY))
}

/// An ISO 8601 time, in UTC or with an offset, as `resets_at` is sent.
fn utc(text: &str) -> Option<SystemTime> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|n| n.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let (clock, offset) = time.find(['Z', 'z', '+', '-']).map_or((time, ""), |at| time.split_at(at));
    let mut clock = clock.split('.').next()?.splitn(3, ':').map(|n| n.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next().flatten().unwrap_or(0));
    let offset = match offset.as_bytes().first() {
        None | Some(b'Z' | b'z') => 0,
        Some(sign) => {
            let (hours, minutes) = offset[1..].split_once(':').unwrap_or((&offset[1..], "0"));
            let seconds = hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60;
            if *sign == b'-' { -seconds } else { seconds }
        }
    };
    let sane = (1970..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60 && second <= 60;
    if !sane {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(seconds).ok()?))
}

/// Days from 1970-01-01 to a date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year - 719_468
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

    #[test]
    fn a_keys_budget_comes_first_then_its_cap_then_what_it_spent_today() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_880_000);
        let budgeted = json!({"data": {
            "limit": 10.0, "limit_remaining": 9.0, "usage_daily": 0.4,
            "effective_budget": {"scope": "workspace", "limit_usd": 20.0, "spend_usd": 16.0, "remaining_usd": 4.0, "reset_interval": "monthly", "resets_at": "2026-12-01T00:00:00Z"},
        }});
        let budget = meters(&budgeted, "anthropic/claude-sonnet-5-5", now);
        assert_eq!(budget.len(), 1, "{budget:?}");
        assert_eq!(budget[0].reading, Reading::Left { dollars: 4.0, cap: 20.0 });
        assert_eq!((budget[0].standing, budget[0].label().as_str()), (Standing::Warned, "mo"), "80% of it is spent");
        assert_eq!(budget[0].resets, utc("2026-12-01T00:00:00Z"));
        let capped = json!({"data": {"limit": 10.0, "limit_remaining": 9.0, "limit_reset": "weekly", "effective_budget": null}});
        assert_eq!(meters_of(&capped, now), [(Reading::Left { dollars: 9.0, cap: 10.0 }, Standing::Fine)]);
        let uncapped = json!({"data": {"limit": null, "limit_remaining": null, "usage_daily": 0.42}});
        assert_eq!(meters_of(&uncapped, now), [(Reading::Spent(0.42), Standing::Fine)]);
        assert!(meters(&json!({"error": "nope"}), "m", now).is_empty(), "nothing said, nothing shown");
    }

    fn meters_of(key: &Value, now: SystemTime) -> Vec<(Reading, Standing)> {
        meters(key, "m", now).into_iter().map(|meter| (meter.reading, meter.standing)).collect()
    }

    #[test]
    fn free_requests_count_on_a_free_model_and_start_over_at_midnight_utc() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_880_000);
        let key = json!({"data": {"usage_daily": 0.0, "free_model_daily_requests": {"used": 50, "limit": 50, "remaining": 0}}});
        assert_eq!(meters(&key, "deepseek/deepseek-v4:free", now).len(), 2);
        let free = &meters(&key, "deepseek/deepseek-v4:free", now)[1];
        assert_eq!((free.reading, free.standing), (Reading::Requests { made: 50, cap: 50 }, Standing::Reached));
        assert_eq!(free.resets, utc("2026-11-06T00:00:00Z"));
        assert_eq!(meters(&key, "deepseek/deepseek-v4", now).len(), 1, "a paid model isn't counted against them");
    }

    #[test]
    fn a_reset_time_is_read_in_utc_whatever_its_offset() {
        let at = utc("2026-11-05T21:00:00Z").unwrap();
        assert_eq!(at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(), 1_793_912_400);
        assert_eq!(utc("2026-11-06T00:00:00.000+03:00"), Some(at));
        assert_eq!(utc("2026-11-05T20:00:00-01:00"), Some(at));
        for wrong in ["", "tomorrow", "2026-13-01T00:00:00Z", "2026-11-05", "99999999999-01-01T00:00:00Z", "2026-11-05T25:00:00Z", "1969-12-31T23:59:59Z"] {
            assert_eq!(utc(wrong), None, "{wrong}");
        }
    }
}
