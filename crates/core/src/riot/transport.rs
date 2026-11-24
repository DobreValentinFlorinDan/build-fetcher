//! One HTTPS GET to Riot, behind a trait so the client's pacing, 429 handling and count reading can be tested
//! against a simulated Riot (tests drive it on tokio's paused clock, which real sockets can't).
//!
//! The key travels only in the `X-Riot-Token` header. A transport must never put it in an error, a log line or a
//! URL; the HTTP one drops the URL from reqwest's errors too (it carries puuids, which are not secret but are noise).

use anyhow::Result;
use serde_json::Value;
use std::future::Future;

/// The rate-limit headers Build Fetcher reads from every answer (names as Riot sends them, lower-cased).
pub const RATE_HEADERS: [&str; 6] =
    ["x-app-rate-limit", "x-app-rate-limit-count", "x-method-rate-limit", "x-method-rate-limit-count", "x-rate-limit-type", "retry-after"];

/// What came back: the status, the rate-limit headers, and the JSON body of a 2xx.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawAnswer {
    pub status: u16,
    /// (lower-case name, value) for the names in RATE_HEADERS that were present.
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl RawAnswer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub trait Transport: Send + Sync + 'static {
    /// GET `url` with `key` in X-Riot-Token. Errors are network failures only (any HTTP status is an answer).
    fn get(&self, url: &str, key: &str) -> impl Future<Output = Result<RawAnswer>> + Send;
}

/// The real thing: reqwest over rustls, gzip on.
pub struct HttpTransport {
    http: reqwest::Client,
}

impl Default for HttpTransport {
    fn default() -> Self {
        HttpTransport { http: reqwest::Client::builder().gzip(true).build().expect("http client") }
    }
}

impl Transport for HttpTransport {
    async fn get(&self, url: &str, key: &str) -> Result<RawAnswer> {
        let res = self.http.get(url).header("X-Riot-Token", key).send().await.map_err(|e| anyhow::anyhow!(e.without_url()))?;
        let status = res.status().as_u16();
        let headers = RATE_HEADERS
            .iter()
            .filter_map(|k| res.headers().get(*k).and_then(|v| v.to_str().ok()).map(|v| (k.to_string(), v.to_string())))
            .collect();
        let body = if (200..300).contains(&status) { Some(res.json::<Value>().await.map_err(|e| anyhow::anyhow!(e.without_url()))?) } else { None };
        Ok(RawAnswer { status, headers, body })
    }
}
