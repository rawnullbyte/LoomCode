//! Provider layer: OpenAI-compatible transport, one internal event model, combo
//! resolution.
//!
//! Two wire adapters — Chat Completions and Responses — are distinct because the
//! protocols are distinct (finish semantics, tool-call reconstruction, usage placement,
//! continuation state). Both share [`ProviderEvent`]. Arbitrary base URLs are
//! first-class: a provider is whatever endpoint the user configured, not a vendor list.

mod args;
pub mod chat;
pub mod combo;
pub mod http;
pub mod mock;
pub mod registry;
pub mod responses;
pub mod sse;

pub use combo::{trail_json, AttemptEvent, ComboOutcome, ComboRunner, RotationState};
pub use http::{Endpoint, HttpTransport};
pub use mock::{MockProvider, MockScript, MockStep};
pub use registry::{ProviderRegistry, ResolvedModel};

use loon_events::Usage;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One tool invocation requested by the model. `arguments_json` is the complete,
/// protocol-marked-finished argument string (validated before execution downstream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallReq {
    /// Protocol call id (Chat: `call_…`; Responses: `call_id`). Member-local — never
    /// replayed to another provider.
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallReq>,
    /// For `Role::Tool`: which call this result answers.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: Role::System,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn user(text: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn assistant(text: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> ChatMessage {
        ChatMessage {
            role: Role::Tool,
            text: text.into(),
            tool_calls: vec![],
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    /// Provider-format tool definitions (function calling).
    pub tools: Vec<serde_json::Value>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    /// Wire reasoning level, removed by routing unless the provider opted in.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

/// Streaming events out of an adapter. Display-only fragments (deltas) are separate from
/// completed, executable facts ([`ProviderEvent::ToolCallComplete`]).
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Progressive tool-argument fragments, for display only. Never executed.
    ToolCallDelta {
        index: usize,
        name: Option<String>,
        args_delta: String,
    },
    /// The protocol marked this call complete and its arguments parsed.
    ToolCallComplete {
        call: ToolCallReq,
    },
    Usage(Usage),
    Finished {
        finish: FinishReason,
    },
    Failure {
        error: ProviderError,
    },
    /// The member is about to be retried from scratch. Anything streamed for the failed
    /// attempt must be discarded, because the retry produces a fresh answer.
    Retrying {
        reason: String,
    },
}

/// Classified provider failure. The class drives combo fallback: transport/server/rate/
/// auth failures may switch members; model-level refusals and invalid requests must not.
#[derive(Debug, Clone, Error)]
pub enum ProviderError {
    #[error("connection error: {0}")]
    Connect(String),
    #[error("timed out")]
    Timeout,
    #[error("connection dropped mid-stream: {0}")]
    Dropped(String),
    #[error("rate limited{0}")]
    RateLimited(String),
    /// A gateway-level rejection that may clear on its own: 400 from an upstream that
    /// mangles a request, 408, 409, 425. Bounded retries, then surfaced.
    #[error("transient error {status}: {message}")]
    Transient { status: u16, message: String },
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("server error {status}: {message}")]
    Server { status: u16, message: String },
    /// The model/endpoint refused this specific request (bad args, policy, unsupported
    /// parameter). Switching members cannot help — surfaced, not retried elsewhere.
    #[error("request rejected: {0}")]
    Rejected(String),
    /// Response body could not be parsed as the expected protocol.
    #[error("malformed response: {0}")]
    Malformed(String),
}

impl ProviderError {
    /// May a combo advance to the next member because of this error?
    pub fn switchable(&self) -> bool {
        matches!(
            self,
            ProviderError::Connect(_)
                | ProviderError::Timeout
                | ProviderError::Dropped(_)
                | ProviderError::RateLimited(_)
                | ProviderError::Transient { .. }
                | ProviderError::Auth(_)
                | ProviderError::Server { .. }
        )
    }

    /// Is a plain retry (same member) meaningful?
    pub fn retriable(&self) -> bool {
        matches!(
            self,
            ProviderError::Connect(_)
                | ProviderError::Timeout
                | ProviderError::Dropped(_)
                | ProviderError::RateLimited(_)
                | ProviderError::Transient { .. }
                | ProviderError::Server { .. }
        )
    }

    /// How long the provider asked us to wait, when it said so.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            ProviderError::RateLimited(m) => parse_retry_after(m),
            _ => None,
        }
    }

    pub fn from_status(status: u16, body: &str) -> ProviderError {
        let detail = error_detail(body);
        match status {
            401 | 403 => ProviderError::Auth(format!("HTTP {status}: {detail}")),
            429 => ProviderError::RateLimited(format!("HTTP 429: {detail}")),
            // Gateway 400s are frequently upstream glitches ("cannot be converted to
            // another protocol", truncated tool JSON) that succeed on a second attempt.
            // They get bounded retries like any other transient failure.
            400 | 408 | 409 | 425 => ProviderError::Transient {
                status,
                message: detail,
            },
            s if (500..600).contains(&s) => ProviderError::Server {
                status: s,
                message: detail,
            },
            // 404 is a wrong model/base URL, not a transport failure: retrying or
            // switching members cannot fix it.
            _ => ProviderError::Rejected(format!("HTTP {status}: {detail}")),
        }
    }

    /// Classify an in-stream `error` object/event (`{"error":{"message","type","code"}}`
    /// or the flat event form used by the Responses protocol).
    pub fn from_error_value(err: &serde_json::Value) -> ProviderError {
        let nested = err.get("error").filter(|e| e.is_object());
        let message = err
            .get("message")
            .or_else(|| nested.and_then(|e| e.get("message")))
            .and_then(|m| m.as_str())
            .unwrap_or("provider error")
            .to_string();
        let code = err
            .get("code")
            .or_else(|| nested.and_then(|e| e.get("code")));
        let kind = err
            .get("type")
            .or_else(|| nested.and_then(|e| e.get("type")))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let status = err
            .get("status")
            .or(code)
            .and_then(|s| s.as_u64().or_else(|| s.as_str()?.parse().ok()))
            .map(|s| s as u16);
        let code_str = code.and_then(|c| c.as_str()).unwrap_or("");
        let tag = format!("{kind} {code_str}").to_ascii_lowercase();
        if status == Some(401)
            || status == Some(403)
            || tag.contains("auth")
            || tag.contains("api_key")
            || tag.contains("permission")
        {
            ProviderError::Auth(message)
        } else if status == Some(429) || tag.contains("rate") || tag.contains("quota") {
            ProviderError::RateLimited(message)
        } else if matches!(status, Some(400 | 408 | 409 | 425)) {
            ProviderError::Transient {
                status: status.unwrap_or(400),
                message,
            }
        } else if matches!(status, Some(500..=599)) {
            ProviderError::Server {
                status: status.unwrap_or(500),
                message,
            }
        } else {
            ProviderError::Rejected(message)
        }
    }
}

/// `Retry-After` as embedded by the transport: either a `retry_after=Ns` marker or a
/// plain `retry after Ns` phrase in the message.
fn parse_retry_after(message: &str) -> Option<std::time::Duration> {
    let lower = message.to_ascii_lowercase();
    let start = lower
        .find("retry_after=")
        .or_else(|| lower.find("retry after "))?;
    let rest = &lower[start..];
    let digits_start = rest.find(|c: char| c.is_ascii_digit())?;
    let digits: String = rest[digits_start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse::<u64>()
        .ok()
        .map(|secs| std::time::Duration::from_secs(secs.min(300)))
}

/// Extract the human-readable message from an error body, falling back to the raw body.
fn error_detail(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(err) = v.get("error") {
            if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
                return m.to_string();
            }
            if let Some(m) = err.as_str() {
                return m.to_string();
            }
        }
        if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
            return m.to_string();
        }
    }
    truncate_body(trimmed)
}

fn truncate_body(s: &str) -> String {
    let t: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        format!("{t}…")
    } else {
        t
    }
}
