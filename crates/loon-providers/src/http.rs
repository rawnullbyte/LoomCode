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
        let base_url = normalize_base_url(base_url);
        let origin = base_url
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or(&base_url)
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
            let retry_after = retry_after_header(resp.headers());
            let body_text = resp.text().await.unwrap_or_default();
            return Err(with_retry_after(
                ProviderError::from_status(status.as_u16(), &body_text),
                retry_after,
            ));
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
            pending: std::collections::VecDeque::new(),
            eof: false,
            content_type,
            endpoint_origin: endpoint.origin.clone(),
        })
    }
}

/// `Retry-After` in seconds or as an HTTP date; only the seconds form is used (the date
/// form is treated as "wait the default backoff").
fn retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| secs.min(300))
}

/// Attach the server's requested wait to the error message the runner parses.
fn with_retry_after(error: ProviderError, secs: Option<u64>) -> ProviderError {
    match (error, secs) {
        (ProviderError::RateLimited(m), Some(s)) => {
            ProviderError::RateLimited(format!("{m} (retry_after={s}s)"))
        }
        (e, _) => e,
    }
}

/// Merges the HTTP response chunk stream with the incremental SSE parser.
pub struct SseStream {
    resp: reqwest::Response,
    parser: SseParser,
    pending: std::collections::VecDeque<SseFrame>,
    eof: bool,
    pub content_type: String,
    pub endpoint_origin: String,
}

impl SseStream {
    /// Next complete frame, with an idle timeout. `None` at clean EOF (after flushing any
    /// trailing frame). Mid-stream failures map to [`ProviderError::Dropped`] — the
    /// request *was* sent, so its remote outcome is unknown.
    pub async fn next_frame(&mut self, idle: Duration) -> Result<Option<SseFrame>, ProviderError> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            if self.eof {
                return Ok(None);
            }
            match tokio::time::timeout(idle, self.resp.chunk()).await {
                // No bytes within the idle window.
                Err(_elapsed) => return Err(ProviderError::Timeout),
                // Transport error mid-body: sent, outcome unknown.
                Ok(Err(e)) => return Err(ProviderError::Dropped(e.to_string())),
                // Clean EOF: flush the parser, then end.
                Ok(Ok(None)) => {
                    self.pending
                        .extend(self.parser.finish().into_iter().flatten());
                    self.eof = true;
                }
                Ok(Ok(Some(bytes))) => {
                    for f in self.parser.feed(&bytes) {
                        match f {
                            Ok(frame) => self.pending.push_back(frame),
                            Err(FeedError::InvalidUtf8) => {
                                tracing::warn!(origin = %self.endpoint_origin, "skipping SSE frame with invalid UTF-8");
                            }
                            Err(FeedError::FrameTooLarge) => {
                                // Fail loudly: silently dropping a runaway frame would
                                // turn a broken server into an empty answer the user
                                // cannot explain.
                                return Err(ProviderError::Malformed(format!(
                                    "SSE frame exceeded {} bytes without a blank-line terminator",
                                    crate::sse::MAX_FRAME_BYTES
                                )));
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

impl HttpTransport {
    /// GET `{base_url}/models` and return the advertised model ids.
    ///
    /// This is the [OI]-standard discovery endpoint: adding a provider should not
    /// require typing model names. Servers that omit it (or return a shape we do not
    /// recognize) are reported as such — never silently treated as "no models".
    pub async fn list_models(&self, endpoint: &Endpoint) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/models", endpoint.base_url.trim_end_matches('/'));
        let mut ids = Vec::new();
        let mut after: Option<String> = None;
        let mut cursors = std::collections::BTreeSet::new();
        for _ in 0..100 {
            let mut req = self.client.get(&url).timeout(endpoint.timeout);
            if let Some(key) = &endpoint.api_key {
                req = req.bearer_auth(key);
            }
            for (k, v) in &endpoint.extra_headers {
                req = req.header(k, v);
            }
            if let Some(cursor) = &after {
                req = req.query(&[("after", cursor)]);
            }
            let mut resp = req.send().await.map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Connect(format!("GET {url}: {e}"))
                }
            })?;
            let status = resp.status();
            let mut bytes = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| ProviderError::Dropped(e.to_string()))?
            {
                if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                    return Err(ProviderError::Malformed(
                        "model catalog exceeds 8 MiB per page".into(),
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                return Err(with_retry_after(
                    ProviderError::from_status(status.as_u16(), &String::from_utf8_lossy(&bytes)),
                    retry_after_header(resp.headers()),
                ));
            }
            let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
                ProviderError::Malformed(format!("GET {url}: response is not JSON: {e}"))
            })?;
            let arr=v.get("data").or_else(||v.get("models")).and_then(|d|d.as_array()).or_else(||v.as_array())
                .ok_or_else(||ProviderError::Malformed(format!("GET {url}: no data/models array; check the API base URL (usually ends in /v1)")))?;
            for m in arr {
                if let Some(id) = m.as_str().or_else(|| {
                    m.get("id")
                        .or_else(|| m.get("name"))
                        .and_then(|v| v.as_str())
                }) {
                    if !id.trim().is_empty() {
                        ids.push(id.into());
                    }
                }
            }
            if v.get("has_more").and_then(|v| v.as_bool()) != Some(true) {
                ids.sort();
                ids.dedup();
                return Ok(ids);
            }
            let next = v
                .get("last_id")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    arr.last()
                        .and_then(|m| m.get("id"))
                        .and_then(|v| v.as_str())
                })
                .ok_or_else(|| {
                    ProviderError::Malformed("paginated model list has no cursor".into())
                })?;
            if !cursors.insert(next.to_string()) {
                return Err(ProviderError::Malformed(
                    "model pagination cursor repeated".into(),
                ));
            }
            after = Some(next.into());
        }
        Err(ProviderError::Malformed(
            "model discovery exceeded 100 pages".into(),
        ))
    }
}

pub fn shared_transport() -> Arc<HttpTransport> {
    Arc::new(HttpTransport::default())
}

/// Accept either an API base or a pasted request/discovery endpoint.
pub fn normalize_base_url(url: &str) -> String {
    let mut base = url.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/responses", "/models"] {
        if base.ends_with(suffix) {
            base.truncate(base.len() - suffix.len());
            break;
        }
    }
    base
}
