//! Typed configuration tables. Every field has a documented default; validation lives in
//! `validate.rs` so it can produce path-aware errors.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiFamily {
    #[default]
    Chat,
    Responses,
}

impl ApiFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiFamily::Chat => "chat",
            ApiFamily::Responses => "responses",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    /// Base URL including version path, e.g. `https://api.example.com/v1`.
    pub base_url: String,
    #[serde(default)]
    pub api: ApiFamily,
    /// Environment variable holding the API key. None ⇒ no auth header (local endpoints).
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Advertised models. Empty ⇒ list unknown (member models are not filtered against it).
    #[serde(default)]
    pub models: Vec<String>,
    /// True for Loon's built-in deterministic mock provider (never a network endpoint).
    #[serde(default)]
    pub mock: bool,
}

fn default_timeout() -> u64 {
    120
}

impl Default for Provider {
    fn default() -> Self {
        Provider {
            base_url: String::new(),
            api: ApiFamily::default(),
            api_key_env: None,
            headers: Default::default(),
            timeout_secs: default_timeout(),
            models: vec![],
            mock: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelDef {
    pub provider: String,
    /// Provider-side model id.
    pub id: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// Capability overrides: `supported` / `unsupported` / `unknown` per feature.
    #[serde(default)]
    pub capabilities: CapabilityOverrides,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapState {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

/// Features combos and routing consult before sending. Absent ⇒ unknown ⇒ not "supported".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CapabilityOverrides {
    #[serde(default)]
    pub tool_calling: Option<CapState>,
    #[serde(default)]
    pub parallel_tool_calls: Option<CapState>,
    #[serde(default)]
    pub vision: Option<CapState>,
    #[serde(default)]
    pub strict_schema: Option<CapState>,
}

/// A combo member: provider + model + optional per-member overrides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComboMember {
    pub provider: String,
    pub model: String,
    /// Per-member retry budget before the combo moves on (fallback) or marks failure.
    #[serde(default)]
    pub max_retries: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ComboMode {
    #[default]
    Fallback,
    #[serde(rename = "round_robin")]
    RoundRobin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Combo {
    #[serde(default)]
    pub mode: ComboMode,
    /// Priority order (fallback) / rotation order (round_robin).
    pub members: Vec<ComboMember>,
    /// Requirements the combo must satisfy as a whole; requests that no member can serve
    /// fail fast with a visible reason.
    #[serde(default)]
    pub requires: ComboRequires,
    /// Aggregate cost ceiling in (unset) currency units; reaching it pauses the turn.
    #[serde(default)]
    pub cost_ceiling: Option<f64>,
    /// Persist rotation cursor across sessions instead of per-session.
    #[serde(default)]
    pub persistent_rotation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ComboRequires {
    #[serde(default)]
    pub tool_calling: bool,
    #[serde(default)]
    pub vision: bool,
    /// Minimum context window in tokens across members.
    #[serde(default)]
    pub min_context_window: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Ask,
    /// A reviewer model checks each mutating call: clear-cut ones are allowed, the
    /// rest are forwarded to the user (interactive) or refused (headless).
    Auto,
    Allowlist,
    /// Default: no approval prompts. Explicitly denied operations still stay denied.
    #[default]
    Bypass,
}

/// A structured permission rule: tool name plus optional path constraint.
/// Path matching is prefix-based on the *resolved* path; substring tricks do not pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Exact tool name (e.g. "bash", "edit", "write").
    pub tool: String,
    /// When set, the rule only matches calls whose `path` argument resolves under it.
    #[serde(default)]
    pub path_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub mode: PermissionMode,
    /// Additional writable roots beyond the workspace cwd.
    #[serde(default)]
    pub extra_write_roots: Vec<String>,
    /// Hosts the agent may fetch. Empty list ⇒ unrestricted (subject to other policy).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Allowlist-mode rules (mode = "allowlist").
    #[serde(default)]
    pub allow_rules: Vec<PermissionRule>,
    /// Explicit denies: enforced in EVERY mode, including bypass.
    #[serde(default)]
    pub deny_rules: Vec<PermissionRule>,
}

impl Default for Permissions {
    fn default() -> Self {
        Permissions {
            mode: PermissionMode::Bypass,
            extra_write_roots: vec![],
            allowed_hosts: vec![],
            allow_rules: vec![],
            deny_rules: vec![],
        }
    }
}

/// One MCP stdio server entry: command to spawn, args, env, timeout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_mcp_timeout")]
    pub timeout_secs: u64,
}

fn default_mcp_timeout() -> u64 {
    60
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Mcp {
    /// Named stdio MCP servers; tools surface as `mcp_<server>_<tool>`.
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Appearance {
    /// `transparent` never paints a background; `opaque` is explicit opt-in only.
    #[serde(default)]
    pub mode: AppearanceMode,
    #[serde(default)]
    pub color: ColorMode,
    #[serde(default)]
    pub ascii_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceMode {
    #[default]
    Transparent,
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorMode {
    #[default]
    Auto,
    True,
    C256,
    C16,
    Mono,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            mode: AppearanceMode::Transparent,
            color: ColorMode::Auto,
            ascii_only: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCfg {
    /// Model reference: a `[models]` key or `combo:NAME`.
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// Tool output above this size goes to a blob, excerpt shown.
    #[serde(default = "default_tool_output")]
    pub tool_output_bytes: usize,
    /// Retry budget per provider request attempt (combo members may override).
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Opt-in: allow combo to restart a partially-streamed request from scratch.
    #[serde(default)]
    pub allow_retry_after_partial_stream: bool,
}

fn default_model() -> String {
    "mock".into()
}
fn default_max_steps() -> u32 {
    24
}
fn default_tool_output() -> usize {
    64 * 1024
}
fn default_retries() -> u32 {
    2
}

impl Default for SessionCfg {
    fn default() -> Self {
        SessionCfg {
            model: default_model(),
            max_steps: default_max_steps(),
            tool_output_bytes: default_tool_output(),
            max_retries: default_retries(),
            allow_retry_after_partial_stream: false,
        }
    }
}
