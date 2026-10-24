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
        if let Some(effort) = &req.reasoning_effort {
            body["reasoning_effort"] = serde_json::json!(effort);
            if let Some(m) = req.max_output_tokens {
                body.as_object_mut().unwrap().remove("max_tokens");
                body["max_completion_tokens"] = serde_json::json!(m);
            }
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

        // Accumulated tool calls in first-appearance order. `by_index` maps the wire
        // index to a position; a fragment that starts a *new* call (fresh id, or a
        // different name once arguments have begun) takes a new position and repoints
        // the index, because several compatible gateways reuse `index: 0` for parallel
        // calls. Concatenating those would produce `{...}{...}` — invalid JSON.
        let mut calls: Vec<PartialToolCall> = Vec::new();
        let mut by_index: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        let mut finish: Option<FinishReason> = None;
        let mut usage = loon_events::Usage::default();
        let mut saw_done = false;

        loop {
            let Some(frame) = stream.next_frame(self.idle_timeout).await? else {
                break;
            };
            // Empty SSE frames are keepalives (some gateways send `data:` with no payload
            // or bare blank lines); skipping beats killing the turn over heartbeat noise.
            if frame.data.trim().is_empty() {
                continue;
            }
            if frame.data.trim() == "[DONE]" {
                saw_done = true;
                break;
            }
            let v: serde_json::Value = match serde_json::from_str(&frame.data) {
                Ok(v) => v,
                Err(e) => return Err(ProviderError::Malformed(format!("chunk is not JSON: {e}"))),
            };
            if v.get("error").is_some() {
                let pe = ProviderError::from_error_value(&v);
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
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .and_then(|v| v.as_str())
                    {
                        if !text.is_empty() {
                            let _ = tx
                                .send(ProviderEvent::ReasoningDelta { text: text.into() })
                                .await;
                        }
                    }
                    if let Some(text) = delta.get("content").and_then(content_text) {
                        if !text.is_empty() {
                            let _ = tx.send(ProviderEvent::TextDelta { text }).await;
                        }
                    }
                    if let Some(tc) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for frag in tc {
                            let frag_id = frag
                                .get("id")
                                .and_then(|x| x.as_str())
                                .filter(|s| !s.is_empty());
                            let frag_name = frag
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|x| x.as_str())
                                .filter(|s| !s.is_empty());
                            let frag_args = frag
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(crate::args::arguments_to_string);
                            // Missing `index` continues the most recent call; index 0 is
                            // the common fallback only when nothing has arrived yet.
                            //
                            // A negative index is not valid on the wire, and accepting it
                            // split one logical call in two: `{"index":-5}` created a
                            // key `-5` for the name, then the arguments arrived under
                            // `index: 0` in a *different* entry — surfacing an executable
                            // call with `{}` arguments. Clamp it to the "continue the
                            // current call" case instead of trusting it.
                            let i = frag
                                .get("index")
                                .and_then(|x| x.as_i64())
                                .filter(|i| *i >= 0)
                                .unwrap_or_else(|| calls.len().saturating_sub(1) as i64);
                            let pos = match by_index.get(&i) {
                                Some(&p) if starts_new_call(&calls[p], frag_id, frag_name) => {
                                    calls.push(PartialToolCall::default());
                                    let p = calls.len() - 1;
                                    by_index.insert(i, p);
                                    p
                                }
                                Some(&p) => p,
                                None => {
                                    calls.push(PartialToolCall::default());
                                    let p = calls.len() - 1;
                                    by_index.insert(i, p);
                                    p
                                }
                            };
                            let entry = &mut calls[pos];
                            if let Some(id) = frag_id {
                                if entry.id.is_empty() {
                                    entry.id = id.to_string();
                                }
                            }
                            if let Some(n) = frag_name {
                                // A repeated name is the same call re-announced; a split
                                // name is still streaming (nothing to append to yet).
                                if entry.name.is_empty()
                                    || (entry.args.is_empty() && n != entry.name)
                                {
                                    entry.name.push_str(n);
                                }
                            }
                            let args_label = frag_args.clone().unwrap_or_default();
                            if let Some(a) = frag_args {
                                entry.args.push_str(&a);
                            }
                            let _ = tx
                                .send(ProviderEvent::ToolCallDelta {
                                    index: pos,
                                    name: if entry.name.is_empty() {
                                        None
                                    } else {
                                        Some(entry.name.clone())
                                    },
                                    // Report the fragment just accumulated: the core counts
                                    // these characters for streamed progress, so sending an
                                    // empty delta made every Chat tool call look stalled.
                                    args_delta: args_label,
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

        // Finalize tool calls when the stream ends in calls. `finish_reason: "stop"`
        // alongside calls is common on compatible gateways, so it is accepted too; a
        // truncated answer (`length`) is not, because its arguments are incomplete.
        let truncated = matches!(
            finish,
            Some(FinishReason::Length) | Some(FinishReason::ContentFilter)
        );
        if !calls.is_empty() && !truncated {
            for (n, c) in calls.into_iter().enumerate() {
                // Placeholder fragments (no name, no arguments) and calls the model
                // never named carry nothing executable. Dropping them lets the turn
                // continue; failing here would kill it over protocol noise.
                if c.name.is_empty() {
                    continue;
                }
                // Some gateways omit the id; the transcript still needs one to pair the
                // result with its call, so synthesize a stable per-response id.
                let id = if c.id.is_empty() {
                    format!("call_{n}")
                } else {
                    c.id
                };
                // Unrecoverable arguments become `{}`: the tool then reports its
                // validation error to the model, which can correct and retry. Killing
                // the whole turn over one malformed call is worse.
                let arguments_json =
                    crate::args::normalize_tool_arguments(&c.args).unwrap_or_else(|| "{}".into());
                let _ = tx
                    .send(ProviderEvent::ToolCallComplete {
                        call: crate::ToolCallReq {
                            id,
                            name: c.name,
                            arguments_json,
                        },
                    })
                    .await;
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

/// A fragment reusing an index starts a new call when it carries a different id, or a
/// different name after arguments have already begun (split names stream in before the
/// first argument byte).
fn starts_new_call(cur: &PartialToolCall, id: Option<&str>, name: Option<&str>) -> bool {
    let new_id = id.is_some_and(|id| !cur.id.is_empty() && id != cur.id);
    let new_name =
        name.is_some_and(|n| !cur.name.is_empty() && !cur.args.is_empty() && n != cur.name);
    new_id || new_name
}

/// `content` is a string in the spec, but some compatible servers send an array of
/// parts (`[{"type":"text","text":"..."}]`).
fn content_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                }
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
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
        .and_then(content_text)
    {
        if !text.is_empty() {
            let _ = tx.send(ProviderEvent::TextDelta { text }).await;
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
                .and_then(crate::args::arguments_to_string)
                .unwrap_or_else(|| "{}".into());
            let arguments_json =
                crate::args::normalize_tool_arguments(&args).unwrap_or_else(|| "{}".into());
            let _ = tx
                .send(ProviderEvent::ToolCallComplete {
                    call: crate::ToolCallReq {
                        id,
                        name,
                        arguments_json,
                    },
                })
                .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, args: &str) -> PartialToolCall {
        PartialToolCall {
            id: id.into(),
            name: name.into(),
            args: args.into(),
        }
    }

    #[test]
    fn a_new_id_on_the_same_index_starts_a_new_call() {
        // Gateways that reuse index 0 for parallel calls must not merge them.
        let first = call("call_a", "read", r#"{"path":"a"}"#);
        assert!(starts_new_call(&first, Some("call_b"), None));
        assert!(!starts_new_call(&first, Some("call_a"), None));
        assert!(!starts_new_call(&first, None, None));
    }

    #[test]
    fn a_different_name_after_arguments_starts_a_new_call() {
        let first = call("call_a", "read", r#"{"path":"a"}"#);
        assert!(starts_new_call(&first, None, Some("bash")));
        // While the name is still streaming in there is nothing to split yet.
        let partial = call("", "re", "");
        assert!(!starts_new_call(&partial, None, Some("read")));
    }

    #[test]
    fn content_arrays_from_compatible_servers_are_flattened() {
        let v = serde_json::json!([{"type":"text","text":"hello "},{"type":"text","text":"world"}]);
        assert_eq!(content_text(&v).as_deref(), Some("hello world"));
        assert_eq!(
            content_text(&serde_json::json!("plain")).as_deref(),
            Some("plain")
        );
        assert_eq!(content_text(&serde_json::Value::Null), None);
    }
}
