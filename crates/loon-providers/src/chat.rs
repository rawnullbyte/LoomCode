//! Chat Completions adapter (`POST {base}/chat/completions`, `stream: true`).
//!
//! Tool calls stream as `delta.tool_calls[{index, id, function{name, arguments}}]`
//! fragments. Fragments are accumulated per index and only surfaced as
//! [`ProviderEvent::ToolCallComplete`] once the protocol ends the stream with
//! `finish_reason: "tool_calls"` (or the call is otherwise finalized) *and* the arguments
//! parse as a JSON object. Usage arrives in the final chunk via
//! `stream_options: {"include_usage": true}`; endpoints that omit it report usage as
//! absent — Loon never fabricates it. Endpoints that ignore `stream: true` and answer
//! with a JSON body are handled through the non-streaming shape.

use crate::http::{Endpoint, HttpTransport};
use crate::sse::SseFrame;
use crate::{ChatMessage, ChatRequest, FinishReason, ProviderError, ProviderEvent, Role};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct ChatCompletionsAdapter {
    http: Arc<HttpTransport>,
    /// Max idle time between stream chunks.
    pub idle_timeout: Duration,
}

impl ChatCompletionsAdapter {
    pub fn new(http: Arc<HttpTransport>) -> ChatCompletionsAdapter {
        ChatCompletionsAdapter {
            http,
            idle_timeout: Duration::from_secs(120),
        }
    }

    pub fn path() -> &'static str {
        "/chat/completions"
    }

    /// Serialize the internal message model to the Chat Completions wire format.
    pub fn build_body(req: &ChatRequest) -> serde_json::Value {
        let mut messages = Vec::with_capacity(req.messages.len() + 2);
        for m in &req.messages {
            messages.push(message_to_wire(m));
        }
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !req.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(req.tools.clone());
            body["tool_choice"] = serde_json::json!("auto");
        }
        if let Some(t) = req.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(m) = req.max_output_tokens {
            body["max_tokens"] = serde_json::json!(m);
        }
        body
    }

    /// Stream a request, forwarding provider events on `tx`. Returns the terminal event
    /// classification for the caller (combo runner).
    pub async fn stream(
        &self,
        endpoint: &Endpoint,
        req: &ChatRequest,
        tx: mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        let body = Self::build_body(req);
        let mut stream = self.http.post_sse(endpoint, Self::path(), &body).await?;

        if stream.is_json_not_sse() {
            let v = stream.body_json().await?;
            return emit_non_streamed(&v, tx).await;
        }

        // Accumulated tool calls by index (protocol reconstructs calls across chunks).
        let mut calls: BTreeMap<i64, PartialToolCall> = BTreeMap::new();
        let mut finish: Option<FinishReason> = None;
        let mut usage = loon_events::Usage::default();
        let mut saw_done = false;

        loop {
            let Some(frame) = stream.next_frame(self.idle_timeout).await? else {
                break;
            };
            if frame.data.trim() == "[DONE]" {
                saw_done = true;
                break;
            }
            let v: serde_json::Value = match serde_json::from_str(&frame.data) {
                Ok(v) => v,
                Err(e) => return Err(ProviderError::Malformed(format!("chunk is not JSON: {e}"))),
            };
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("provider error");
                let code = err.get("code").and_then(|c| c.as_i64());
                let pe = match code {
                    Some(429) => ProviderError::RateLimited(msg.to_string()),
                    _ => ProviderError::Rejected(msg.to_string()),
                };
                let _ = tx.send(ProviderEvent::Failure { error: pe }).await;
                return Ok(());
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                usage = parse_usage(u);
            }
            let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
                continue; // keepalive/comment frames, usage-only frames
            };
            for choice in choices {
                let idx = choice.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                        if !text.is_empty() {
                            let _ = tx
                                .send(ProviderEvent::TextDelta {
                                    text: text.to_string(),
                                })
                                .await;
                        }
                    }
                    if let Some(tc) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for frag in tc {
                            let i = frag.get("index").and_then(|x| x.as_i64()).unwrap_or(idx);
                            let entry = calls.entry(i).or_default();
                            if let Some(id) = frag.get("id").and_then(|x| x.as_str()) {
                                if !id.is_empty() {
                                    entry.id = id.to_string();
                                }
                            }
                            if let Some(f) = frag.get("function") {
                                if let Some(n) = f.get("name").and_then(|x| x.as_str()) {
                                    if !n.is_empty() {
                                        entry.name.push_str(n);
                                    }
                                }
                                if let Some(a) = f.get("arguments").and_then(|x| x.as_str()) {
                                    entry.args.push_str(a);
                                }
                            }
                            let _ = tx
                                .send(ProviderEvent::ToolCallDelta {
                                    index: i.max(0) as usize,
                                    name: if entry.name.is_empty() {
                                        None
                                    } else {
                                        Some(entry.name.clone())
                                    },
                                    args_delta: String::new(),
                                })
                                .await;
                        }
                    }
                }
                if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    finish = Some(match fr {
                        "stop" => FinishReason::Stop,
                        "tool_calls" | "function_call" => FinishReason::ToolCalls,
                        "length" => FinishReason::Length,
                        "content_filter" => FinishReason::ContentFilter,
                        _ => FinishReason::Other,
                    });
                }
            }
        }

        if !saw_done && finish.is_none() {
            return Err(ProviderError::Dropped(
                "stream ended without [DONE] or finish_reason".into(),
            ));
        }

        if !usage.is_empty() {
            let _ = tx.send(ProviderEvent::Usage(usage)).await;
        }

        // Finalize tool calls only when the protocol says the response ends in tool calls.
        if finish == Some(FinishReason::ToolCalls) {
            for (_, c) in calls {
                if c.id.is_empty() || c.name.is_empty() {
                    let _ = tx
                        .send(ProviderEvent::Failure {
                            error: ProviderError::Malformed(format!(
                                "incomplete tool call (id={:?}, name={:?})",
                                c.id, c.name
                            )),
                        })
                        .await;
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(&c.args) {
                    Ok(v) if v.is_object() => {
                        let _ = tx
                            .send(ProviderEvent::ToolCallComplete {
                                call: crate::ToolCallReq {
                                    id: c.id,
                                    name: c.name,
                                    arguments_json: c.args,
                                },
                            })
                            .await;
                    }
                    Ok(_) => {
                        let _ = tx
                            .send(ProviderEvent::Failure {
                                error: ProviderError::Malformed(format!(
                                    "tool call `{}` arguments are not a JSON object",
                                    c.name
                                )),
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(ProviderEvent::Failure {
                                error: ProviderError::Malformed(format!(
                                    "tool call `{}` arguments are not valid JSON: {e}",
                                    c.name
                                )),
                            })
                            .await;
                    }
                }
            }
        }

        let _ = tx
            .send(ProviderEvent::Finished {
                finish: finish.unwrap_or(FinishReason::Other),
            })
            .await;
        Ok(())
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    args: String,
}

fn parse_usage(u: &serde_json::Value) -> loon_events::Usage {
    loon_events::Usage {
        input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()),
        output_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()),
        cached_input_tokens: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
    }
}

fn message_to_wire(m: &ChatMessage) -> serde_json::Value {
    match m.role {
        Role::Tool => serde_json::json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.text,
        }),
        Role::Assistant if !m.tool_calls.is_empty() => {
            let mut v = serde_json::json!({
                "role": "assistant",
                "content": if m.text.is_empty() { serde_json::Value::Null } else { serde_json::json!(m.text) },
                "tool_calls": m.tool_calls.iter().map(|c| serde_json::json!({
                    "id": c.id,
                    "type": "function",
                    "function": {"name": c.name, "arguments": c.arguments_json},
                })).collect::<Vec<_>>(),
            });
            if m.text.is_empty() {
                v["content"] = serde_json::Value::Null;
            }
            v
        }
        Role::Assistant => serde_json::json!({"role": "assistant", "content": m.text}),
        Role::User => serde_json::json!({"role": "user", "content": m.text}),
        Role::System => serde_json::json!({"role": "system", "content": m.text}),
    }
}

/// Handle endpoints that answered a stream request with one JSON document.
async fn emit_non_streamed(
    v: &serde_json::Value,
    tx: mpsc::Sender<ProviderEvent>,
) -> Result<(), ProviderError> {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("provider error");
        let _ = tx
            .send(ProviderEvent::Failure {
                error: ProviderError::Rejected(msg.to_string()),
            })
            .await;
        return Ok(());
    }
    if let Some(u) = v.get("usage") {
        let _ = tx.send(ProviderEvent::Usage(parse_usage(u))).await;
    }
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned();
    let Some(choice) = choice else {
        return Err(ProviderError::Malformed(
            "non-streamed response has no choices".into(),
        ));
    };
    if let Some(text) = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        if !text.is_empty() {
            let _ = tx
                .send(ProviderEvent::TextDelta {
                    text: text.to_string(),
                })
                .await;
        }
    }
    if let Some(tcs) = choice
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for tc in tcs {
            let id = tc
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|x| x.as_str())
                .unwrap_or("{}")
                .to_string();
            match serde_json::from_str::<serde_json::Value>(&args) {
                Ok(o) if o.is_object() => {
                    let _ = tx
                        .send(ProviderEvent::ToolCallComplete {
                            call: crate::ToolCallReq {
                                id,
                                name,
                                arguments_json: args,
                            },
                        })
                        .await;
                }
                _ => {
                    let _ = tx
                        .send(ProviderEvent::Failure {
                            error: ProviderError::Malformed(format!(
                                "tool call `{name}` has invalid arguments"
                            )),
                        })
                        .await;
                }
            }
        }
    }
    let finish = match choice.get("finish_reason").and_then(|f| f.as_str()) {
        Some("stop") => FinishReason::Stop,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    };
    let _ = tx.send(ProviderEvent::Finished { finish }).await;
    Ok(())
}

/// Helper used by tests: run a complete frame sequence through the same event machinery.
#[doc(hidden)]
pub fn finish_reason_from_str(s: &str) -> FinishReason {
    match s {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

pub type Frame = SseFrame;
