//! Event kinds. Versioned via `schema_version` on the session; additive changes bump it.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Time-ordered stable id used for sessions, turns, requests, tool calls, attempts.
pub type EvId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Token/cost accounting as reported by the provider. `None` means the provider did not
/// report it; Loon never invents numbers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.reasoning_tokens.is_none()
    }
}

/// How a provider request ended. `Unknown` is a real outcome (e.g. dropped connection after
/// the request may have reached the server) and is never reported as a success.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum RequestOutcome {
    Completed {
        finish: String,
    },
    Cancelled,
    Errored {
        error: String,
        retriable: bool,
    },
    /// Sent (or possibly sent) but the final state could not be determined.
    Unknown {
        reason: String,
    },
}

/// Which combo member actually served a request, and why earlier members did not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemberAttempt {
    pub provider_id: String,
    pub model_id: String,
    pub combo: Option<String>,
    pub combo_mode: Option<String>,
    pub rotation_index: Option<usize>,
    pub endpoint_origin: String,
    pub api_family: String,
    /// Why this member was picked / skipped / failed. Human-readable by design.
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EventKind {
    SessionStarted {
        cwd: String,
        model: String,
        headless: bool,
    },
    UserMessage {
        text: String,
    },
    TurnStarted {
        turn: EvId,
    },
    TurnFinished {
        turn: EvId,
        reason: String,
    },
    /// One provider request attempt (combo members each get their own).
    RequestStarted {
        request: EvId,
        turn: EvId,
        member: MemberAttempt,
        /// Rough estimate; provider-reported usage replaces it in RequestFinished.
        est_context_tokens: Option<u64>,
    },
    /// Streaming text delta from the provider.
    TextDelta {
        request: EvId,
        text: String,
    },
    /// Reasoning summary surfaced by the API (never synthesized by Loon).
    ReasoningDelta {
        request: EvId,
        text: String,
    },
    /// A complete, schema-valid tool call observed from the provider.
    ToolCallObserved {
        request: EvId,
        call: EvId,
        /// The protocol-level call id (needed to rebuild valid wire conversations).
        #[serde(default)]
        wire_id: String,
        name: String,
        arguments_json: String,
    },
    ToolExecutionStarted {
        call: EvId,
        name: String,
    },
    ToolExecutionFinished {
        call: EvId,
        name: String,
        ok: bool,
        /// Short excerpt for the transcript; full output lives in a blob.
        excerpt: String,
        blob: Option<BlobRef>,
        duration_ms: u64,
    },
    /// Tool arguments were rejected before execution (invalid JSON / schema).
    ToolCallRejected {
        request: EvId,
        name: String,
        reason: String,
    },
    RequestFinished {
        request: EvId,
        outcome: RequestOutcome,
        usage: Usage,
        /// True when the provider reported the usage, false when absent.
        usage_reported: bool,
    },
    /// A combo advanced from one member to another mid-request (pre-stream only).
    MemberSwitched {
        request: EvId,
        from: MemberAttempt,
        to: MemberAttempt,
        reason: String,
    },
    /// One workspace file changed by a tool transaction, with content-addressed
    /// before/after snapshots — the substrate for undo/redo/rewind.
    FileMutated {
        tx: EvId,
        call: EvId,
        path: String,
        tool: String,
        before_sha256: String,
        after_sha256: String,
        before_blob: Option<BlobRef>,
        after_blob: Option<BlobRef>,
    },
    MutationUndone {
        tx: EvId,
        files: usize,
        /// Files left untouched because the current content did not match the
        /// expected hash — user work is never silently overwritten.
        conflicts: Vec<String>,
    },
    MutationRedone {
        tx: EvId,
        files: usize,
        conflicts: Vec<String>,
    },
    /// Conversation/workspace rewound to before `target_request`. `truncate_to_seq`
    /// lets a resume replay drop everything after the mark deterministically.
    Rewound {
        target_request: EvId,
        mode: String,
        truncate_to_seq: u64,
        files_restored: usize,
        files_conflicted: usize,
    },
    ForkCreated {
        name: String,
        at_seq: u64,
    },
    CheckpointCreated {
        name: String,
        at_seq: u64,
    },
    /// Context compaction: everything the model saw before this point is replaced by
    /// `summary` plus the verbatim `kept` tail. The event fully describes the
    /// post-compaction context, so a resume replays byte-identical history with no
    /// duplicated user/tool events in the journal.
    ContextCompacted {
        turn: EvId,
        before_tokens: u64,
        after_tokens: u64,
        summary: String,
        kept: Vec<JournalMessage>,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
}

/// A content-addressed reference to large output stored beside the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub sha256: String,
    pub bytes: u64,
}

/// Wire-format conversation message as journaled inside [`EventKind::ContextCompacted`].
/// Field-compatible with `loon_providers::ChatMessage` (which depends on this crate, so
/// the neutral shape lives here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalMessage {
    /// "user" | "assistant" | "tool"
    pub role: String,
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<JournalToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EvId,
    pub parent: Option<EvId>,
    pub seq: u64,
    pub ts_ms: i64,
    #[serde(rename = "kind", flatten)]
    pub kind: EventKind,
}

impl Event {
    /// One-line summary used by `sessions list` and timeline pickers.
    pub fn summary(&self) -> String {
        use EventKind::*;
        match &self.kind {
            SessionStarted { cwd, model, .. } => format!("session started in {cwd} on {model}"),
            UserMessage { text } => {
                let t = truncate_line(text, 60);
                format!("user: {t}")
            }
            TurnStarted { .. } => "turn started".into(),
            TurnFinished { reason, .. } => format!("turn finished ({reason})"),
            RequestStarted { member, .. } => format!(
                "request → {}/{}{}",
                member.provider_id,
                member.model_id,
                member
                    .combo
                    .as_ref()
                    .map(|c| format!(" (combo {c})"))
                    .unwrap_or_default()
            ),
            TextDelta { .. } => "text delta".into(),
            ReasoningDelta { .. } => "reasoning delta".into(),
            ToolCallObserved { name, .. } => format!("tool call: {name}"),
            ToolExecutionStarted { name, .. } => format!("tool exec: {name}"),
            ToolExecutionFinished {
                name,
                ok,
                duration_ms,
                ..
            } => format!("tool done: {name} ok={ok} in {duration_ms}ms"),
            ToolCallRejected {
                request: _,
                name,
                reason,
            } => format!("tool rejected: {name}: {reason}"),
            RequestFinished { outcome, usage, .. } => {
                format!("request finished: {outcome:?} usage={usage:?}")
            }
            MemberSwitched {
                request: _,
                from,
                to,
                reason,
            } => {
                format!(
                    "combo switch {}→{} ({})",
                    from.model_id, to.model_id, reason
                )
            }
            FileMutated { path, tool, .. } => format!("file mutated by {tool}: {path}"),
            MutationUndone {
                tx,
                files,
                conflicts,
            } => {
                format!(
                    "undid tx {} ({files} files, {} conflicts)",
                    tx.as_simple(),
                    conflicts.len()
                )
            }
            MutationRedone { tx, files, .. } => {
                format!("redone tx {} ({files} files)", tx.as_simple())
            }
            Rewound {
                target_request,
                mode,
                ..
            } => format!("rewound to before request {target_request} ({mode})"),
            ForkCreated { name, .. } => format!("fork created: {name}"),
            CheckpointCreated { name, .. } => format!("checkpoint: {name}"),
            ContextCompacted {
                before_tokens,
                after_tokens,
                ..
            } => format!("context compacted ({before_tokens} → {after_tokens} est. tokens)"),
            Warning { message } => format!("warning: {message}"),
            Error { message } => format!("error: {message}"),
        }
    }
}

fn truncate_line(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}
