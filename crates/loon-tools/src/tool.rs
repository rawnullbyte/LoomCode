//! Tool contract: specs, effects, bounded outputs, context, registry.

use async_trait::async_trait;
use loon_workspace::Workspace;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effects {
    /// Advertised read-only: must actually be read-only.
    ReadOnly,
    /// May change workspace state; runs serialized by the scheduler.
    Mutating,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema (structural subset) for the arguments object.
    pub parameters: Value,
    pub effects: Effects,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub ok: bool,
    /// Bounded text shown to the model and transcript.
    pub text: String,
    /// Full payload when it exceeded the output limit (stored as a blob by the journal).
    pub full: Option<Vec<u8>>,
    pub truncated: bool,
    pub structured: Value,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> ToolOutput {
        ToolOutput {
            ok: true,
            text: text.into(),
            full: None,
            truncated: false,
            structured: Value::Null,
        }
    }
    pub fn err(text: impl Into<String>) -> ToolOutput {
        ToolOutput {
            ok: false,
            text: text.into(),
            full: None,
            truncated: false,
            structured: Value::Null,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error(transparent)]
    Workspace(#[from] loon_workspace::WsError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("{0}")]
    Other(String),
}

/// Execution context handed to every tool.
#[derive(Clone)]
pub struct ToolCtx {
    pub ws: Arc<Workspace>,
    /// Output bytes kept inline; beyond this, `full` carries the rest for blob storage.
    pub output_limit: usize,
    pub default_timeout: Duration,
    /// Filtered environment for child processes (allowlist, not inheritance).
    pub child_env: BTreeMap<String, String>,
    /// Cooperative cancellation: long-running tools (shell) watch this.
    pub cancel: tokio::sync::watch::Receiver<bool>,
    /// Optional sink for content-addressed snapshots. Mutating file tools capture
    /// before/after bytes through it so undo/redo can restore exact content.
    pub blob_sink: Option<BlobSink>,
}

/// Content-addressed snapshot sink: bytes in, blob reference out (None when no store).
pub type BlobSink =
    std::sync::Arc<dyn Fn(&[u8]) -> Option<loon_events_type_alias::BlobRef> + Send + Sync>;

/// Alias indirection so loon-tools does not need a direct dependency bump; resolved by
/// the loon-events re-export below.
pub mod loon_events_type_alias {
    pub use loon_events::BlobRef;
}

impl Default for ToolCtx {
    fn default() -> Self {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        ToolCtx {
            ws: Arc::new(Workspace::new(".", vec![]).expect("workspace")),
            output_limit: 64 * 1024,
            default_timeout: Duration::from_secs(120),
            child_env: Default::default(),
            cancel: rx,
            blob_sink: None,
        }
    }
}

impl ToolCtx {
    /// Capture bytes through the blob sink, if one is attached.
    pub fn snapshot(&self, data: &[u8]) -> Option<loon_events_type_alias::BlobRef> {
        self.blob_sink.as_ref().and_then(|f| f(data))
    }
}

impl ToolCtx {
    /// Bound `text` to the output limit on a char boundary, retaining the full payload.
    pub fn bound(&self, text: String) -> (String, Option<Vec<u8>>, bool) {
        let bytes = text.as_bytes();
        if bytes.len() <= self.output_limit {
            return (text, None, false);
        }
        let mut cut = self.output_limit;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let full = bytes.to_vec();
        let excerpt = format!(
            "{}\n[truncated: full output {} bytes stored as blob]",
            &text[..cut],
            bytes.len()
        );
        (excerpt, Some(full), true)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Validate `args` against a spec's parameter schema and type-check the top level.
pub fn validate_args(spec: &ToolSpec, args: &Value) -> Result<(), ToolError> {
    if !args.is_object() {
        return Err(ToolError::InvalidArgs(
            "arguments must be a JSON object".into(),
        ));
    }
    let issues = crate::schema::validate(args, &spec.parameters);
    // Extra keys are ignored, not fatal. Models add `reason`, `description` or a stray
    // `path` all the time; rejecting the call costs a whole turn and teaches nothing,
    // and the tool only reads the keys it declares anyway. Missing or wrongly-typed
    // required arguments are still errors.
    let fatal: Vec<_> = issues
        .into_iter()
        .filter(|i| !i.message.starts_with("unknown field"))
        .collect();
    if fatal.is_empty() {
        Ok(())
    } else {
        Err(ToolError::InvalidArgs(
            fatal
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ))
    }
}

/// OpenAI-compatible tool definition (function calling) for the provider request.
pub fn to_provider_format(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
        }
    })
}

/// Normalize only unambiguous aliases seen from compatible coding endpoints.
/// Canonical fields always win; unknown fields remain available to MCP tools.
pub fn normalize_args(spec: &ToolSpec, mut args: Value) -> Value {
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    let mut alias = |target: &str, alternatives: &[&str]| {
        if !obj.contains_key(target) {
            if let Some(value) = alternatives.iter().find_map(|a| obj.get(*a).cloned()) {
                obj.insert(target.to_string(), value);
            }
        }
    };
    match spec.name.as_str() {
        "bash" => {
            alias("command", &["cmd", "script"]);
            alias("cwd", &["workdir", "working_directory"]);
            alias("timeout_secs", &["timeout"]);
        }
        "read" | "write" | "edit" | "fuzzy_edit" | "multi_edit" | "read_json" | "json_edit"
        | "notebook_read" | "notebook_edit" | "file_info" => {
            alias("path", &["file_path", "file", "filename"]);
            if spec.name == "read" {
                alias("offset", &["start_line"]);
                alias("limit", &["max_lines"]);
            }
            if spec.name == "write" {
                alias("content", &["text"]);
            }
            if spec.name == "edit" || spec.name == "fuzzy_edit" {
                alias("old_string", &["old_text"]);
                alias("new_string", &["new_text"]);
            }
        }
        "grep" | "glob" => alias("pattern", &["query", "search_pattern"]),
        _ => {}
    }
    args
}
