//! Loon agent core: the understand → act → observe → verify loop.
//!
//! Owns the conversation history, the tool scheduler, budgets and cancellation, and
//! journals every state change. The TUI and headless clients are consumers of
//! [`AgentEvent`]s; they hold no business logic.

mod builtin_skills;
pub mod catalog;
pub mod commands;
pub mod compaction;
pub mod config_ui;
pub mod est;
pub mod guidance;
pub mod hooks;
pub mod index;
pub mod loops;
pub mod mcp;
mod mcp_http;
pub mod memory;
pub mod oneshot;
pub mod output;
pub mod permissions;
pub mod prompt;
pub mod replay;
pub mod rewind;
pub mod skills;
pub mod subagent;
pub mod supervisor;
pub mod tasks;
pub mod tool_search;
pub mod verify;
pub mod worklog;

use loon_config::LoonPaths;
use loon_events::model::{EventKind, MemberAttempt, RequestOutcome, Usage};
use loon_events::{Journal, SessionMeta, Store};
use loon_providers::{AttemptEvent, ComboOutcome, ComboRunner, ProviderEvent, ProviderRegistry};
use loon_tools::{validate_args, Effects, ToolCtx, ToolRegistry};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    TextDelta(String),
    /// Discard the text streamed so far for this turn: the attempt is being retried.
    StreamReset {
        note: String,
    },
    ReasoningDelta(String),
    RequestStarted,
    RequestFinished,
    RequestUsage {
        output_tokens: u64,
    },
    OutputProgress {
        chars: u64,
    },
    ReasoningUsage {
        tokens: u64,
    },
    ToolStarted {
        name: String,
        args_excerpt: String,
        /// The human-facing command/path being run (e.g. `$ cargo test`), already
        /// formatted for display. Multi-line for shell heredocs.
        detail: String,
    },
    ToolFinished {
        name: String,
        ok: bool,
        excerpt: String,
    },
    /// Combo trail / member information worth surfacing.
    MemberNote(String),
    /// Ask-mode approval request. The UI answers through the agent's approval hook.
    ApprovalRequested {
        tool: String,
        args_excerpt: String,
    },
    /// Permission mode changed (shift+tab in the TUI, /config set).
    ModeChanged {
        mode: String,
    },
    /// /theme cycled — the UI flips its own transparent flag.
    ToggleTheme,
    CatalogUpdated {
        catalog: catalog::Catalog,
    },
    OpenCatalog {
        view: String,
        catalog: catalog::Catalog,
    },
    OpenEditor(catalog::Editor),
    OpenMenu {
        title: String,
        items: Vec<catalog::MenuItem>,
    },
    ContextUpdated {
        tokens: u64,
    },
    UiPreference {
        key: String,
        value: String,
    },
    ModelChanged {
        model: String,
    },
    EffortChanged {
        level: String,
    },
    SessionLoaded {
        blocks: Vec<(String, String)>,
    },
    ConversationRewound {
        blocks: Vec<(String, String)>,
        prompt: String,
    },
    WorkStateChanged {
        state: worklog::WorkState,
    },
    SetPalette {
        name: String,
    },
    /// `/config theme <mode>` — the UI sets its transparency explicitly.
    SetTransparent {
        transparent: bool,
    },
    /// bare /config — the UI opens its interactive settings menu.
    OpenConfigMenu,
    /// bare /model — the UI opens the model picker over these refs.
    OpenModelPicker {
        refs: Vec<String>,
    },
    /// bare /effort — the UI opens the effort picker.
    OpenEffortPicker,
    /// bare /provider — the UI opens the add-provider form.
    OpenProviderForm,
    /// Legacy event kept for UI compatibility; `/clear` now starts a new session.
    ClearTranscript,
    Warning(String),
    TurnFinished {
        status: TurnStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Cancelled,
    Failed(String),
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("model_ref", &self.model_ref)
            .field("context_window", &self.context_window)
            .field("extra_tools", &self.extra_tools.len())
            .finish()
    }
}

impl TurnStatus {
    /// Short honest status word for embedders.
    pub fn status_text(&self) -> &'static str {
        match self {
            TurnStatus::Completed => "completed",
            TurnStatus::Cancelled => "cancelled",
            TurnStatus::Failed(_) => "failed",
        }
    }
}

/// What one turn produced.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub status: TurnStatus,
    pub assistant_text: String,
    pub steps: u32,
    pub usage: Usage,
}

#[derive(Clone)]
pub struct AgentConfig {
    pub model_ref: String,
    pub tool_output_bytes: usize,
    pub max_retries: u32,
    pub allow_retry_after_partial_stream: bool,
    /// Ultra Loop: verification command run when the agent stops, and how many times it
    /// may be sent back to work before the turn is allowed to end.
    pub check_command: Option<String>,
    pub ultra_rounds: u32,
    /// Permission policy (mode + allow/deny rules) resolved from config and CLI overrides.
    pub permissions: loon_config::Permissions,
    /// Effective context window in tokens (from the model def / combo minimum). 0
    /// disables compaction. Unknown windows fall back to a conservative 128k default.
    pub context_window: u64,
    /// Host-provided extra tools (e.g. MCP), merged into the registry at construction.
    pub extra_tools: Vec<Arc<dyn loon_tools::Tool>>,
}

impl From<&loon_config::SessionCfg> for AgentConfig {
    fn from(s: &loon_config::SessionCfg) -> AgentConfig {
        AgentConfig {
            model_ref: s.model.clone(),
            tool_output_bytes: s.tool_output_bytes,
            max_retries: s.max_retries,
            allow_retry_after_partial_stream: s.allow_retry_after_partial_stream,
            check_command: s.check_command.clone(),
            ultra_rounds: s.ultra_rounds,
            permissions: loon_config::Permissions::default(),
            context_window: 128_000,
            extra_tools: Vec::new(),
        }
    }
}

pub struct Agent {
    pub journal: Arc<Journal>,
    pub registry: Arc<ProviderRegistry>,
    pub tools: ToolRegistry,
    /// Ultra Loop baseline: the check result before this session made changes.
    pub baseline: Option<crate::verify::CheckResult>,
    /// Identical rejected tool calls within the current turn. A model that repeats the
    /// same invalid call is stuck, and the turn must end instead of spinning.
    rejected_calls: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    pub ctx: ToolCtx,
    pub cfg: AgentConfig,
    pub history: Vec<loon_providers::ChatMessage>,
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
    /// Steering messages queued while a turn runs; applied at safe boundaries.
    steering_rx: mpsc::Receiver<String>,
    pub steering_tx: mpsc::Sender<String>,
    /// Live event sink for UIs/headless output; None when nobody listens.
    events: Option<mpsc::Sender<AgentEvent>>,
    /// Permission policy + redacted audit trail.
    pub permissions: Arc<permissions::PermissionEngine>,
    /// Interactive approval hook (ask mode): None ⇒ approval-required without prompting.
    pub approval_hook: Option<permissions::ApprovalHook>,
    /// Context summarizer; defaults to a model-backed summarizer (compaction.rs).
    pub summarizer: Option<compaction::Summarizer>,
    /// Injectable independent stop reviewer; production uses a no-tools model call.
    pub stop_reviewer: Option<compaction::Summarizer>,
    /// Loon data dir (for memory and friends). Set at construction.
    pub data_dir: PathBuf,
    /// Loon config dir (for /config persistence).
    pub config_dir: PathBuf,
    /// User-defined skills (custom slash commands), loaded at construction.
    pub skills: Vec<skills::Skill>,
    pub(crate) loaded_mcp: tool_search::Loaded,
    pub max_turn_steps: Option<u32>,
    pub(crate) mcp_errors: std::collections::BTreeMap<String, String>,
    pub(crate) session_hook_started: bool,
    pub(crate) settings_draft: Option<config_ui::Draft>,
    /// Loop tasks (recurring prompts). The host scheduler polls `take_due`; the agent
    /// cancels through the same registry via the loop_cancel tool.
    pub loops: Arc<loops::LoopRegistry>,
    /// /effort: requested output budget per request (max_output_tokens).
    pub effort: Option<String>,
    pub autocompact_tokens: Option<u64>,
    /// Compact at this percentage of the window; follows the window when the model changes.
    pub autocompact_percent: Option<u8>,
    /// Set after a failed compaction: (when to retry, why it failed). Automatic
    /// compaction honours it; `/compact` ignores it.
    pub compaction_backoff: Option<(Instant, String)>,
    /// Settings commands sent while a turn is running. Drained at the next request
    /// boundary so a live `/effort`, `/context 1m` or `/config …` applies to the very
    /// next request instead of waiting for the turn to end.
    live_cmds_tx: mpsc::Sender<commands::SlashCommand>,
    live_cmds_rx: mpsc::Receiver<commands::SlashCommand>,
    pub worklog: worklog::WorkLog,
    pub rotation: Arc<loon_providers::RotationState>,
    /// /goal: session goal, injected into every request as a leading system message.
    pub session_goal: Option<String>,
    /// `tips = true|false`: show the input placeholder hint (Claude-Code style).
    pub tips: bool,
    /// Whether this session renders transparently (mirrors the appearance config;
    /// `/config theme` flips it and emits SetTransparent for the UI).
    pub transparent_request: bool,
    /// Whether the session has been auto-titled from its first prompt.
    titled: std::sync::atomic::AtomicBool,
    system_prompt: String,
    session_id: loon_events::EvId,
}

impl Agent {
    /// Attach a live event sink (TUI or headless JSONL writer).
    pub fn set_event_sink(&mut self, tx: mpsc::Sender<AgentEvent>) {
        self.events = Some(tx);
    }

    async fn emit_async(&self, ev: AgentEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(ev).await;
        }
    }

    fn emit(&self, ev: AgentEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.try_send(ev);
        }
    }
}

/// Open (or create) the shared SQLite event store under the Loon data dir.
pub fn open_store(paths: &LoonPaths) -> loon_events::Result<Arc<Store>> {
    let path = paths.data_dir.join("loon.db");
    Ok(Arc::new(Store::open(&path)?))
}

/// The default toolset plus the memory tool (scoped to this Loon home + workspace).
pub fn standard_tools(paths: &LoonPaths, cwd: &Path) -> ToolRegistry {
    ToolRegistry::standard().with_extra(vec![
        Arc::new(memory::MemoryTool {
            paths: Arc::new(memory::MemoryPaths::resolve(&paths.data_dir, cwd)),
        }),
        Arc::new(skills::LoadSkillTool {
            data_dir: paths.data_dir.clone(),
            cwd: cwd.to_path_buf(),
        }),
    ])
}

impl Agent {
    /// Build the tool context shared by every constructor: workspace sandbox (with
    /// configured extra write roots) plus the journal-backed blob sink for snapshots.
    fn build_ctx(
        cfg: &AgentConfig,
        cwd: &Path,
        cancel_rx: watch::Receiver<bool>,
        journal: &Arc<Journal>,
    ) -> loon_events::Result<ToolCtx> {
        let extra_roots: Vec<PathBuf> = cfg
            .permissions
            .extra_write_roots
            .iter()
            .map(PathBuf::from)
            .collect();
        let ws = loon_workspace::Workspace::new(cwd, extra_roots).map_err(|e| {
            loon_events::EventsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let j = journal.clone();
        Ok(ToolCtx {
            ws: Arc::new(ws),
            output_limit: cfg.tool_output_bytes,
            default_timeout: std::time::Duration::from_secs(120),
            child_env: Default::default(),
            cancel: cancel_rx,
            blob_sink: Some(Arc::new(move |data: &[u8]| j.put_blob(data).ok())),
        })
    }

    /// Assemble an Agent from its parts — the single place every constructor ends,
    /// so new fields are added once rather than in each constructor.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        paths: &LoonPaths,
        cfg: AgentConfig,
        registry: Arc<ProviderRegistry>,
        tools: ToolRegistry,
        journal: Arc<Journal>,
        cwd: PathBuf,
        history: Vec<loon_providers::ChatMessage>,
        system_prompt: String,
        session_id: loon_events::EvId,
    ) -> loon_events::Result<Agent> {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (steering_tx, steering_rx) = mpsc::channel(64);
        let (live_cmds_tx, live_cmds_rx) = mpsc::channel(32);
        let ctx = Agent::build_ctx(&cfg, &cwd, cancel_rx.clone(), &journal)?;
        let mut worklog = worklog::WorkLog::new(&paths.data_dir, session_id);
        // Work state stays in memory until the session itself is written, so a session
        // that never receives a message cannot leave an orphaned worklog on disk.
        {
            let journal = journal.clone();
            worklog.follow_session(Arc::new(move || journal.is_materialized()));
        }
        let mut cfg = cfg;
        if let Ok(state) = worklog.read() {
            if let Some(model) = state.model {
                if registry.resolve(&model).is_ok() {
                    cfg.model_ref = model;
                }
            }
        }
        let tools = tools
            .without(&[
                "update_plan",
                "note",
                "load_skill",
                "memory",
                "read_output",
                "task",
                "todo_write",
                "todo_read",
            ])
            .with_extra(vec![
                Arc::new(skills::LoadSkillTool {
                    data_dir: paths.data_dir.clone(),
                    cwd: cwd.clone(),
                }),
                Arc::new(memory::MemoryTool {
                    paths: Arc::new(memory::MemoryPaths::resolve(&paths.data_dir, &cwd)),
                }),
                Arc::new(output::OutputTool {
                    journal: journal.clone(),
                }),
                Arc::new(worklog::TodoTool {
                    log: worklog.clone(),
                    read_only: false,
                }),
                Arc::new(worklog::TodoTool {
                    log: worklog.clone(),
                    read_only: true,
                }),
                Arc::new(tasks::TaskTool {
                    log: worklog.clone(),
                }),
                Arc::new(worklog::PlanTool {
                    log: worklog.clone(),
                }),
                Arc::new(worklog::NoteTool {
                    log: worklog.clone(),
                }),
            ]);
        let saved = loon_config::Config::load_effective(paths, &cwd)
            .ok()
            .map(|(c, _)| c);
        let tools = if tools.get("context7_search").is_some() {
            let options = saved
                .as_ref()
                .map(|c| loon_tools::context7::Options {
                    enabled: c.context7.enabled,
                    api_key: c.context7.api_key.clone(),
                    api_key_env: c.context7.api_key_env.clone(),
                    timeout_secs: c.context7.timeout_secs,
                    ..Default::default()
                })
                .unwrap_or_default();
            tools
                .without(&[
                    "context7_search",
                    "context7_resolve_library",
                    "context7_query_docs",
                ])
                .with_extra(
                    [
                        loon_tools::context7::Operation::Search,
                        loon_tools::context7::Operation::Resolve,
                        loon_tools::context7::Operation::Query,
                    ]
                    .into_iter()
                    .map(|operation| {
                        Arc::new(loon_tools::context7::Context7Tool {
                            operation,
                            options: options.clone(),
                        }) as Arc<dyn loon_tools::Tool>
                    })
                    .collect(),
                )
        } else {
            tools
        };
        let audit_path = paths.state_dir.join("audit.jsonl");
        let mut agent = Agent {
            permissions: Arc::new(permissions::PermissionEngine::new(
                &cfg.permissions,
                Some(audit_path),
            )),
            skills: skills::load_with_builtins(&paths.data_dir, &cwd),
            loaded_mcp: Default::default(),
            max_turn_steps: None,
            mcp_errors: Default::default(),
            session_hook_started: false,
            settings_draft: None,
            data_dir: paths.data_dir.clone(),
            config_dir: paths.config_dir.clone(),
            journal,
            registry,
            tools,
            baseline: None,
            rejected_calls: std::sync::Mutex::new(std::collections::HashMap::new()),
            ctx,
            history,
            cancel_tx,
            cancel_rx,
            steering_rx,
            steering_tx,
            events: None,
            approval_hook: None,
            summarizer: None,
            stop_reviewer: None,
            loops: Arc::new(loops::LoopRegistry::new()),
            effort: worklog
                .read()
                .ok()
                .and_then(|s| s.effort)
                .or_else(|| saved.as_ref().and_then(|c| c.session.effort.clone())),
            session_goal: worklog.read().ok().and_then(|s| s.goal),
            autocompact_tokens: saved.as_ref().and_then(|c| c.session.autocompact_tokens),
            autocompact_percent: saved.as_ref().and_then(|c| c.session.autocompact_percent),
            compaction_backoff: None,
            live_cmds_tx,
            live_cmds_rx,
            worklog,
            rotation: Arc::new(loon_providers::RotationState::new()),
            tips: saved.as_ref().map(|c| c.ui.tips).unwrap_or(true),
            transparent_request: saved
                .as_ref()
                .map(|c| c.appearance.mode == loon_config::AppearanceMode::Transparent)
                .unwrap_or(true),
            titled: std::sync::atomic::AtomicBool::new(false),
            cfg,
            system_prompt,
            session_id,
        };
        agent.refresh_tool_search();
        Ok(agent)
    }

    /// Create a session agent. `cwd` is the workspace root; AGENTS.md there (if any) is
    /// included as task guidance.
    pub fn new_session(
        paths: &LoonPaths,
        cfg: AgentConfig,
        registry: Arc<ProviderRegistry>,
        tools: ToolRegistry,
        cwd: PathBuf,
        headless: bool,
    ) -> loon_events::Result<Agent> {
        let session_id = loon_events::EvId::now_v7();
        let meta = SessionMeta::fresh(
            session_id,
            "session",
            chrono_ms(),
            cwd.display().to_string(),
            headless,
        );
        let store = open_store(paths)?;
        // The session exists only in memory until its first write: no row, no events.
        // Opening the app and quitting without sending anything therefore leaves
        // nothing behind in `/resume` or `loom sessions list`.
        let journal = Arc::new(Journal::create_pending(
            &store,
            &meta,
            Some(EventKind::SessionStarted {
                cwd: cwd.display().to_string(),
                model: cfg.model_ref.clone(),
                headless,
            }),
        ));
        let system = prompt::system_prompt_with_memory(
            &cwd,
            &memory::MemoryPaths::resolve(&paths.data_dir, &cwd),
        );
        Agent::assemble(
            paths,
            cfg,
            registry,
            tools,
            journal,
            cwd,
            Vec::new(),
            system,
            session_id,
        )
    }

    pub fn session_id(&self) -> loon_events::EvId {
        self.session_id
    }

    /// True while this session exists only in memory; the first append, rename or resume
    /// materializes it into the store.
    pub fn is_pending(&self) -> bool {
        self.journal.is_pending()
    }

    /// Called after a journal write that may have materialized the session: work state
    /// staged while the session was in memory is written through now, so a later
    /// `resume` (a different process even) reads it from disk.
    pub(crate) fn flush_worklog_if_durable(&self) {
        if self.journal.is_materialized() {
            let _ = self.worklog.flush_staged();
        }
    }

    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }

    /// Replace this agent wholesale (/new, /resume): the replacement keeps THIS
    /// agent's event sink, approval hook and summarizer so the UI keeps streaming.
    pub fn replace_with(&mut self, mut other: Agent) {
        other.events = self.events.take();
        other.approval_hook = self.approval_hook.take();
        other.summarizer = self.summarizer.take();
        other.stop_reviewer = self.stop_reviewer.take();
        *self = other;
    }

    /// A lock-free cancellation handle. A running turn holds the agent mutex for its
    /// whole duration, so cancellers (TUI, tests) must signal through this clone
    /// instead of taking the lock — otherwise cancel queues behind the turn it is
    /// meant to stop.
    /// Reconstructed LoonPaths for slash commands that need the store (/new,
    /// /resume): derived from the dirs captured at construction.
    pub fn loon_paths(&self) -> loon_config::LoonPaths {
        let base = self
            .data_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.data_dir.clone());
        loon_config::LoonPaths {
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.clone(),
            state_dir: base.join("state"),
            cache_dir: base.join("cache"),
            runtime_dir: base.join("runtime"),
        }
    }

    /// /export: write the session as Markdown (events → readable transcript).
    pub fn export_markdown(&self, path: &std::path::Path) -> Result<String, String> {
        let events = self.journal.events().map_err(|e| e.to_string())?;
        let mut out = format!("# Loon session {}\n\n", &self.session_id.to_string()[..8]);
        for e in &events {
            match &e.kind {
                loon_events::model::EventKind::UserMessage { text } => {
                    out.push_str(&format!("## you\n\n{text}\n\n"));
                }
                loon_events::model::EventKind::TextDelta { text, .. } => {
                    out.push_str(text);
                }
                loon_events::model::EventKind::ToolExecutionFinished { name, excerpt, .. } => {
                    out.push_str(&format!(
                        "\n\n> `{name}`: {}\n\n",
                        excerpt.lines().next().unwrap_or("")
                    ));
                }
                _ => {}
            }
        }
        std::fs::write(path, out.trim_end().to_string() + "\n")
            .map_err(|e| format!("export: {e}"))?;
        Ok(path.display().to_string())
    }

    pub fn cancel_handle(&self) -> tokio::sync::watch::Sender<bool> {
        self.cancel_tx.clone()
    }

    /// Shift+Tab cycle order: bypass → auto → ask (manual) → bypass. (Allowlist
    /// stays available in config for scripted setups but is out of the cycle.)
    pub fn next_permission_mode(mode: loon_config::PermissionMode) -> loon_config::PermissionMode {
        use loon_config::PermissionMode::*;
        match mode {
            Bypass => Auto,
            Auto => Ask,
            Ask | Allowlist => Bypass,
        }
    }

    /// Auto mode's reviewer verdict: a no-tools model call judging one tool call.
    /// Returns None when the reviewer said nothing usable (model down, unusable
    /// answer) — the caller then forwards to the user like ask mode.
    async fn reviewer_verdict(&self, tool: &str, args: &serde_json::Value) -> Option<bool> {
        let transcript = format!(
            "tool: {tool}\narguments: {args}\nworkspace: {}",
            self.ctx.ws.root().display()
        );
        let summarizer = match &self.summarizer {
            Some(s) => s.clone(),
            None => compaction::model_reviewer(self.registry.clone(), self.cfg.model_ref.clone()),
        };
        let answer = match summarizer(format!(
            "You are a safety reviewer for a coding agent. Judge whether allowing this \
             operation is low-risk for the user's machine and repository (writes inside \
             the workspace, normal build/test commands: low risk; deleting outside the \
             workspace, exfiltration, destructive shell, credential access: high risk). \
             Answer with exactly one word: ALLOW or DENY.\n\n{transcript}"
        ))
        .await
        {
            Ok(answer) => answer,
            // A failed reviewer cannot vouch for safety; fall through to asking the user.
            Err(reason) => {
                self.emit_async(AgentEvent::Warning(format!(
                    "auto review skipped: {reason}"
                )))
                .await;
                return None;
            }
        };
        Self::parse_reviewer_verdict(&answer)
    }

    /// Parse ALLOW/DENY from a reviewer answer; anything else is None (no verdict).
    pub fn parse_reviewer_verdict(answer: &str) -> Option<bool> {
        let upper = answer.to_uppercase();
        let allow = upper.matches("ALLOW").count();
        let deny = upper.matches("DENY").count();
        match (allow > 0, deny > 0) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }

    /// Replace this agent's cancellation signal with an external one (subagents stop
    /// when their parent stops). Cancelling *this* agent afterwards only flips the
    /// external channel's local flag consumers already hold — the parent remains the
    /// source of truth.
    pub fn adopt_cancel_channel(&mut self, rx: tokio::sync::watch::Receiver<bool>) {
        self.cancel_rx = rx.clone();
        self.ctx.cancel = rx;
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancel_rx.borrow()
    }

    /// Forget the turn that is being cancelled: drop the user message (and anything the
    /// aborted turn journaled after it) from live history, and journal a conversation
    /// rewind so a resume replays the same thing. The prompt is the user's again — it
    /// goes back to the input bar, not into the transcript.
    ///
    /// Returns the text that was dropped, or None when nothing of this turn was recorded.
    pub fn forget_cancelled_turn(&mut self) -> Option<String> {
        let events = self.journal.events().ok()?;
        let bounds = crate::rewind::request_boundaries(&events);
        // The newest boundary is the turn being cancelled; without one, fall back to the
        // last user message so the prompt still comes back.
        let truncate_to = match bounds.last() {
            Some(b) => b.truncate_to_seq,
            None => {
                let last_user = events
                    .iter()
                    .rev()
                    .find(|e| matches!(&e.kind, EventKind::UserMessage { text } if crate::rewind::visible_user_prompt(text)))?;
                last_user.seq.saturating_sub(1)
            }
        };
        // Drop live history from the cancelled user message onward.
        let mut dropped_prompt = None;
        let mut keep = self.history.len();
        for (i, message) in self.history.iter().enumerate().rev() {
            if message.role == loon_providers::Role::User
                && !message.text.starts_with("[steering]")
                && !message.text.starts_with("[ultra loop]")
                && !message.text.starts_with("[Local lifecycle hook]")
            {
                dropped_prompt = Some(message.text.clone());
                keep = i;
                break;
            }
        }
        self.history.truncate(keep);
        let _ = self.journal.append(
            None,
            EventKind::Rewound {
                target_request: loon_events::EvId::now_v7(),
                mode: "conversation".into(),
                truncate_to_seq: truncate_to,
                files_restored: 0,
                files_conflicted: 0,
            },
        );
        dropped_prompt
    }

    /// Queue a steering message; consumed at the next safe boundary.
    pub fn steer(&self, text: String) {
        let _ = self.steering_tx.try_send(text);
    }

    /// Lock-free handle for settings commands that must apply to the next request even
    /// while this agent is busy running a turn.
    pub fn live_command_sender(&self) -> mpsc::Sender<commands::SlashCommand> {
        self.live_cmds_tx.clone()
    }

    /// Apply settings commands queued while the turn was running. Called at the request
    /// boundary; only session-live settings are honoured, everything else is reported.
    fn apply_live_commands(&mut self) {
        while let Ok(cmd) = self.live_cmds_rx.try_recv() {
            let (key, value) = match &cmd {
                commands::SlashCommand::Effort { level } if !level.is_empty() => {
                    ("effort".to_string(), level.clone())
                }
                commands::SlashCommand::AutoCompact { setting } if setting != "toggle" => {
                    match loon_config::tokens::parse_token_spec(setting) {
                        Some(loon_config::tokens::TokenSpec::Percent(p)) => {
                            ("autocompact_percent".to_string(), p.to_string())
                        }
                        Some(loon_config::tokens::TokenSpec::Tokens(n)) => {
                            ("autocompact_tokens".to_string(), n.to_string())
                        }
                        None if setting == "off" => ("context_window".to_string(), "0".to_string()),
                        None if setting == "on" => {
                            ("context_window".to_string(), "128k".to_string())
                        }
                        None => continue,
                    }
                }
                commands::SlashCommand::Context { size: Some(size) } => {
                    match loon_config::tokens::parse_token_spec(size) {
                        Some(loon_config::tokens::TokenSpec::Percent(p)) => {
                            ("autocompact_percent".to_string(), p.to_string())
                        }
                        Some(loon_config::tokens::TokenSpec::Tokens(n)) => {
                            ("context_window".to_string(), n.to_string())
                        }
                        None => continue,
                    }
                }
                commands::SlashCommand::Config { args } => {
                    let args: Vec<&str> = args
                        .iter()
                        .map(String::as_str)
                        .filter(|a| !matches!(*a, "set" | "menu-set"))
                        .collect();
                    let Some(first) = args.first() else { continue };
                    let (key, inline) = match first.split_once('=') {
                        Some((k, v)) => (k, Some(v)),
                        None => (*first, None),
                    };
                    if !matches!(
                        key,
                        "effort"
                            | "context_window"
                            | "autocompact"
                            | "autocompact_tokens"
                            | "autocompact_percent"
                            | "permission_mode"
                            | "tool_output_bytes"
                            | "max_retries"
                            | "ultra_rounds"
                            | "plan_mode"
                            | "session_goal"
                    ) {
                        self.emit(crate::AgentEvent::Warning(format!(
                            "/config {key} needs the idle session; it applies after this turn"
                        )));
                        continue;
                    }
                    let value = match inline {
                        Some(v) => v.to_string(),
                        None => args[1..].join(" "),
                    };
                    if value.is_empty() {
                        continue;
                    }
                    (key.to_string(), value)
                }
                _ => continue,
            };
            match self.config_apply(&key, &value) {
                Ok(report) => self.emit(crate::AgentEvent::MemberNote(format!(
                    "applied at this request: {report}"
                ))),
                Err(e) => self.emit(crate::AgentEvent::Warning(format!(
                    "queued setting `{key}` refused: {e}"
                ))),
            }
        }
    }

    /// Drain settings queued while a turn runs. Public so the TUI driver and tests share
    /// exactly one implementation; production calls it at each request boundary.
    pub fn apply_pending_live_commands(&mut self) {
        self.apply_live_commands();
    }

    /// Rebuild an agent over an existing session (resume). Replays journal into history.
    pub fn resume(
        paths: &LoonPaths,
        session_id: loon_events::EvId,
        cfg: AgentConfig,
        registry: Arc<ProviderRegistry>,
        tools: ToolRegistry,
    ) -> loon_events::Result<Agent> {
        let store = open_store(paths)?;
        let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), session_id)?;
        let mut cwd = String::new();
        for ev in &events {
            if let EventKind::SessionStarted { cwd: c, .. } = &ev.kind {
                cwd = c.clone();
            }
        }
        let history = replay::replay_history(&events, None);
        let journal = Arc::new(journal);
        let system = prompt::system_prompt_with_memory(
            Path::new(&cwd),
            &memory::MemoryPaths::resolve(&paths.data_dir, Path::new(&cwd)),
        );
        let resumed = Agent::assemble(
            paths,
            cfg,
            registry,
            tools,
            journal,
            PathBuf::from(&cwd),
            history,
            system,
            session_id,
        )?;
        resumed
            .titled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(resumed)
    }

    /// Run one user turn to completion, journaling everything.
    ///
    /// An empty or whitespace-only prompt is refused up front: journaling it would create
    /// a session with no user message and a placeholder title, which then shows up in
    /// `/resume` as a phantom entry.
    pub async fn run_turn(&mut self, user_text: &str) -> TurnResult {
        if user_text.trim().is_empty() {
            return TurnResult {
                status: TurnStatus::Failed("empty prompt; nothing to do".into()),
                assistant_text: String::new(),
                steps: 0,
                usage: Usage::default(),
            };
        }
        self.system_prompt = prompt::system_prompt_with_memory(
            self.ctx.ws.root(),
            &memory::MemoryPaths::resolve(&self.data_dir, self.ctx.ws.root()),
        );
        self.repair_interrupted_calls();
        let _ = self.cancel_tx.send(false);
        self.emit_async(AgentEvent::TurnStarted).await;
        if !self.session_hook_started {
            self.session_hook_started = true;
            let hook = self.run_hooks("SessionStart", serde_json::json!({})).await;
            if !hook.context.is_empty() {
                self.record_hook_context(&hook.context);
            }
        }
        let hook = self
            .run_hooks("UserPromptSubmit", serde_json::json!({"prompt":user_text}))
            .await;
        if hook.blocked {
            let reason = format!("Prompt blocked by hook: {}", hook.context);
            self.emit_async(AgentEvent::Warning(reason.clone())).await;
            let status = TurnStatus::Failed(reason);
            self.emit_async(AgentEvent::TurnFinished {
                status: status.clone(),
            })
            .await;
            return TurnResult {
                status,
                assistant_text: String::new(),
                steps: 0,
                usage: Usage::default(),
            };
        }
        if !hook.context.is_empty() {
            self.record_hook_context(&hook.context);
        }
        // Auto-title: the first real prompt names the session (shows in /resume).
        if !self.titled.load(std::sync::atomic::Ordering::Relaxed) && !user_text.trim().is_empty() {
            let title: String = user_text.trim().chars().take(60).collect();
            let title = if user_text.trim().chars().count() > 60 {
                format!("{title}…")
            } else {
                title
            };
            let _ = self.journal.rename_session(&title);
            self.titled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // A failed write (full or read-only disk) must end the turn with a reason, not
        // abort the process: the caller's UI stays alive and the user can act on it.
        if let Err(e) = self.journal.append(
            None,
            EventKind::UserMessage {
                text: user_text.to_string(),
            },
        ) {
            let reason = format!("cannot journal the message: {e}");
            self.emit_async(AgentEvent::Warning(reason.clone())).await;
            let status = TurnStatus::Failed(reason);
            self.emit_async(AgentEvent::TurnFinished {
                status: status.clone(),
            })
            .await;
            return TurnResult {
                status,
                assistant_text: String::new(),
                steps: 0,
                usage: Usage::default(),
            };
        }
        self.flush_worklog_if_durable();
        self.history
            .push(loon_providers::ChatMessage::user(user_text));
        self.execute_turn().await
    }

    /// Execute the agent loop for the current history state (used after resume too).
    pub async fn execute_turn(&mut self) -> TurnResult {
        // A new turn starts fresh: a cancellation left over from a previous turn (or
        // from an idle Esc/Ctrl+C) must not poison this one. Cancelling DURING the
        // turn still works — the flag is re-set from the UI while this loop runs.
        let _ = self.cancel_tx.send(false);
        self.rejected_calls.lock().expect("rejection lock").clear();
        let turn_id = loon_events::EvId::now_v7();
        let _ = self
            .journal
            .append(None, EventKind::TurnStarted { turn: turn_id });
        self.flush_worklog_if_durable();
        let mut steps: u32 = 0;
        let mut ultra_rounds: u32 = 0;
        let mut supervisor_rounds: u32 = 0;
        let mut total_usage = Usage::default();
        let mut assistant_text = String::new();
        let mut status = TurnStatus::Completed;

        loop {
            if self.is_cancelled() {
                status = TurnStatus::Cancelled;
                break;
            }
            if self.max_turn_steps.is_some_and(|limit| steps >= limit) {
                status =
                    TurnStatus::Failed("subagent maxTurns reached; task remains incomplete".into());
                break;
            }
            steps += 1;

            // ── safe boundary: apply queued steering messages ─────────────────────
            while let Ok(msg) = self.steering_rx.try_recv() {
                tracing::info!("steering applied at step boundary");
                let _ = self.journal.append(
                    None,
                    EventKind::UserMessage {
                        text: format!("[steering] {msg}"),
                    },
                );
                self.history.push(loon_providers::ChatMessage::user(format!(
                    "[steering] {msg}"
                )));
            }
            // Settings typed while the turn ran apply to this request, not the next turn.
            self.apply_live_commands();

            // ── one provider request (combo-aware) ────────────────────────────────
            // Context guard first: summarize older exchanges if the next request would
            // cross the compaction threshold.
            self.maybe_compact().await;
            let request_id = loon_events::EvId::now_v7();
            let effort = loon_config::Effort::parse(self.effort.as_deref().unwrap_or("medium"))
                .unwrap_or_default();
            let max_output_tokens = Some(effort.output_tokens());
            let req = loon_providers::ChatRequest {
                model: self.cfg.model_ref.clone(),
                messages: self.assemble_messages(),
                tools: self.provider_tools(),
                max_output_tokens,
                temperature: None,
                reasoning_effort: Some(effort.wire_level().into()),
            };
            self.emit_async(AgentEvent::RequestStarted).await;
            let est_tokens = est::estimate_messages(&req.messages);
            let (tx, mut rx) = mpsc::channel::<AttemptEvent>(512);
            let registry = self.registry.clone();
            let model_ref = self.cfg.model_ref.clone();
            let allow_partial = self.cfg.allow_retry_after_partial_stream;
            let mut runner = ComboRunner::new(registry);
            runner.rotation = self.rotation.clone();
            runner.max_retries = self.cfg.max_retries;
            runner.allow_retry_after_partial_stream = allow_partial;
            let req_for_task = req.clone();
            let mut handle = supervisor::AbortTask(tokio::spawn(async move {
                runner.run(&model_ref, &req_for_task, tx).await
            }));

            let mut step_text = String::new();
            let mut tool_calls = Vec::new();
            let mut saw_tool_delta = false;
            let mut observed_ids = std::collections::HashMap::new();
            let mut member_logged = false;
            let mut member_for_request: Option<MemberAttempt> = None;

            loop {
                tokio::select! {
                    ev = rx.recv() => {
                        let Some(ev) = ev else { break };
                        if !member_logged {
                            member_logged = true;
                            member_for_request = Some(ev.attempt.clone());
                            let _ = self.journal.append(
                                Some(request_id),
                                EventKind::RequestStarted {
                                    request: request_id,
                                    turn: turn_id,
                                    member: ev.attempt.clone(),
                                    est_context_tokens: Some(est_tokens),
                                },
                            );
                        }
                        match ev.event {
                            ProviderEvent::TextDelta { text } => {
                                step_text.push_str(&text);
                                let _ = self.journal.append(Some(request_id), EventKind::TextDelta { request: request_id, text: text.clone() });
                                self.emit_async(AgentEvent::TextDelta(text)).await;
                            }
                            ProviderEvent::ReasoningDelta { text } => {
                                let _ = self.journal.append(Some(request_id), EventKind::ReasoningDelta { request: request_id, text: text.clone() });
                                self.emit_async(AgentEvent::ReasoningDelta(text)).await;
                            }
                            ProviderEvent::ToolCallComplete { call } => {
                                if !saw_tool_delta {self.emit_async(AgentEvent::OutputProgress {chars:call.arguments_json.chars().count() as u64}).await;}
                                let observed_id=loon_events::EvId::now_v7();
                                observed_ids.insert(call.id.clone(),observed_id);
                                let _ = self.journal.append(
                                    Some(request_id),
                                    EventKind::ToolCallObserved {
                                        request: request_id,
                                        call: observed_id,
                                        wire_id: call.id.clone(),
                                        name: call.name.clone(),
                                        arguments_json: call.arguments_json.clone(),
                                    },
                                );
                                tool_calls.push(call);
                            }
                            ProviderEvent::Usage(u) => {
                                if let Some(output_tokens)=u.output_tokens {self.emit_async(AgentEvent::RequestUsage {output_tokens}).await;}
                                if let Some(tokens) = u.reasoning_tokens { self.emit_async(AgentEvent::ReasoningUsage { tokens }).await; }
                                total_usage = Usage {
                                    input_tokens: total_usage.input_tokens.max(u.input_tokens),
                                    output_tokens: Some(total_usage.output_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0)),
                                    cached_input_tokens: total_usage.cached_input_tokens.max(u.cached_input_tokens),
                                    reasoning_tokens: total_usage.reasoning_tokens.max(u.reasoning_tokens),
                                };
                            }
                            ProviderEvent::Failure { error } => {
                                let _ = self.journal.append(Some(request_id), EventKind::Warning { message: format!("provider: {error}") });
                            }
                            // The attempt is being repeated: drop what it streamed so the
                            // retried answer is not appended to a half-finished one.
                            ProviderEvent::Retrying { reason } => {
                                step_text.clear();
                                tool_calls.clear();
                                observed_ids.clear();
                                saw_tool_delta=false;
                                let _ = self.journal.append(Some(request_id), EventKind::Warning { message: format!("retrying: {reason}") });
                                self.emit_async(AgentEvent::StreamReset { note: reason }).await;
                            }
                            ProviderEvent::ToolCallDelta {args_delta,..} => {saw_tool_delta=true;self.emit_async(AgentEvent::OutputProgress {chars:args_delta.chars().count() as u64}).await;}
                            ProviderEvent::Finished { .. } => {}
                        }
                    }
                    _ = cancel_changed(&mut self.cancel_rx) => {
                        self.cancel();
                        handle.0.abort();
                        break;
                    }
                }
            }
            let combo_outcome: Option<Result<ComboOutcome, String>> = match (&mut handle.0).await {
                Ok(r) => Some(r),
                Err(e) => Some(Err(format!("request task panicked: {e}"))),
            };

            self.emit_async(AgentEvent::RequestFinished).await;
            // ── journal the request outcome ───────────────────────────────────────
            let (outcome_kind, _served_member) = match &combo_outcome {
                Some(Ok(outcome)) => {
                    for (m, reason) in &outcome.trail {
                        let _ = self.journal.append(
                            None,
                            EventKind::Warning {
                                message: format!("combo: skipped {m}: {reason}"),
                            },
                        );
                        self.emit_async(AgentEvent::MemberNote(format!("skipped {m}: {reason}")))
                            .await;
                    }
                    let member = outcome.served.clone().or(member_for_request.clone());
                    (
                        EventKind::RequestFinished {
                            request: request_id,
                            outcome: outcome.outcome.clone(),
                            usage: outcome.usage.clone(),
                            usage_reported: outcome.usage_reported,
                        },
                        member,
                    )
                }
                Some(Err(e)) => (
                    EventKind::RequestFinished {
                        request: request_id,
                        outcome: RequestOutcome::Errored {
                            error: e.clone(),
                            retriable: false,
                        },
                        usage: Usage::default(),
                        usage_reported: false,
                    },
                    member_for_request.clone(),
                ),
                None => (
                    EventKind::RequestFinished {
                        request: request_id,
                        outcome: RequestOutcome::Unknown {
                            reason: "request task ended without report".into(),
                        },
                        usage: Usage::default(),
                        usage_reported: false,
                    },
                    member_for_request.clone(),
                ),
            };
            let _ = self.journal.append(Some(request_id), outcome_kind);

            // ── classify and continue or stop ─────────────────────────────────────
            if self.is_cancelled() {
                status = TurnStatus::Cancelled;
                break;
            }
            match &combo_outcome {
                Some(Ok(o)) => match &o.outcome {
                    RequestOutcome::Completed { .. } => {}
                    RequestOutcome::Cancelled => {
                        status = TurnStatus::Cancelled;
                        break;
                    }
                    RequestOutcome::Unknown { reason } => {
                        status = TurnStatus::Failed(format!("request outcome unknown: {reason}"));
                        let _ = self.journal.append(
                            None,
                            EventKind::Error {
                                message: format!("request outcome unknown: {reason}"),
                            },
                        );
                        break;
                    }
                    RequestOutcome::Errored { error, retriable } => {
                        status = TurnStatus::Failed(error.clone());
                        let _ = self.journal.append(
                            None,
                            EventKind::Error {
                                message: format!("request failed (retriable={retriable}): {error}"),
                            },
                        );
                        self.emit_async(AgentEvent::Warning(format!("request failed: {error}")))
                            .await;
                        break;
                    }
                },
                Some(Err(e)) => {
                    status = TurnStatus::Failed(e.clone());
                    let _ = self
                        .journal
                        .append(None, EventKind::Error { message: e.clone() });
                    break;
                }
                None => unreachable!(),
            }
            if self.is_cancelled() {
                status = TurnStatus::Cancelled;
                break;
            }

            assistant_text.push_str(&step_text);
            self.history.push(loon_providers::ChatMessage {
                role: loon_providers::Role::Assistant,
                text: step_text,
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
            });
            if tool_calls.is_empty() {
                let hook = self
                    .run_hooks(
                        "Stop",
                        serde_json::json!({"last_assistant_message":assistant_text,"steps":steps}),
                    )
                    .await;
                if hook.blocked {
                    self.record_hook_context(&format!(
                        "Stop hook requires more work: {}",
                        hook.context
                    ));
                    continue;
                }
                match self.ultra_loop_verdict(&mut ultra_rounds).await {
                    Ok(true) => continue,
                    Err(e) if self.ultraloop_enabled() => {
                        let prompt=format!("[ultraloop stop reason] Required verification is still failing: {e}. Review must continue with a different repair approach; completion is not supported.");
                        let _ = self.journal.append(
                            None,
                            EventKind::UserMessage {
                                text: prompt.clone(),
                            },
                        );
                        self.history.push(loon_providers::ChatMessage::user(prompt));
                        ultra_rounds = 0;
                    }
                    Err(e) => {
                        status = TurnStatus::Failed(e);
                        break;
                    }
                    Ok(false) => {}
                }
                supervisor_rounds += 1;
                match self.supervise_stop(supervisor_rounds, turn_id).await {
                    Ok(true) => continue,
                    Err(e) => {
                        // The turn ends, but the goal is not silently dropped: a review
                        // that could not run leaves the goal explicitly unverified, and
                        // the next turn is told to resume it rather than start over.
                        self.hold_unverified_goal(&e).await;
                        status = TurnStatus::Failed(e);
                    }
                    Ok(false) => {}
                }
                break;
            }

            let mut index = 0;
            while index < tool_calls.len() {
                if self.is_cancelled() {
                    status = TurnStatus::Cancelled;
                    break;
                }
                let mut end = index + 1;
                let readonly = |call: &loon_providers::ToolCallReq| {
                    self.tools
                        .get(&call.name)
                        .map(|t| t.spec().effects == Effects::ReadOnly)
                        .unwrap_or(false)
                };
                if readonly(&tool_calls[index]) {
                    while end < tool_calls.len() && readonly(&tool_calls[end]) {
                        end += 1;
                    }
                }
                let results = {
                    use futures::StreamExt;
                    let agent: &Agent = self;
                    let ids = &observed_ids;
                    let jobs = tool_calls[index..end].iter().cloned().enumerate().map(
                        |(offset, call): (usize, loon_providers::ToolCallReq)| async move {
                            let id = ids
                                .get(&call.id)
                                .copied()
                                .unwrap_or_else(loon_events::EvId::now_v7);
                            (offset, agent.execute_tool(&id, &call).await)
                        },
                    );
                    let mut results = futures::stream::iter(jobs)
                        .buffer_unordered(6)
                        .collect::<Vec<_>>()
                        .await;
                    results.sort_by_key(|(i, _)| *i);
                    results
                };
                let mut stuck = false;
                for (offset, result) in results {
                    let call = &tool_calls[index + offset];
                    if !result.ok {
                        let count = {
                            let mut failures = self.rejected_calls.lock().expect("rejection lock");
                            let key = format!(
                                "{}\u{1}{}",
                                call.name,
                                serde_json::from_str::<serde_json::Value>(&call.arguments_json)
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|_| call.arguments_json.clone())
                            );
                            let count = failures.entry(key).or_insert(0);
                            *count += 1;
                            *count
                        };
                        if count >= MAX_IDENTICAL_REJECTIONS {
                            stuck = true;
                        }
                    }
                    if call.name == "update_plan" || call.name == "note" {
                        if let Ok(state) = self.worklog.read() {
                            self.emit_async(AgentEvent::WorkStateChanged { state })
                                .await;
                        }
                    }
                    self.history.push(loon_providers::ChatMessage::tool_result(
                        call.id.clone(),
                        result.model_text,
                    ));
                }
                if stuck {
                    status = TurnStatus::Failed(format!(
                        "the model repeated the same rejected tool call {MAX_IDENTICAL_REJECTIONS} times; stopping instead of looping"
                    ));
                    break;
                }
                index = end;
            }
            // Any non-completed status ends the turn: a failed batch must not start
            // another provider request.
            if status != TurnStatus::Completed {
                if !self.is_cancelled() && self.ultraloop_enabled() {
                    self.repair_interrupted_calls();
                    let prompt=format!("[ultraloop stop reason] Main execution stopped: {status:?}. Identify a different valid approach; do not repeat the same failing call unchanged.");
                    let _ = self.journal.append(
                        None,
                        EventKind::UserMessage {
                            text: prompt.clone(),
                        },
                    );
                    self.history.push(loon_providers::ChatMessage::user(prompt));
                    supervisor_rounds += 1;
                    match self.supervise_stop(supervisor_rounds, turn_id).await {
                        Ok(true) => {
                            self.rejected_calls.lock().expect("rejection lock").clear();
                            status = TurnStatus::Completed;
                            continue;
                        }
                        Err(e) => {
                            self.hold_unverified_goal(&e).await;
                            status = TurnStatus::Failed(e);
                        }
                        Ok(false) => {}
                    }
                }
                break;
            }
        }

        if self.is_cancelled() {
            status = TurnStatus::Cancelled;
        }
        if matches!(status, TurnStatus::Failed(_)) {
            let _ = self
                .run_hooks(
                    "StopFailure",
                    serde_json::json!({"status":format!("{status:?}")}),
                )
                .await;
        }
        let _ = self.journal.append(
            None,
            EventKind::TurnFinished {
                turn: turn_id,
                reason: format!("{status:?}"),
            },
        );
        self.emit_async(AgentEvent::ContextUpdated {
            tokens: self.estimated_context_tokens(),
        })
        .await;
        self.emit_async(AgentEvent::TurnFinished {
            status: status.clone(),
        })
        .await;
        TurnResult {
            status,
            assistant_text,
            steps,
            usage: total_usage,
        }
    }

    async fn execute_tool(
        &self,
        call_event_id: &loon_events::EvId,
        call: &loon_providers::ToolCallReq,
    ) -> ToolRunOutcome {
        let spec = match self.tools.get(&call.name) {
            Some(t) => t,
            None => {
                let msg = format!(
                    "unknown tool `{}` (available: {:?})",
                    call.name,
                    self.tools
                        .specs()
                        .iter()
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>()
                );
                let _ = self.journal.append(
                    Some(*call_event_id),
                    EventKind::ToolCallRejected {
                        request: *call_event_id,
                        name: call.name.clone(),
                        reason: msg.clone(),
                    },
                );
                self.emit_async(AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    ok: false,
                    excerpt: msg.clone(),
                })
                .await;
                return ToolRunOutcome {
                    model_text: format!("ERROR: {msg}"),
                    ok: false,
                };
            }
        };
        let args: serde_json::Value = match parse_tool_arguments(&call.arguments_json) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("arguments are not valid JSON: {e}");
                let _ = self.journal.append(
                    Some(*call_event_id),
                    EventKind::ToolCallRejected {
                        request: *call_event_id,
                        name: call.name.clone(),
                        reason: msg.clone(),
                    },
                );
                let _ = self
                    .emit_async(AgentEvent::ToolFinished {
                        name: call.name.clone(),
                        ok: false,
                        excerpt: msg.clone(),
                    })
                    .await;
                return ToolRunOutcome {
                    model_text: tool_argument_feedback(
                        &spec.spec(),
                        &msg,
                        Some(&call.arguments_json),
                    ),
                    ok: false,
                };
            }
        };
        let args = loon_tools::tool::normalize_args(&spec.spec(), args);
        if let Err(e) = validate_args(&spec.spec(), &args) {
            let reason = e.to_string();
            let _ = self.journal.append(
                Some(*call_event_id),
                EventKind::ToolCallRejected {
                    request: *call_event_id,
                    name: call.name.clone(),
                    reason: reason.clone(),
                },
            );
            // The transcript shows the same explanation the model receives, so a bad
            // call is visible instead of only failing somewhere in the journal.
            let _ = self
                .emit_async(AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    ok: false,
                    excerpt: reason.clone(),
                })
                .await;
            let raw = serde_json::to_string(&args).unwrap_or_default();
            let repeats = self
                .rejected_calls
                .lock()
                .expect("rejection lock")
                .get(&format!("{}\u{1}{}", call.name, args))
                .copied()
                .unwrap_or(0)
                + 1;
            let mut feedback = tool_argument_feedback(&spec.spec(), &reason, Some(&raw));
            if repeats >= 2 {
                feedback.push_str(&format!(
                    "\n\nThis exact call has now been rejected {repeats} times. \
                     Do not send it again. Either send a corrected call that satisfies the \
                     parameters above, or stop and tell the user what you cannot do."
                ));
            }
            return ToolRunOutcome {
                model_text: feedback,
                ok: false,
            };
        }

        if self.worklog.read().map(|s| s.plan_mode).unwrap_or(false)
            && spec.spec().effects == Effects::Mutating
            && !["update_plan", "note", "task", "todo_write"].contains(&spec.spec().name.as_str())
        {
            let reason="Plan mode blocks workspace mutations. Continue inspection/planning; only the user can leave plan mode with /plan off.".to_string();
            let _ = self.journal.append(
                Some(*call_event_id),
                EventKind::ToolCallRejected {
                    request: *call_event_id,
                    name: call.name.clone(),
                    reason: reason.clone(),
                },
            );
            self.emit_async(AgentEvent::ToolFinished {
                name: call.name.clone(),
                ok: false,
                excerpt: reason.clone(),
            })
            .await;
            return ToolRunOutcome {
                ok: false,
                model_text: reason,
            };
        }

        // ── Permission gate: policy decides before the tool ever runs. ──────────────
        let args_summary: String = serde_json::to_string(&args).unwrap_or_default();
        // Observation-only actions are not mutations: polling a monitor or listing
        // sessions/connections must not require approval in ask mode.
        let observation_only = match spec.spec().name.as_str() {
            "process" | "monitor" => ["poll", "list", "logs", "wait_for"]
                .contains(&args["action"].as_str().unwrap_or("")),
            "shell_session" => ["list", "cwd"].contains(&args["action"].as_str().unwrap_or("")),
            "ssh" => args["action"].as_str() == Some("list"),
            _ => false,
        };
        let mut decision = self.permissions.evaluate_call(
            &spec.spec().name,
            &args,
            self.ctx.ws.root(),
            spec.spec().effects == Effects::Mutating
                && !["todo_write", "update_plan", "note", "task"]
                    .contains(&spec.spec().name.as_str())
                && !observation_only,
        );
        if matches!(decision, permissions::Decision::ApprovalRequired) {
            // Interactive UIs answer through the hook; headless runs cannot approve.
            // Auto mode consults the reviewer agent first.
            let approved = if self.cfg.permissions.mode == loon_config::PermissionMode::Auto {
                self.auto_review_then_ask(&call.name, &args).await
            } else {
                self.request_approval(&call.name, &args).await
            };
            decision = if approved {
                permissions::Decision::Allowed
            } else {
                permissions::Decision::ApprovalRequired
            };
        }
        self.permissions.audit(
            &self.session_id.to_string(),
            &call.name,
            &args_summary,
            &decision,
        );
        if !decision.is_allowed() {
            let reason = decision_refusal_reason(&decision);
            let _ = self.journal.append(
                Some(*call_event_id),
                EventKind::ToolCallRejected {
                    request: *call_event_id,
                    name: call.name.clone(),
                    reason: reason.clone(),
                },
            );
            self.emit_async(AgentEvent::Warning(reason)).await;
            return ToolRunOutcome {
                model_text: decision.refusal_text(),
                ok: false,
            };
        }

        let pre_hook = self
            .run_hooks(
                "PreToolUse",
                serde_json::json!({"tool_name":spec.spec().name,"tool_input":args}),
            )
            .await;
        if pre_hook.blocked {
            let reason = format!("Tool blocked by PreToolUse hook: {}", pre_hook.context);
            self.emit_async(AgentEvent::ToolFinished {
                name: call.name.clone(),
                ok: false,
                excerpt: reason.clone(),
            })
            .await;
            let _ = self.journal.append(
                Some(*call_event_id),
                EventKind::ToolCallRejected {
                    request: *call_event_id,
                    name: call.name.clone(),
                    reason: reason.clone(),
                },
            );
            return ToolRunOutcome {
                model_text: reason,
                ok: false,
            };
        }
        let args_excerpt: String = {
            let s = serde_json::to_string(&args).unwrap_or_default();
            s.chars().take(200).collect()
        };
        let _ = self.journal.append(
            Some(*call_event_id),
            EventKind::ToolExecutionStarted {
                call: *call_event_id,
                name: call.name.clone(),
            },
        );
        self.emit_async(AgentEvent::ToolStarted {
            name: call.name.clone(),
            args_excerpt: args_excerpt.clone(),
            detail: tool_detail(&spec.spec().name, &args),
        })
        .await;
        let started = Instant::now();
        let result = spec.run(args.clone(), &self.ctx).await;
        let success = result.as_ref().map(|out| out.ok).unwrap_or(false);
        let output = result
            .as_ref()
            .map(|out| out.text.clone())
            .unwrap_or_else(|e| e.to_string());
        let post_hook = self.run_hooks(if success {"PostToolUse"} else {"PostToolUseFailure"}, serde_json::json!({"tool_name":spec.spec().name,"tool_input":args,"tool_response":output})).await;
        let guidance = crate::guidance::for_tool(self.ctx.ws.root(), &args);
        let extra_context = format!("{}{}{}", pre_hook.context, guidance, post_hook.context);
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(mut out) => {
                let blob = out
                    .full
                    .as_ref()
                    .and_then(|bytes| self.journal.put_blob(bytes).ok());
                if let Some(blob) = &blob {
                    out.text.push_str(&format!(
                        "\n[Full output sha256 {}; use read_output to inspect or search it.]",
                        blob.sha256
                    ));
                }
                self.emit_async(AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    ok: out.ok,
                    excerpt: out.text.clone(),
                })
                .await;
                let _ = self.journal.append(
                    Some(*call_event_id),
                    EventKind::ToolExecutionFinished {
                        call: *call_event_id,
                        name: call.name.clone(),
                        ok: out.ok,
                        excerpt: out.text.clone(),
                        blob,
                        duration_ms,
                    },
                );
                // Journal a FileMutated record when a mutating tool reported a content
                // change (structured carries before/after hashes and snapshot blobs).
                if out.ok {
                    if let Some(obj) = out.structured.as_object() {
                        let has_change = obj.contains_key("before_sha256");
                        if has_change {
                            let get =
                                |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
                            let blob_sha = |k: &str| {
                                obj.get(k).and_then(|v| v.as_str()).map(|sha| {
                                    loon_events::BlobRef {
                                        sha256: sha.to_string(),
                                        bytes: 0, // size resolved from the store on read
                                    }
                                })
                            };
                            let _ = self.journal.append(
                                Some(*call_event_id),
                                EventKind::FileMutated {
                                    tx: *call_event_id,
                                    call: *call_event_id,
                                    path: get("path").unwrap_or_default(),
                                    tool: get("tool").unwrap_or_else(|| call.name.clone()),
                                    before_sha256: get("before_sha256").unwrap_or_default(),
                                    after_sha256: get("after_sha256").unwrap_or_default(),
                                    before_blob: blob_sha("before_blob_sha256"),
                                    after_blob: blob_sha("after_blob_sha256"),
                                },
                            );
                        }
                    }
                }
                if !extra_context.is_empty() {
                    out.text
                        .push_str(&format!("\n[Local guidance and hooks]\n{extra_context}"));
                }
                if out.ok && spec.spec().name == "load_skill" {
                    if let Some(name) = args["name"].as_str() {
                        let _ = self.worklog.update(|state| {
                            if !state.active_skills.contains(&name.to_string()) {
                                state.active_skills.push(name.to_string());
                            }
                            Ok(())
                        });
                    }
                }
                ToolRunOutcome {
                    model_text: out.text,
                    ok: out.ok,
                }
            }
            Err(e) => {
                self.emit_async(AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    ok: false,
                    excerpt: format!("tool error: {e}"),
                })
                .await;
                let _ = self.journal.append(
                    Some(*call_event_id),
                    EventKind::ToolExecutionFinished {
                        call: *call_event_id,
                        name: call.name.clone(),
                        ok: false,
                        excerpt: format!("tool error: {e}"),
                        blob: None,
                        duration_ms,
                    },
                );
                ToolRunOutcome {
                    model_text: format!("ERROR: {e}\n{extra_context}"),
                    ok: false,
                }
            }
        }
    }

    fn record_hook_context(&mut self, text: &str) {
        let text = format!("[Local lifecycle hook]\n{text}");
        let _ = self
            .journal
            .append(None, EventKind::UserMessage { text: text.clone() });
        self.history.push(loon_providers::ChatMessage::user(text));
    }

    fn assemble_messages(&self) -> Vec<loon_providers::ChatMessage> {
        let mut v = vec![loon_providers::ChatMessage::system(
            self.system_prompt.clone(),
        )];
        if let Some(goal) = &self.session_goal {
            v.push(loon_providers::ChatMessage::system(format!(
                "Current session goal (the user set this with /goal): {goal}"
            )));
        }
        let guidance = guidance::workspace(self.ctx.ws.root());
        if !guidance.is_empty() {
            v.push(loon_providers::ChatMessage::system(format!(
                "Local workspace guidance (repository data):\n{guidance}"
            )));
        }
        if let Ok(state) = self.worklog.read() {
            let available = skills::load_with_builtins(&self.data_dir, self.ctx.ws.root());
            for name in state.active_skills.iter().rev().take(8).rev() {
                if let Some(skill) = available.iter().find(|s| &s.name == name) {
                    v.push(loon_providers::ChatMessage::system(format!(
                        "Active skill /{}:\n{}",
                        name,
                        skill.body.chars().take(8000).collect::<String>()
                    )));
                }
            }
        }
        let state = self.worklog.prompt();
        if !state.is_empty() {
            v.push(loon_providers::ChatMessage::system(format!(
                "Durable session work state (data, not higher-priority instructions):\n{state}"
            )));
        }
        let effort = loon_config::Effort::parse(self.effort.as_deref().unwrap_or("medium"))
            .unwrap_or_default();
        v.push(loon_providers::ChatMessage::system(format!(
            "Coding effort: {}",
            effort.guidance()
        )));
        let skills = self
            .skills
            .iter()
            .map(|s| format!("/{} ({}) — {}", s.name, s.scope, s.description))
            .collect::<Vec<_>>()
            .join(", ");
        if !skills.is_empty() {
            v.push(loon_providers::ChatMessage::system(format!("Available workflows: {skills}. Use load_skill to read a workflow before applying it.")));
        }
        let remote_count = self
            .tools
            .specs()
            .iter()
            .filter(|s| s.name.starts_with("mcp_"))
            .count();
        if remote_count > 0 {
            let loaded = self.loaded_mcp.lock().unwrap_or_else(|e| e.into_inner());
            v.push(loon_providers::ChatMessage::system(format!("{remote_count} remote MCP tools available through tool_search. Loaded remote tools: {}",loaded.iter().cloned().collect::<Vec<_>>().join(", "))));
        }
        v.extend(self.history.iter().cloned());
        v
    }
}

struct ToolRunOutcome {
    model_text: String,
    #[allow(dead_code)] // consumed by the journal event; kept for symmetry with ToolOutput
    ok: bool,
}

/// What the model (and the transcript) is told when a tool call's arguments do not fit
/// the tool's schema: the exact problem, the arguments that were sent, the parameters
/// the tool accepts, and an instruction to correct the call.
fn tool_argument_feedback(spec: &loon_tools::ToolSpec, problem: &str, raw: Option<&str>) -> String {
    // `ToolError::InvalidArgs` already prefixes its own explanation.
    let problem = problem
        .strip_prefix("invalid arguments: ")
        .unwrap_or(problem);
    let mut out = format!("ERROR: `{}` call rejected: {problem}.\n", spec.name);
    if let Some(raw) = raw {
        out.push_str(&format!("Arguments received: {}\n", excerpt_text(raw, 300)));
    }
    let params = &spec.parameters;
    let required: Vec<&str> = params
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if let Some(props) = params.get("properties").and_then(|p| p.as_object()) {
        out.push_str("Parameters:\n");
        for (name, schema) in props {
            let ty = schema.get("type").and_then(|t| t.as_str()).unwrap_or("any");
            let req = if required.contains(&name.as_str()) {
                "required"
            } else {
                "optional"
            };
            let desc = schema
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            out.push_str(&format!("  - {name} ({ty}, {req})"));
            if !desc.is_empty() {
                out.push_str(&format!(": {}", excerpt_text(desc, 160)));
            }
            out.push('\n');
        }
    }
    out.push_str(
        "Call the tool again with every required field present. Do not repeat the same call unchanged.",
    );
    out
}

/// One-line, length-bounded rendering for error text.
fn excerpt_text(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max).collect::<String>() + "…"
    }
}

impl Agent {
    /// Run configured verification through the normal permission/tool path.
    /// Repair budget exhaustion is a failure, never a successful completion.
    async fn ultra_loop_verdict(&mut self, rounds: &mut u32) -> Result<bool, String> {
        let Some(command) = self
            .cfg
            .check_command
            .clone()
            .filter(|s| !s.trim().is_empty())
        else {
            return Ok(false);
        };
        if self.cfg.ultra_rounds == 0 {
            return Ok(false);
        }
        self.emit_async(AgentEvent::MemberNote(format!(
            "verification · `{command}` (repair {}/{})",
            *rounds, self.cfg.ultra_rounds
        )))
        .await;
        let observed = loon_events::EvId::now_v7();
        let call = loon_providers::ToolCallReq {
            id: format!("verify_{observed}"),
            name: "bash".into(),
            arguments_json: serde_json::json!({"command":command,"timeout_secs":600}).to_string(),
        };
        let _ = self.journal.append(
            None,
            EventKind::ToolCallObserved {
                request: observed,
                call: observed,
                wire_id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            },
        );
        self.history.push(loon_providers::ChatMessage {
            role: loon_providers::Role::Assistant,
            text: String::new(),
            tool_calls: vec![call.clone()],
            tool_call_id: None,
        });
        let result = self.execute_tool(&observed, &call).await;
        self.history.push(loon_providers::ChatMessage::tool_result(
            call.id,
            result.model_text.clone(),
        ));
        if result.ok {
            self.emit_async(AgentEvent::MemberNote(format!(
                "verification · `{command}` passed"
            )))
            .await;
            return Ok(false);
        }
        if *rounds >= self.cfg.ultra_rounds {
            return Err(format!(
                "verification `{command}` still fails after {} repair rounds\n{}",
                *rounds, result.model_text
            ));
        }
        *rounds += 1;
        let prompt = format!("[ultra loop] Required verification `{command}` did not pass.\n{}\nRead the error, fix the cause or use a different approach, and verify again. Do not claim completion or bypass permission denials.", result.model_text);
        let _ = self.journal.append(
            None,
            EventKind::UserMessage {
                text: prompt.clone(),
            },
        );
        self.history.push(loon_providers::ChatMessage::user(prompt));
        Ok(true)
    }

    fn repair_interrupted_calls(&mut self) {
        let mut missing = Vec::new();
        for (i, message) in self.history.iter().enumerate() {
            for call in &message.tool_calls {
                let found = self.history[i + 1..]
                    .iter()
                    .take_while(|m| m.role == loon_providers::Role::Tool)
                    .any(|m| m.tool_call_id.as_deref() == Some(&call.id));
                if !found {
                    missing.push((i, call.id.clone()));
                }
            }
        }
        for (index, id) in missing.into_iter().rev() {
            self.history.insert(index + 1, loon_providers::ChatMessage::tool_result(id,
                "[Interrupted before a result was recorded. Execution outcome unknown; inspect workspace state before retrying a mutation.]"));
        }
    }
}

/// How many times one identical invalid call may be rejected before the turn stops.
const MAX_IDENTICAL_REJECTIONS: u32 = 4;

/// One line describing what a tool call is doing, for the transcript header:
/// `$ cargo test` for shell, the path for file tools, the pattern for searches.
fn tool_detail(name: &str, args: &serde_json::Value) -> String {
    let s = |k: &str| {
        args.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    };
    match name {
        "bash" => format!("$ {}", s("command")),
        "read" | "write" | "edit" | "fuzzy_edit" | "multi_edit" | "read_json" | "json_edit"
        | "notebook_read" | "notebook_edit" | "file_info" | "list_directory" | "project_info" => {
            s("path").to_string()
        }
        "grep" => {
            let pattern = s("pattern");
            let path = s("path");
            if path.is_empty() {
                pattern.to_string()
            } else {
                format!("{pattern} in {path}")
            }
        }
        "glob" => s("pattern").to_string(),
        "tree" => {
            let path = s("path");
            if path.is_empty() {
                ".".into()
            } else {
                path.to_string()
            }
        }
        "delegate" => s("task").to_string(),
        "context7_search" | "context7_resolve_library" | "context7_query_docs" => {
            format!("Context7 · {}", s("query"))
        }
        "find_symbol" => format!("{} in {}", s("symbol"), s("path")),
        "run_tests" | "run_build" | "run_lint" => format!(
            "{name} · {}",
            if s("path").is_empty() { "." } else { s("path") }
        ),
        n if n.starts_with("git_") => format!("{} {} {}", n, s("revision"), s("path"))
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// Parse a tool call's arguments, unwrapping the double-encoded form some gateways
/// produce (`"{\"path\":\"a\"}"` — a JSON string containing the object).
fn parse_tool_arguments(raw: &str) -> Result<serde_json::Value, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    if let serde_json::Value::String(inner) = &value {
        if let Ok(nested) = serde_json::from_str::<serde_json::Value>(inner) {
            if nested.is_object() {
                return Ok(nested);
            }
        }
    }
    Ok(value)
}

async fn cancel_changed(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn chrono_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Agent {
    /// Ask the attached UI (or headless caller) to approve one operation in ask mode.
    /// With no hook installed the answer is always no — approval is never implied.
    /// Auto mode: the reviewer consults first; ALLOW passes, DENY/no-verdict forwards
    /// to the user hook (which refuses in headless runs). Never bypasses deny rules —
    /// those were enforced before this runs.
    async fn auto_review_then_ask(&self, tool: &str, args: &serde_json::Value) -> bool {
        match self.reviewer_verdict(tool, args).await {
            Some(true) => {
                self.emit_async(AgentEvent::MemberNote(format!(
                    "auto review: {tool} allowed by reviewer"
                )))
                .await;
                true
            }
            verdict => {
                self.emit_async(AgentEvent::MemberNote(format!(
                    "auto review: {tool} {}— asking you",
                    match verdict {
                        Some(false) => "DENIED ",
                        _ => "unclear ",
                    }
                )))
                .await;
                self.request_approval(tool, args).await
            }
        }
    }

    async fn request_approval(&self, tool: &str, args: &serde_json::Value) -> bool {
        let args_excerpt: String = serde_json::to_string(args)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        self.emit_async(AgentEvent::ApprovalRequested {
            tool: tool.to_string(),
            args_excerpt: args_excerpt.clone(),
        })
        .await;
        match &self.approval_hook {
            Some(hook) => {
                let req = permissions::ApprovalRequest {
                    tool: tool.to_string(),
                    args_excerpt,
                    reason: "permission mode is ask".into(),
                };
                hook(req).await
            }
            None => false,
        }
    }
}

/// Short journal/audit-facing reason for a refusal (the model-facing text lives in
/// [`permissions::Decision::refusal_text`]).
fn decision_refusal_reason(d: &permissions::Decision) -> String {
    match d {
        permissions::Decision::Denied { rule } => format!("denied by configured rule `{rule}`"),
        permissions::Decision::NotAllowlisted => {
            "permission mode is allowlist and no rule allows this operation".into()
        }
        permissions::Decision::ApprovalRequired => {
            "permission mode is ask and the operation was not approved".into()
        }
        permissions::Decision::Allowed => "allowed".into(),
    }
}

#[cfg(test)]
mod auto_mode_tests {
    use super::*;
    use loon_config::PermissionMode;

    #[test]
    fn reviewer_verdict_parses_allow_deny_and_garbage() {
        assert_eq!(Agent::parse_reviewer_verdict("ALLOW"), Some(true));
        assert_eq!(Agent::parse_reviewer_verdict("deny"), Some(false));
        assert_eq!(
            Agent::parse_reviewer_verdict("The operation is low risk, ALLOW"),
            Some(true)
        );
        assert_eq!(Agent::parse_reviewer_verdict("maybe"), None);
        assert_eq!(Agent::parse_reviewer_verdict(""), None);
        assert_eq!(
            Agent::parse_reviewer_verdict("ALLOW DENY"),
            None,
            "contradictory"
        );
    }

    #[test]
    fn shift_tab_cycle_bypass_auto_ask() {
        assert_eq!(
            Agent::next_permission_mode(PermissionMode::Bypass),
            PermissionMode::Auto
        );
        assert_eq!(
            Agent::next_permission_mode(PermissionMode::Auto),
            PermissionMode::Ask
        );
        // allowlist is out of the interactive cycle (config-only), back to bypass
        assert_eq!(
            Agent::next_permission_mode(PermissionMode::Ask),
            PermissionMode::Bypass
        );
        assert_eq!(
            Agent::next_permission_mode(PermissionMode::Allowlist),
            PermissionMode::Bypass
        );
    }

    #[test]
    fn config_parses_auto_mode() {
        // serde round trip through the config enum (what config.toml parsing uses)
        let cfg: loon_config::Permissions = serde_json::from_str(r#"{"mode": "auto"}"#).unwrap();
        assert_eq!(cfg.mode, PermissionMode::Auto);
    }
}
