//! Provider layer: OpenAI-compatible transport, one internal event model, combo
//! resolution.
//!
//! Two wire adapters — Chat Completions and Responses — are distinct because the
//! protocols are distinct (finish semantics, tool-call reconstruction, usage placement,
//! continuation state). Both share [`ProviderEvent`]. Arbitrary base URLs are
//! first-class: a provider is whatever endpoint the user configured, not a vendor list.

pub mod chat;
pub mod combo;
pub mod http;
pub mod mock;
pub mod registry;
pub mod responses;
pub mod sse;

pub use combo::{trail_json, AttemptEvent, ComboOutcome, ComboRunner, RotationState};
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
                | ProviderError::Server { .. }
        )
    }

    pub fn from_status(status: u16, body: &str) -> ProviderError {
        match status {
            401 | 403 => ProviderError::Auth(format!("HTTP {status}: {}", truncate_body(body))),
            429 => ProviderError::RateLimited(format!("HTTP 429: {}", truncate_body(body))),
            s if (500..600).contains(&s) => ProviderError::Server {
                status: s,
                message: truncate_body(body),
            },
            _ => ProviderError::Rejected(format!("HTTP {status}: {}", truncate_body(body))),
        }
    }
}

fn truncate_body(s: &str) -> String {
    let t: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        format!("{t}…")
    } else {
        t
    }
}
