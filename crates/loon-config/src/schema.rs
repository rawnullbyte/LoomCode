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
    /// API key stored directly in the config (written by `/provider`). Takes
    /// precedence over `api_key_env`. Plain text on disk — the file is created 0600
    /// where the platform allows it.
    #[serde(default)]
    pub api_key: Option<String>,
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
    /// Accepted wire reasoning levels. Empty omits reasoning parameters for compatibility.
    /// Example: ["low", "medium", "high", "xhigh", "max"]. UI ultra maps to max.
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
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
            api_key: None,
            headers: Default::default(),
            timeout_secs: default_timeout(),
            models: vec![],
            mock: false,
            reasoning_efforts: vec![],
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
    /// Shell command prefix at a word boundary; allow rules reject shell composition.
    #[serde(default)]
    pub command_prefix: Option<String>,
    /// Exact URL hostname (no wildcard or suffix matching).
    #[serde(default)]
    pub host: Option<String>,
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
    /// Explicit asks take precedence over allow rules and bypass mode.
    #[serde(default)]
    pub ask_rules: Vec<PermissionRule>,
}

impl Default for Permissions {
    fn default() -> Self {
        Permissions {
            mode: PermissionMode::Bypass,
            extra_write_roots: vec![],
            allowed_hosts: vec![],
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
        }
    }
}

/// MCP server: either a stdio command or a Streamable HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
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
    #[serde(default = "default_palette")]
    pub palette: String,
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
            palette: default_palette(),
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
    /// Tool output above this size goes to a blob, excerpt shown.
    #[serde(default = "default_tool_output")]
    pub tool_output_bytes: usize,
    /// Retry budget per provider request attempt (combo members may override).
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Opt-in: allow combo to restart a partially-streamed request from scratch.
    #[serde(default)]
    pub allow_retry_after_partial_stream: bool,
    /// Ultra Loop: after the agent stops, run this command and keep the turn going
    /// until it passes. Empty disables the loop.
    #[serde(default)]
    pub check_command: Option<String>,
    /// How many times the Ultra Loop may push the agent back to work.
    #[serde(default = "default_ultra_rounds")]
    pub ultra_rounds: u32,
    /// Context window override when the model def has none (0 = use the model's).
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Coding effort: low | medium | high | xhigh | ultra.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub autocompact_tokens: Option<u64>,
    /// Compact at this percentage of the context window (1–99). Takes precedence over
    /// `autocompact_tokens` and follows the window when the model changes.
    #[serde(default)]
    pub autocompact_percent: Option<u8>,
    /// Enable automatic summarization independently of the model context window.
    #[serde(default = "yes")]
    pub autocompact: bool,
}

/// UI/behavior settings (the `[ui]` table). Defaults mirror Claude-Code's feel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiCfg {
    /// Disable motion while keeping all execution status information visible.
    #[serde(default = "yes")]
    pub animations: bool,
    /// Show the input placeholder tip ("Try \"write a test for …\"").
    #[serde(default = "yes")]
    pub tips: bool,
    /// Placeholder tip text (empty = rotate built-in suggestions).
    #[serde(default)]
    pub tip_text: String,
    /// Permission-mode footer row.
    #[serde(default = "yes")]
    pub footer: bool,
    /// Show the animated welcome banner.
    #[serde(default = "yes")]
    pub welcome_banner: bool,
    /// Turn spinner verbs, one shown at a time.
    #[serde(default)]
    pub spinner_verbs: Vec<String>,
    /// Max visible rows in the slash-command popup.
    #[serde(default = "default_popup_rows")]
    pub popup_rows: u16,
}

fn yes() -> bool {
    true
}
fn default_popup_rows() -> u16 {
    6
}

impl Default for UiCfg {
    fn default() -> Self {
        UiCfg {
            animations: true,
            tips: true,
            tip_text: String::new(),
            footer: true,
            welcome_banner: true,
            spinner_verbs: vec![],
            popup_rows: default_popup_rows(),
        }
    }
}

fn default_model() -> String {
    "mock".into()
}
fn default_tool_output() -> usize {
    64 * 1024
}
fn default_ultra_rounds() -> u32 {
    3
}

fn default_retries() -> u32 {
    3
}

impl Default for SessionCfg {
    fn default() -> Self {
        SessionCfg {
            model: default_model(),
            tool_output_bytes: default_tool_output(),
            max_retries: default_retries(),
            allow_retry_after_partial_stream: false,
            check_command: None,
            ultra_rounds: default_ultra_rounds(),
            context_window: None,
            effort: None,
            autocompact_tokens: None,
            autocompact_percent: None,
            autocompact: true,
        }
    }
}

/// Coding budgets are independent of provider reasoning support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Ultra,
}
impl Effort {
    pub const LEVELS: [&'static str; 5] = ["low", "medium", "high", "xhigh", "ultra"];
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "ultra" => Self::Ultra,
            _ => return None,
        })
    }
    pub fn output_tokens(self) -> u64 {
        match self {
            Self::Low => 4096,
            Self::Medium => 8192,
            Self::High => 16384,
            Self::Xhigh => 32768,
            Self::Ultra => 65536,
        }
    }
    pub fn wire_level(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Ultra => "max",
        }
    }
    pub fn guidance(self) -> &'static str {
        match self {
        Self::Low => "Use a focused implementation and the smallest meaningful verification.",
        Self::Medium => "Plan multi-step work, implement fully, and run relevant checks.",
        Self::High => "Inspect dependencies and edge cases, maintain a plan, implement and verify thoroughly.",
        Self::Xhigh => "Investigate root causes, keep a durable plan and notes, review the final diff and validate affected workflows.",
        Self::Ultra => "Carry complex work through implementation, review and verification. Record decisions and blockers; continue until the acceptance criteria are satisfied or a concrete external blocker remains.",
    }
    }
}

fn default_palette() -> String {
    "terminal".into()
}

/// A local command or HTTP lifecycle hook. Exactly one of command/url must be set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hook {
    /// Exact tool name, `*`, or a `|`-separated set of tool names.
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}
fn default_hook_timeout() -> u64 {
    10
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context7 {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Optional UI-managed credential. Environment variable takes precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_context7_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_context7_timeout")]
    pub timeout_secs: u64,
}
fn default_context7_key_env() -> String {
    "CONTEXT7_API_KEY".into()
}
fn default_context7_timeout() -> u64 {
    30
}
impl Default for Context7 {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key: None,
            api_key_env: default_context7_key_env(),
            timeout_secs: default_context7_timeout(),
        }
    }
}
