//! What rein-mcp needs to reach a server at a URL, given from rein's own:
//! its HTTP client, which follows a redirect only on the server it went
//! to, and the sign-ins rein-llm keeps.

use std::sync::Arc;

use rein_mcp::remote::{Body, Fut, Method, Request, Response, Tokens, Web};

/// rein's HTTP client, for rein-mcp.
pub(crate) struct Http(pub(crate) rein_llm::reqwest::Client);

impl Web for Http {
    fn send(&self, request: Request) -> Fut<'_, Result<Response, String>> {
        Box::pin(async move {
            let mut builder = match request.method {
                Method::Get => self.0.get(&request.url),
                Method::Post => self.0.post(&request.url),
            };
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder.send().await.map_err(|e| crate::with_causes(&e))?;
            let status = response.status().as_u16();
            let headers = response.headers().iter().filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))).collect();
            Ok(Response { status, headers, body: Box::new(Chunks(response)) })
        })
    }
}

/// An answer's body, read as it comes.
struct Chunks(rein_llm::reqwest::Response);

impl Body for Chunks {
    fn chunk(&mut self) -> Fut<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move { self.0.chunk().await.map(|chunk| chunk.map(|bytes| bytes.to_vec())).map_err(|e| crate::with_causes(&e)) })
    }
}

/// The sign-ins rein keeps, for rein-mcp.
pub(crate) struct SignIns(pub(crate) Arc<rein_llm::mcp::Tokens>);

impl Tokens for SignIns {
    fn token<'a>(&'a self, url: &'a str) -> Fut<'a, Result<Option<String>, String>> {
        Box::pin(async move { self.0.token(url).await.map_err(crate::describe) })
    }

    fn renew<'a>(&'a self, url: &'a str, rejected: &'a str) -> Fut<'a, Result<Option<String>, String>> {
        Box::pin(async move { self.0.renew(url, rejected).await.map_err(crate::describe) })
    }
}
