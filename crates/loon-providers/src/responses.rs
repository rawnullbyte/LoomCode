//! Responses API adapter (`POST {base}/responses`, `stream: true`).
//!
//! The Responses wire protocol is materially different from Chat Completions: typed
//! stream events (`response.output_text.delta`, `response.function_call_arguments.done`,
//! `response.completed`, …), item-based tool calls with `call_id`, usage inside the
//! final `response.completed` payload, and a response id that is opaque continuation
//! state (member-local — never replayed to another provider). Events are mapped onto the
//! shared [`ProviderEvent`] model without flattening those distinctions.

use crate::http::{Endpoint, HttpTransport};
use crate::{
    ChatMessage, ChatRequest, FinishReason, ProviderError, ProviderEvent, Role, ToolCallReq,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct ResponsesAdapter {
    http: Arc<HttpTransport>,
    pub idle_timeout: Duration,
}

impl ResponsesAdapter {
    pub fn new(http: Arc<HttpTransport>) -> ResponsesAdapter {
        ResponsesAdapter {
            http,
            idle_timeout: Duration::from_secs(120),
        }
    }

    pub fn path() -> &'static str {
        "/responses"
    }

    /// Serialize the internal message model to Responses `input` items.
    pub fn build_body(req: &ChatRequest) -> serde_json::Value {
        let mut input = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            match message_to_item(m) {
                serde_json::Value::Array(items) => input.extend(items),
                item => input.push(item),
            }
        }
        let mut body = serde_json::json!({
            "model": req.model,
            "input": input,
            "stream": true,
            "store": false,
        });
        if !req.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                req.tools
                    .iter()
                    .filter_map(|t| {
                        // Chat-format function tools are converted to Responses flat form.
                        let f = t.get("function")?;
                        Some(serde_json::json!({
                            "type": "function",
                            "name": f.get("name")?,
                            "description": f.get("description").cloned().unwrap_or(serde_json::Value::Null),
                            "parameters": f.get("parameters").cloned().unwrap_or(serde_json::json!({})),
                            "strict": false,
                        }))
                    })
                    .collect(),
            );
            body["tool_choice"] = serde_json::json!("auto");
        }
        if let Some(t) = req.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(effort) = &req.reasoning_effort {
            body["reasoning"] = serde_json::json!({"effort": effort});
        }
        if let Some(m) = req.max_output_tokens {
            body["max_output_tokens"] = serde_json::json!(m);
        }
        body
    }

    pub async fn stream(
        &self,
        endpoint: &Endpoint,
        req: &ChatRequest,
        tx: mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        let body = Self::build_body(req);
        let mut stream = self.http.post_sse(endpoint, Self::path(), &body).await?;

        // function_call items by item_id, completed when arguments are done.
        let mut calls: BTreeMap<String, PartialCall> = BTreeMap::new();
        let mut completed_status: Option<String> = None;
        let mut usage = loon_events::Usage::default();
        let mut response_id: Option<String> = None;
        let mut saw_terminal = false;

        loop {
            let Some(frame) = stream.next_frame(self.idle_timeout).await? else {
                break;
            };
            // Empty SSE frames are keepalives (some gateways send `data:` with no payload
            // or bare blank lines); skipping beats killing the turn over heartbeat noise.
            if frame.data.trim().is_empty() {
                continue;
            }
            // Some OpenAI-compatible proxies append a chat-style [DONE] sentinel to
            // Responses streams; treat it as stream end, not protocol data.
            if frame.data.trim() == "[DONE]" {
                break;
            }
            let v: serde_json::Value = match serde_json::from_str(&frame.data) {
                Ok(v) => v,
                Err(e) => return Err(ProviderError::Malformed(format!("event is not JSON: {e}"))),
            };
            let etype = v
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            match etype.as_str() {
                "response.output_text.delta" => {
                    if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                        let _ = tx
                            .send(ProviderEvent::TextDelta {
                                text: d.to_string(),
                            })
                            .await;
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                        let _ = tx
                            .send(ProviderEvent::ReasoningDelta {
                                text: d.to_string(),
                            })
                            .await;
                    }
                }
                "response.output_item.added" => {
                    if let Some(item) = v.get("item") {
                        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                            let id = item
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let entry = calls.entry(id).or_default();
                            entry.call_id = item
                                .get("call_id")
                                .and_then(|c| c.as_str())
                                .unwrap_or_default()
                                .to_string();
                            entry.name = item
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or_default()
                                .to_string();
                            entry.args = item
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or_default()
                                .to_string();
                        }
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let (Some(item_id), Some(d)) = (
                        v.get("item_id").and_then(|i| i.as_str()),
                        v.get("delta").and_then(|d| d.as_str()),
                    ) {
                        if let Some(entry) = calls.get_mut(item_id) {
                            entry.args.push_str(d);
                        }
                        let _ = tx
                            .send(ProviderEvent::ToolCallDelta {
                                index: 0,
                                name: None,
                                args_delta: d.to_string(),
                            })
                            .await;
                    }
                }
                "response.function_call_arguments.done" => {
                    if let Some(item_id) = v.get("item_id").and_then(|i| i.as_str()) {
                        if let Some(entry) = calls.get_mut(item_id) {
                            if let Some(a) = v.get("arguments").and_then(|a| a.as_str()) {
                                entry.args = a.to_string();
                            }
                            entry.done = true;
                        }
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = v.get("item") {
                        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                            let id = item
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let entry = calls.entry(id).or_default();
                            entry.call_id = item
                                .get("call_id")
                                .and_then(|c| c.as_str())
                                .unwrap_or_default()
                                .to_string();
                            entry.name = item
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or_default()
                                .to_string();
                            if let Some(a) = item.get("arguments").and_then(|a| a.as_str()) {
                                entry.args = a.to_string();
                            }
                            entry.done = true;
                        }
                    }
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    saw_terminal = true;
                    if let Some(resp) = v.get("response") {
                        response_id = resp.get("id").and_then(|i| i.as_str()).map(str::to_string);
                        completed_status = resp
                            .get("status")
                            .and_then(|s| s.as_str())
                            .map(str::to_string);
                        if let Some(u) = resp.get("usage") {
                            usage = parse_usage(u);
                        }
                    }
                    if etype == "response.failed" {
                        let msg = v
                            .get("response")
                            .and_then(|r| r.get("error"))
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("response failed");
                        let _ = tx
                            .send(ProviderEvent::Failure {
                                error: ProviderError::Rejected(msg.to_string()),
                            })
                            .await;
                        return Ok(());
                    }
                }
                "error" => {
                    // Classify like the Chat adapter: a flat `{"type":"error"}` event can
                    // carry a 429 or 5xx, and mapping everything to `Rejected` (neither
                    // switchable nor retriable) made a combo neither back off nor advance
                    // to the next member — a rate limit killed the turn.
                    let error = ProviderError::from_error_value(&v);
                    let _ = tx.send(ProviderEvent::Failure { error }).await;
                    return Ok(());
                }
                _ => {} // response.created, heartbeats, other typed events: ignored safely
            }
        }

        if !saw_terminal {
            return Err(ProviderError::Dropped(
                "stream ended without a terminal response event".into(),
            ));
        }

        if !usage.is_empty() {
            let _ = tx.send(ProviderEvent::Usage(usage)).await;
        }

        for (_, entry) in calls {
            if entry.call_id.is_empty() || entry.name.is_empty() {
                let _ = tx
                    .send(ProviderEvent::Failure {
                        error: ProviderError::Malformed(format!(
                            "function_call item missing call_id/name (name={:?})",
                            entry.name
                        )),
                    })
                    .await;
                continue;
            }
            // Match Chat Completions recovery: malformed arguments become a tool
            // validation result the model can correct, rather than killing the turn.
            let arguments_json =
                crate::args::normalize_tool_arguments(&entry.args).unwrap_or_else(|| "{}".into());
            let _ = tx
                .send(ProviderEvent::ToolCallComplete {
                    call: ToolCallReq {
                        id: entry.call_id,
                        name: entry.name,
                        arguments_json,
                    },
                })
                .await;
        }

        let finish = match completed_status.as_deref() {
            Some("completed") => FinishReason::Stop,
            Some("incomplete") => FinishReason::Length,
            _ => FinishReason::Other,
        };
        let _ = tx.send(ProviderEvent::Finished { finish }).await;
        let _ = response_id; // opaque continuation state; surfaced on the request record by the caller layer
        Ok(())
    }
}

#[derive(Default)]
struct PartialCall {
    call_id: String,
    name: String,
    args: String,
    done: bool,
}

fn parse_usage(u: &serde_json::Value) -> loon_events::Usage {
    let details = u.get("output_tokens_details");
    loon_events::Usage {
        input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()),
        cached_input_tokens: u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        reasoning_tokens: details
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
    }
}

fn message_to_item(m: &ChatMessage) -> serde_json::Value {
    match m.role {
        Role::System => serde_json::json!({
            "role": "system",
            "content": [{"type": "input_text", "text": m.text}],
        }),
        Role::User => serde_json::json!({
            "role": "user",
            "content": [{"type": "input_text", "text": m.text}],
        }),
        Role::Assistant => {
            let mut items = Vec::new();
            if !m.text.is_empty() {
                items.push(serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": m.text}],
                }));
            }
            for c in &m.tool_calls {
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": c.id,
                    "name": c.name,
                    "arguments": c.arguments_json,
                }));
            }
            if items.is_empty() {
                items.push(serde_json::json!({"role": "assistant", "content": []}));
            }
            serde_json::Value::Array(items)
        }
        Role::Tool => serde_json::json!({
            "type": "function_call_output",
            "call_id": m.tool_call_id.clone().unwrap_or_default(),
            "output": m.text,
        }),
    }
}
