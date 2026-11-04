use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A request a fake server got: `GET /path`, and its body.
#[derive(Debug, Clone)]
struct Got {
    asked: String,
    body: String,
}

type Answer = dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync;

/// A sign-in server, and the MCP server's metadata, on this computer:
/// `answer` answers each request, given the server's base URL, `GET
/// /path` and the body; what it was asked is kept.
async fn server(answer: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<Got>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (got, seen) = (Arc::new(Mutex::new(Vec::new())), base.clone());
    let kept = got.clone();
    let answer: Arc<Answer> = Arc::new(answer);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (got, answer, base) = (kept.clone(), answer.clone(), seen.clone());
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let end = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end;
                    }
                };
                let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                let length: usize = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|n| n.trim().parse().unwrap_or(0))).unwrap_or(0);
                while bytes.len() < end + 4 + length {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&bytes[end + 4..]).into_owned();
                let mut line = head.lines().next().unwrap_or_default().split_whitespace();
                let asked = format!("{} {}", line.next().unwrap_or_default(), line.next().unwrap_or_default().split('?').next().unwrap_or_default());
                got.lock().unwrap().push(Got { asked: asked.clone(), body: body.clone() });
                let (status, text) = answer(&base, &asked, &body);
                let response = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (base, got)
}

/// What a sign-in server's metadata says, at `base`: with S256 or not, and
/// a registration endpoint or not.
fn metadata(base: &str, issuer: &str, pkce: bool, registers: bool) -> String {
    let mut metadata = json!({ "issuer": issuer, "authorization_endpoint": format!("{base}/authorize"), "token_endpoint": format!("{base}/token") });
    if pkce {
        metadata["code_challenge_methods_supported"] = json!(["S256"]);
    }
    if registers {
        metadata["registration_endpoint"] = json!(format!("{base}/register"));
    }
    metadata.to_string()
}

/// The fields of a form, as a server reads them.
fn form(body: &str) -> BTreeMap<String, String> {
    Url::parse(&format!("http://x/?{body}")).unwrap().query_pairs().into_owned().collect()
}

fn tokens(name: &str) -> Tokens {
    let dir = std::env::temp_dir().join(format!("rein-mcp-signin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Tokens::new(dir.join("mcp-tokens.json"), reqwest::Client::new())
}

/// Goes back to `link`'s `redirect_uri`, as a browser would once you've
/// signed in, with `code` and `state`.
async fn come_back(link: &str, code: &str, state: &str) -> String {
    let link = Url::parse(link).unwrap();
    let redirect = link.query_pairs().find(|(name, _)| name == "redirect_uri").unwrap().1.into_owned();
    let back = Url::parse(&redirect).unwrap();
    let mut socket = TcpStream::connect(("127.0.0.1", back.port().unwrap())).await.unwrap();
    socket.write_all(format!("GET {}?code={code}&state={state} HTTP/1.1\r\nHost: x\r\n\r\n", back.path()).as_bytes()).await.unwrap();
    let mut answer = String::new();
    socket.read_to_string(&mut answer).await.unwrap();
    answer
}

fn param(link: &str, name: &str) -> String {
    Url::parse(link).unwrap().query_pairs().find(|(own, _)| own == name).map(|(_, value)| value.into_owned()).unwrap_or_default()
}

/// A server whose sign-in is all as it should be, at `/auth`: metadata
/// first at the second place the spec gives, tokens renewed once.
async fn good() -> (String, Arc<Mutex<Vec<Got>>>) {
    // A refresh token works once.
    let spent = std::sync::atomic::AtomicBool::new(false);
    server(move |base, asked, body| match asked {
        "GET /prm" => (200, json!({ "resource": format!("{base}/mcp"), "authorization_servers": [format!("{base}/auth")], "scopes_supported": ["read", "write"] }).to_string()),
        "GET /.well-known/openid-configuration/auth" => (200, metadata(base, &format!("{base}/auth"), true, true)),
        "POST /register" => (201, json!({ "client_id": "rein-1" }).to_string()),
        "POST /token" if form(body)["grant_type"] == "authorization_code" => {
            (200, json!({ "access_token": "FAKE-a1", "refresh_token": "FAKE-r1", "expires_in": 3600 }).to_string())
        }
        "POST /token" if form(body)["refresh_token"] == "FAKE-r1" && !spent.swap(true, std::sync::atomic::Ordering::SeqCst) => {
            (200, json!({ "access_token": "FAKE-a2", "expires_in": 3600 }).to_string())
        }
        "POST /token" => (400, json!({ "error": "invalid_grant" }).to_string()),
        _ => (404, "{}".into()),
    })
    .await
}

#[tokio::test]
async fn rein_registers_itself_and_signs_in_with_pkce_for_this_server_alone() {
    let (base, got) = good().await;
    let tokens = tokens("signs-in");
    let url = format!("{base}/mcp");
    let found = discover(&tokens.http, &tokens, &url, &format!(r#"Bearer resource_metadata="{base}/prm", scope="files:read""#)).await.unwrap();
    assert_eq!((found.resource.as_str(), found.scope.as_deref(), found.registers()), (url.as_str(), Some("files:read"), true), "the challenge's scope over the metadata's");
    let pending = tokens.begin(&url, &found, None).await.unwrap();
    let link = pending.link.clone();
    assert!(link.starts_with(&format!("{base}/authorize?")), "{link}");
    assert_eq!((param(&link, "client_id"), param(&link, "code_challenge_method"), param(&link, "resource")), ("rein-1".into(), "S256".into(), url.clone()));
    let registered: Value = serde_json::from_str(&got.lock().unwrap().iter().find(|got| got.asked == "POST /register").unwrap().body).unwrap();
    assert_eq!((registered["client_name"].as_str(), registered["token_endpoint_auth_method"].as_str()), (Some("rein"), Some("none")), "as itself, never another app");
    assert_eq!(registered["redirect_uris"][0], param(&link, "redirect_uri"));

    let browser = async {
        assert!(come_back(&link, "c0", "an-old-tab").await.contains("400"), "a stale tab's code isn't taken");
        come_back(&link, "c1", &param(&link, "state")).await
    };
    let (finished, page) = tokio::join!(pending.finish(&tokens), browser);
    finished.unwrap();
    assert!(page.contains("rein is signed in"), "{page}");
    let traded = form(&got.lock().unwrap().iter().find(|got| got.asked == "POST /token").unwrap().body);
    assert_eq!((traded["code"].as_str(), traded["resource"].as_str(), traded["client_id"].as_str()), ("c1", url.as_str(), "rein-1"));
    assert_eq!(pkce_challenge(&traded["code_verifier"]), param(&link, "code_challenge"), "the verifier only rein knew");
    assert_eq!(tokens.token(&url).await.unwrap().as_deref(), Some("FAKE-a1"));
    assert!(tokens.signed_in(&url) && !tokens.signed_in(&format!("{base}/other")), "a token is for its server alone");
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&tokens.path).unwrap().permissions()) & 0o777, 0o600);
    assert!(!format!("{tokens:?} {found:?}").contains("FAKE"));

    // An hour on, it's renewed before it's sent, and its refresh token kept.
    let mut file: Value = serde_json::from_slice(&std::fs::read(&tokens.path).unwrap()).unwrap();
    file["servers"][&url]["expires_at"] = json!(0);
    std::fs::write(&tokens.path, file.to_string()).unwrap();
    assert_eq!(tokens.token(&url).await.unwrap().as_deref(), Some("FAKE-a2"));
    let renewal = form(&got.lock().unwrap().iter().rfind(|got| got.asked == "POST /token").unwrap().body);
    assert_eq!((renewal["grant_type"].as_str(), renewal["resource"].as_str()), ("refresh_token", url.as_str()));
    // Turned down, it needs signing in again.
    assert_eq!(tokens.renew(&url, "FAKE-a2").await.unwrap(), None);
    assert!(!tokens.signed_in(&url));
    // The client rein registered is kept: signing in again doesn't register again.
    let again = discover(&tokens.http, &tokens, &url, &format!(r#"Bearer resource_metadata="{base}/prm""#)).await.unwrap();
    assert_eq!(again.scope.as_deref(), Some("read write"), "without the challenge's, the metadata's");
    let pending = tokens.begin(&url, &again, None).await.unwrap();
    assert_eq!(param(&pending.link, "client_id"), "rein-1");
    assert_eq!(got.lock().unwrap().iter().filter(|got| got.asked == "POST /register").count(), 1);
}

#[tokio::test]
async fn two_windows_whose_token_runs_out_together_renew_it_once() {
    let (base, got) = good().await;
    let one = tokens("two-windows");
    let other = Tokens::new(one.path.clone(), reqwest::Client::new());
    let url = format!("{base}/mcp");
    let client = Client { client_id: "rein-1".into(), client_secret: None, auth_method: None, redirect_uri: "http://127.0.0.1:1/callback".into() };
    let signed = Signed {
        resource: url.clone(),
        token_endpoint: format!("{base}/token"),
        client,
        access_token: "FAKE-a1".into(),
        refresh_token: Some("FAKE-r1".into()),
        expires_at: Some(0),
    };
    one.keep(&url, signed).await.unwrap();
    let (a, b) = tokio::join!(one.token(&url), other.token(&url));
    assert_eq!((a.unwrap(), b.unwrap()), (Some("FAKE-a2".to_owned()), Some("FAKE-a2".to_owned())));
    assert_eq!(got.lock().unwrap().iter().filter(|got| got.asked == "POST /token").count(), 1, "a refresh token works once");
}

#[tokio::test]
async fn discovery_goes_where_the_spec_says_and_turns_down_what_isnt_safe() {
    let (base, got) = server(|base, asked, _| match asked {
        "GET /.well-known/oauth-protected-resource" => (200, json!({ "authorization_servers": [base] }).to_string()),
        "GET /.well-known/oauth-authorization-server" => (200, metadata(base, base, true, false)),
        _ => (404, "{}".into()),
    })
    .await;
    let tokens = tokens("discovery");
    let url = format!("{base}/v1/mcp");
    let found = discover(&tokens.http, &tokens, &url, "Bearer").await.unwrap();
    let asked: Vec<String> = got.lock().unwrap().iter().map(|got| got.asked.clone()).collect();
    assert_eq!(asked, ["GET /.well-known/oauth-protected-resource/v1/mcp", "GET /.well-known/oauth-protected-resource", "GET /.well-known/oauth-authorization-server"]);
    assert_eq!((found.resource.as_str(), found.registers()), (url.as_str(), false), "the server's own URL, and no way to register");
    assert!(tokens.begin(&url, &found, None).await.unwrap_err().to_string().contains("client ID"));
    let yours = tokens.begin(&url, &found, Some(("your-client".into(), None))).await.unwrap();
    assert_eq!(param(&yours.link, "client_id"), "your-client", "the one you registered");

    let (base, _) = server(|base, asked, _| match asked {
        "GET /.well-known/oauth-protected-resource/mcp" => (200, json!({ "authorization_servers": [base] }).to_string()),
        "GET /.well-known/oauth-authorization-server" => (200, metadata(base, base, false, true)),
        _ => (404, "{}".into()),
    })
    .await;
    let e = discover(&tokens.http, &tokens, &format!("{base}/mcp"), "Bearer").await.unwrap_err();
    assert!(e.to_string().contains("PKCE"), "{e}");

    let (base, _) = server(|base, asked, _| match asked {
        "GET /.well-known/oauth-protected-resource/mcp" => (200, json!({ "resource": "https://elsewhere.example/mcp", "authorization_servers": [base] }).to_string()),
        _ => (404, "{}".into()),
    })
    .await;
    let e = discover(&tokens.http, &tokens, &format!("{base}/mcp"), "Bearer").await.unwrap_err();
    assert!(e.to_string().contains("another address"), "a token for one server isn't asked for another's: {e}");
}

#[test]
fn a_challenge_is_read_as_it_comes() {
    assert_eq!(
        challenge(r#"Bearer error="insufficient_scope", scope="files:read files:write", resource_metadata="https://x.example/.well-known/oauth-protected-resource""#),
        (Some("https://x.example/.well-known/oauth-protected-resource".to_owned()), Some("files:read files:write".to_owned()))
    );
    assert_eq!(challenge("Bearer realm=x,resource_metadata=https://y.example/m"), (Some("https://y.example/m".to_owned()), None));
    assert_eq!(challenge(""), (None, None));
}
