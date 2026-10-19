//! HTTP transport shared by adapters: pooled client, TLS via rustls, SSE byte streaming
//! with idle timeouts, and error classification that distinguishes definitely-not-sent
//! from sent-but-failed.

use crate::sse::{FeedError, SseFrame, SseParser};
use crate::ProviderError;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct Endpoint {
    /// Base URL including version path, e.g. `https://api.example.com/v1`.
    pub base_url: String,
    /// Bearer token, if any. Local endpoints may omit it.
    pub api_key: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub origin: String,
}

impl Endpoint {
    pub fn from_parts(
        base_url: &str,
        api_key: Option<String>,
        headers: &std::collections::BTreeMap<String, String>,
        timeout: Duration,
    ) -> Endpoint {
        let origin = base_url
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or(base_url)
            .to_string();
        Endpoint {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            extra_headers: headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            timeout,
            origin,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

pub struct HttpTransport {
    client: reqwest::Client,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new().expect("tls-backed http client")
    }
}

impl HttpTransport {
    pub fn new() -> Result<HttpTransport, ProviderError> {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .pool_idle_timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(15))
            // Redirects never carry credentials: we send auth per request and pin to the
            // same origin policy — cross-origin redirects are refused by reqwest policy.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 {
                    attempt.error("too many redirects")
                } else if attempt.url().origin() != attempt.previous()[0].origin() {
                    attempt.error("cross-origin redirect refused (credentials never cross origins)")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| ProviderError::Connect(e.to_string()))?;
        Ok(HttpTransport { client })
    }

    /// POST and return a live SSE stream. Errors before the response are classified as
    /// not-sent; non-2xx statuses are classified from the body.
    pub async fn post_sse(
        &self,
        endpoint: &Endpoint,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<SseStream, ProviderError> {
        let url = endpoint.url(path);
        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .timeout(endpoint.timeout);
        if let Some(key) = &endpoint.api_key {
            req = req.bearer_auth(key);
        }
        for (k, v) in &endpoint.extra_headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.json(body).send().await.map_err(|e| {
            if e.is_connect() {
                ProviderError::Connect(e.to_string())
            } else if e.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Connect(e.to_string())
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::from_status(status.as_u16(), &body_text));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        Ok(SseStream {
            resp,
            parser: SseParser::new(),
            content_type,
            endpoint_origin: endpoint.origin.clone(),
        })
    }
}

/// Merges the HTTP response chunk stream with the incremental SSE parser.
pub struct SseStream {
    resp: reqwest::Response,
    parser: SseParser,
    pub content_type: String,
    pub endpoint_origin: String,
}

impl SseStream {
    /// Next complete frame, with an idle timeout. `None` at clean EOF (after flushing any
    /// trailing frame). Mid-stream failures map to [`ProviderError::Dropped`] — the
    /// request *was* sent, so its remote outcome is unknown.
    pub async fn next_frame(&mut self, idle: Duration) -> Result<Option<SseFrame>, ProviderError> {
        loop {
            match tokio::time::timeout(idle, self.resp.chunk()).await {
                // No bytes within the idle window.
                Err(_elapsed) => return Err(ProviderError::Timeout),
                // Transport error mid-body: sent, outcome unknown.
                Ok(Err(e)) => return Err(ProviderError::Dropped(e.to_string())),
                // Clean EOF: flush the parser, then end.
                Ok(Ok(None)) => {
                    if let Some(frame) = self.parser.finish().into_iter().flatten().next() {
                        return Ok(Some(frame));
                    }
                    return Ok(None);
                }
                Ok(Ok(Some(bytes))) => {
                    for f in self.parser.feed(&bytes) {
                        match f {
                            Ok(frame) => return Ok(Some(frame)),
                            Err(FeedError::InvalidUtf8) => {
                                tracing::warn!(origin = %self.endpoint_origin, "skipping SSE frame with invalid UTF-8");
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn is_json_not_sse(&self) -> bool {
        self.content_type.contains("application/json")
            && !self.content_type.contains("event-stream")
    }

    /// Drain the whole body as JSON (endpoints that ignore `stream: true`).
    pub async fn body_json(&mut self) -> Result<serde_json::Value, ProviderError> {
        let mut buf = Vec::new();
        loop {
            match self.resp.chunk().await {
                Ok(Some(b)) => buf.extend_from_slice(&b),
                Ok(None) => break,
                Err(e) => return Err(ProviderError::Dropped(e.to_string())),
            }
        }
        serde_json::from_slice(&buf)
            .map_err(|e| ProviderError::Malformed(format!("non-SSE JSON body: {e}")))
    }
}

pub fn shared_transport() -> Arc<HttpTransport> {
    Arc::new(HttpTransport::default())
}
