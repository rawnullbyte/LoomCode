//! Loon's authoritative event model and append-only journal.
//!
//! Every state change the agent makes is recorded here as an [`Event`] with a stable
//! UUIDv7 id, causal parent, sequence number, timestamp and schema version. The journal
//! (JSONL) plus the content-addressed blob store are the source of truth; transcripts and
//! UI state are rebuildable projections.

pub mod journal;
pub mod model;

pub use journal::{list_sessions, Journal, SessionMeta, SCHEMA_VERSION};
pub use model::*;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EventsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("journal schema {found} cannot be read by this build (supports <= {supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("malformed journal line {line}: {reason}")]
    MalformedLine { line: usize, reason: String },
}

pub type Result<T> = std::result::Result<T, EventsError>;
