//! How rein's requests travel: the one HTTP client they all go through,
//! and where a key may be sent at all.

use std::time::Duration;

use reqwest::redirect::{Action, Attempt, Policy};
use reqwest::{Client, Url};

/// How long connecting to a server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How many redirects in a row are followed, all on one server.
const MAX_REDIRECTS: usize = 10;

/// The client for every request rein makes, with `agent` as its user agent.
///
/// It follows a redirect only on the server the request went to. reqwest
/// drops `Authorization` when a redirect leads to another server, but not
/// `x-api-key` or `x-goog-api-key`, which carry Anthropic's and Gemini's
/// keys: a server, or whoever got into it, could send your key on to anyone.
pub fn client(agent: &str) -> reqwest::Result<Client> {
    Client::builder().user_agent(agent).connect_timeout(CONNECT_TIMEOUT).redirect(Policy::custom(same_server)).build()
}

/// Follows `attempt` if it stays on the server the request started at, and
/// stops there otherwise: the redirect comes back as the answer.
fn same_server(attempt: Attempt) -> Action {
    let stays = attempt.previous().first().is_some_and(|first| first.origin() == attempt.url().origin());
    if stays && attempt.previous().len() <= MAX_REDIRECTS { attempt.follow() } else { attempt.stop() }
}

/// Whether a key may be sent to `url`: over https, or over plain http only
/// to this computer (`localhost`, `127.0.0.1`, `::1`), where it crosses no
/// network. Plain http anywhere else can be read by anyone on the way, and
/// what doesn't parse as a URL gets no key either.
pub fn may_carry_a_key(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else { return false };
    match url.scheme() {
        "https" => true,
        "http" => url
            .host_str()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .is_some_and(|host| host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn a_key_goes_over_https_or_to_this_computer_only() {
        for url in ["https://api.anthropic.com/v1", "http://localhost:11434/v1", "http://LOCALHOST:1234", "http://127.0.0.1:8080/v1", "http://127.1.2.3", "http://[::1]:8080/v1"] {
            assert!(may_carry_a_key(url), "{url}");
        }
        for url in
            ["http://api.example.com/v1", "http://192.168.1.5:11434/v1", "http://localhost.example.com/v1", "http://127.0.0.1.example.com", "ftp://localhost", "localhost:1234", ""]
        {
            assert!(!may_carry_a_key(url), "{url}");
        }
    }

    /// A server on port 0 that answers each request with `answer(path)`,
    /// and says which paths it was asked for.
    async fn server(answer: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (asked, heard) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0u8; 4096];
                let n = socket.read(&mut buffer).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buffer[..n]).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
                let _ = asked.send(head.clone());
                let _ = socket.write_all(answer(&path).as_bytes()).await;
            }
        });
        (base, heard)
    }

    fn redirect(to: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

    #[tokio::test]
    async fn a_key_never_follows_a_redirect_to_another_server() {
        let (elsewhere, mut heard_elsewhere) = server(|_| OK.to_owned()).await;
        let to = format!("{elsewhere}/steal");
        let (base, mut heard) = server(move |_| redirect(&to)).await;
        let answer = client("rein-test").unwrap().get(format!("{base}/v1/models")).header("x-api-key", "sk-ant-FAKE").send().await.unwrap();
        assert_eq!(answer.status(), 302, "the redirect comes back as the answer");
        assert!(heard.recv().await.unwrap().contains("x-api-key: sk-ant-FAKE"));
        assert!(heard_elsewhere.try_recv().is_err(), "the other server heard nothing");
    }

    #[tokio::test]
    async fn a_redirect_on_the_same_server_is_followed() {
        let (base, mut heard) = server(|path| if path == "/old" { redirect("/new") } else { OK.to_owned() }).await;
        let answer = client("rein-test").unwrap().get(format!("{base}/old")).header("x-api-key", "sk-ant-FAKE").send().await.unwrap();
        assert_eq!((answer.status().as_u16(), answer.text().await.unwrap().as_str()), (200, "ok"));
        assert!(heard.recv().await.unwrap().starts_with("GET /old "));
        assert!(heard.recv().await.unwrap().starts_with("GET /new "));
    }
}
