//! MCP Streamable HTTP: JSON or SSE responses, session and negotiated-version headers.
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::Mutex;
pub(crate) struct HttpMcp {
    url: String,
    client: reqwest::Client,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
}
impl HttpMcp {
    pub(crate) fn new(url: String, timeout: Duration) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            url,
            client,
            session: Mutex::new(None),
            version: Mutex::new(None),
        })
    }
    pub(crate) async fn set_version(&self, version: &str) {
        *self.version.lock().await = Some(version.into());
    }
    pub(crate) async fn send(&self, message: Value) -> Result<Value, String> {
        let request_id = message.get("id").cloned();
        let mut request = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .json(&message);
        if let Some(s) = self.session.lock().await.as_ref() {
            request = request.header("Mcp-Session-Id", s);
        }
        if let Some(v) = self.version.lock().await.as_ref() {
            request = request.header("MCP-Protocol-Version", v);
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| format!("MCP HTTP {}: {e}", self.url))?;
        if !response.status().is_success() {
            return Err(format!(
                "MCP endpoint {} returned HTTP {}{}",
                self.url,
                response.status(),
                if response.status() == 404 {
                    "; reconnect the server (check its endpoint path/session)"
                } else {
                    ""
                }
            ));
        }
        if let Some(s) = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|s| s.to_str().ok())
        {
            *self.session.lock().await = Some(s.into());
        }
        if request_id.is_none() {
            return Ok(Value::Null);
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|s| s.to_str().ok())
            .unwrap_or("")
            .contains("text/event-stream");
        let mut parser = loon_providers::sse::SseParser::new();
        let mut bytes = Vec::new();
        let mut total = 0usize;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("MCP HTTP body: {e}"))?
        {
            total += chunk.len();
            if total > 8 * 1024 * 1024 {
                return Err("MCP response exceeded 8 MiB".into());
            }
            if is_sse {
                for frame in parser.feed(&chunk) {
                    let frame = frame.map_err(|_| "MCP SSE contained invalid UTF-8")?;
                    if frame.data.is_empty() {
                        continue;
                    }
                    let data: Value = serde_json::from_str(&frame.data)
                        .map_err(|e| format!("MCP SSE JSON: {e}"))?;
                    if data.get("id") == request_id.as_ref() && data.get("method").is_none() {
                        return rpc_result(data);
                    }
                    if data.get("id").is_some() && data.get("method").is_some() {
                        return Err("MCP server requested a client capability that this client has not advertised".into());
                    }
                }
            } else {
                bytes.extend_from_slice(&chunk);
            }
        }
        if is_sse {
            for frame in parser.finish().into_iter().flatten() {
                if let Ok(data) = serde_json::from_str::<Value>(&frame.data) {
                    if data.get("id") == request_id.as_ref() {
                        return rpc_result(data);
                    }
                }
            }
            Err("MCP stream ended before its matching response".into())
        } else {
            let data: Value =
                serde_json::from_slice(&bytes).map_err(|e| format!("MCP JSON response: {e}"))?;
            if data.get("id") != request_id.as_ref() {
                return Err("MCP response id does not match the request".into());
            }
            rpc_result(data)
        }
    }
    pub(crate) async fn shutdown(&self) {
        if let Some(session) = self.session.lock().await.take() {
            let mut r = self
                .client
                .delete(&self.url)
                .header("Mcp-Session-Id", session);
            if let Some(v) = self.version.lock().await.as_ref() {
                r = r.header("MCP-Protocol-Version", v);
            }
            let _ = r.send().await;
        }
    }
}
fn rpc_result(data: Value) -> Result<Value, String> {
    if let Some(error) = data.get("error") {
        return Err(format!("MCP JSON-RPC error: {error}"));
    }
    data.get("result")
        .cloned()
        .ok_or_else(|| "MCP response omitted result".into())
}
// Kept explicit so callers never auto-retry a possibly mutating tools/call.
pub(crate) fn initialize() -> Value {
    json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"loon","version":env!("CARGO_PKG_VERSION")}})
}
