//! Loon agent core: the understand → act → observe → verify loop.
//!
//! Owns the conversation history, the tool scheduler, budgets and cancellation, and
//! journals every state change. The TUI and headless clients are consumers of
//! [`AgentEvent`]s; they hold no business logic.

pub mod commands;
pub mod compaction;
pub mod est;
pub mod index;
pub mod loops;
pub mod mcp;
pub mod memory;
pub mod permissions;
pub mod prompt;
pub mod replay;
pub mod rewind;
pub mod skills;
pub mod subagent;
pub mod verify;

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
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted {
        name: String,
        args_excerpt: String,
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
    /// bare /config — the UI opens its interactive settings menu.
    OpenConfigMenu,
    /// bare /model — the UI opens the model picker over these refs.
    OpenModelPicker {
        refs: Vec<String>,
    },
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
    /// Step/tool budget exhausted; state is consistent and resumable.
    BudgetPaused {
        steps_used: u32,
    },
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("model_ref", &self.model_ref)
            .field("max_steps", &self.max_steps)
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
            TurnStatus::BudgetPaused { .. } => "budget paused",
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
    pub max_steps: u32,
    pub tool_output_bytes: usize,
    pub max_retries: u32,
    pub allow_retry_after_partial_stream: bool,
    /// Permission policy (mode + allow/deny rules) resolved from config and CLI overrides.
    pub permissions: loon_config::Permissions,
    /// Effective context window in tokens (from the model def / combo minimum). 0
    /// disables compaction. Unknown windows fall back to a conservative 128k default.
    pub context_window: u64,
    /// Host-provided extra tools (e.g. MCP), merged into the registry at construction.
    pub extra_tools: Vec<Arc<dyn loon_tools::Tool>>,
    /// Ultra Loop: the project's check command run automatically after edit batches
    /// (e.g. "cargo test"). None disables the automatic verification cycle.
    pub check_command: Option<String>,
}

impl From<&loon_config::SessionCfg> for AgentConfig {
    fn from(s: &loon_config::SessionCfg) -> AgentConfig {
        AgentConfig {
            model_ref: s.model.clone(),
            max_steps: s.max_steps,
            tool_output_bytes: s.tool_output_bytes,
            max_retries: s.max_retries,
            allow_retry_after_partial_stream: s.allow_retry_after_partial_stream,
            permissions: loon_config::Permissions::default(),
            context_window: 128_000,
            extra_tools: Vec::new(),
            check_command: None,
        }
    }
}

pub struct Agent {
    pub journal: Arc<Journal>,
    pub registry: Arc<ProviderRegistry>,
    pub tools: ToolRegistry,
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
    /// Loon data dir (for memory and friends). Set at construction.
    pub data_dir: PathBuf,
    /// Loon config dir (for /config persistence).
    pub config_dir: PathBuf,
    /// User-defined skills (custom slash commands), loaded at construction.
    pub skills: Vec<skills::Skill>,
    /// Loop tasks (recurring prompts). The host scheduler polls `take_due`; the agent
    /// cancels through the same registry via the loop_cancel tool.
    pub loops: Arc<loops::LoopRegistry>,
    /// /effort: requested output budget per request (max_output_tokens).
    pub effort: Option<String>,
    /// /goal: session goal, injected into every request as a leading system message.
    pub session_goal: Option<String>,
    /// Ultra Loop baseline check result (established on the first verify).
    baseline: Option<crate::verify::CheckResult>,
    system_prompt: String,
    session_id: loon_events::EvId,
}

impl Agent {
    /// Attach a live event sink (TUI or headless JSONL writer).
    pub fn set_event_sink(&mut self, tx: mpsc::Sender<AgentEvent>) {
        self.events = Some(tx);
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
    ToolRegistry::standard().with_extra(vec![Arc::new(memory::MemoryTool {
        paths: Arc::new(memory::MemoryPaths::resolve(&paths.data_dir, cwd)),
    })])
}

impl Agent {
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
        let meta = SessionMeta {
            id: session_id,
            title: "session".into(),
            created_ms: chrono_ms(),
            cwd: cwd.display().to_string(),
            schema_version: loon_events::SCHEMA_VERSION,
            headless,
        };
        let store = open_store(paths)?;
        let journal = Arc::new(Journal::create_default(&store, &meta)?);
        // (blob_sink attached below, after ctx is built)
        let system = prompt::system_prompt(&cwd);
        journal.append(
            None,
            EventKind::SessionStarted {
                cwd: cwd.display().to_string(),
                model: cfg.model_ref.clone(),
                headless,
            },
        )?;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (steering_tx, steering_rx) = mpsc::channel(64);
        let extra_roots: Vec<PathBuf> = cfg
            .permissions
            .extra_write_roots
            .iter()
            .map(PathBuf::from)
            .collect();
        let ctx = ToolCtx {
            ws: Arc::new(
                loon_workspace::Workspace::new(&cwd, extra_roots).map_err(|e| {
                    loon_events::EventsError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        e.to_string(),
                    ))
                })?,
            ),
            output_limit: cfg.tool_output_bytes,
            default_timeout: std::time::Duration::from_secs(120),
            child_env: Default::default(),
            cancel: cancel_rx.clone(),
            blob_sink: None,
        };
        let mut ctx = ctx;
        {
            let j = journal.clone();
            ctx.blob_sink = Some(Arc::new(move |data: &[u8]| j.put_blob(data).ok()));
        }
        let audit_path = paths.state_dir.join("audit.jsonl");
        Ok(Agent {
            journal,
            registry,
            tools,
            ctx,
            history: Vec::new(),
            cancel_tx,
            cancel_rx,
            steering_rx,
            steering_tx,
            events: None,
            permissions: Arc::new(permissions::PermissionEngine::new(
                &cfg.permissions,
                Some(audit_path),
            )),
            approval_hook: None,
            summarizer: None,
            data_dir: paths.data_dir.clone(),
            config_dir: paths.config_dir.clone(),
            skills: skills::load(&paths.data_dir, Path::new(&cwd)),
            loops: Arc::new(loops::LoopRegistry::new()),
            effort: None,
            session_goal: None,
            baseline: None,
            cfg,
            system_prompt: system,
            session_id,
        })
    }

    pub fn session_id(&self) -> loon_events::EvId {
        self.session_id
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
            None => compaction::model_summarizer(self.registry.clone(), self.cfg.model_ref.clone()),
        };
        let answer = summarizer(format!(
            "You are a safety reviewer for a coding agent. Judge whether allowing this \
             operation is low-risk for the user's machine and repository (writes inside \
             the workspace, normal build/test commands: low risk; deleting outside the \
             workspace, exfiltration, destructive shell, credential access: high risk). \
             Answer with exactly one word: ALLOW or DENY.\n\n{transcript}"
        ))
        .await;
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

    /// Queue a steering message; consumed at the next safe boundary.
    pub fn steer(&self, text: String) {
        let _ = self.steering_tx.try_send(text);
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
        // The newest Rewound mark truncates the replay (see rewind.rs).
        let mut truncate_at: Option<u64> = None;
        for ev in &events {
            match &ev.kind {
                EventKind::SessionStarted { cwd: c, .. } => cwd = c.clone(),
                EventKind::Rewound {
                    truncate_to_seq, ..
                } => truncate_at = Some(*truncate_to_seq),
                _ => {}
            }
        }
        let history = replay::replay_history(&events, truncate_at);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (steering_tx, steering_rx) = mpsc::channel(64);
        let extra_roots: Vec<PathBuf> = cfg
            .permissions
            .extra_write_roots
            .iter()
            .map(PathBuf::from)
            .collect();
        let ctx = ToolCtx {
            ws: Arc::new(
                loon_workspace::Workspace::new(PathBuf::from(&cwd), extra_roots).map_err(|e| {
                    loon_events::EventsError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        e.to_string(),
                    ))
                })?,
            ),
            output_limit: cfg.tool_output_bytes,
            default_timeout: std::time::Duration::from_secs(120),
            child_env: Default::default(),
            cancel: cancel_rx.clone(),
            blob_sink: None,
        };
        let journal = Arc::new(journal);
        let mut ctx = ctx;
        {
            let j = journal.clone();
            ctx.blob_sink = Some(Arc::new(move |data: &[u8]| j.put_blob(data).ok()));
        }
        let audit_path = paths.state_dir.join("audit.jsonl");
        Ok(Agent {
            journal,
            registry,
            tools,
            ctx,
            history,
            cancel_tx,
            cancel_rx,
            steering_rx,
            steering_tx,
            events: None,
            permissions: Arc::new(permissions::PermissionEngine::new(
                &cfg.permissions,
                Some(audit_path),
            )),
            approval_hook: None,
            summarizer: None,
            data_dir: paths.data_dir.clone(),
            config_dir: paths.config_dir.clone(),
            skills: skills::load(&paths.data_dir, Path::new(&cwd)),
            loops: Arc::new(loops::LoopRegistry::new()),
            effort: None,
            session_goal: None,
            baseline: None,
            cfg,
            system_prompt: prompt::system_prompt(Path::new(&cwd)),
            session_id,
        })
    }

    /// Run one user turn to completion, journaling everything.
    pub async fn run_turn(&mut self, user_text: &str) -> TurnResult {
        self.journal
            .append(
                None,
                EventKind::UserMessage {
                    text: user_text.to_string(),
                },
            )
            .expect("journal append");
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
        let turn_id = loon_events::EvId::now_v7();
        let _ = self
            .journal
            .append(None, EventKind::TurnStarted { turn: turn_id });
        let mut steps: u32 = 0;
        let mut total_usage = Usage::default();
        let mut assistant_text = String::new();
        let mut status = TurnStatus::Completed;

        loop {
            if self.is_cancelled() {
                status = TurnStatus::Cancelled;
                break;
            }
            if steps >= self.cfg.max_steps {
                status = TurnStatus::BudgetPaused { steps_used: steps };
                let _ = self.journal.append(
                    None,
                    EventKind::Warning {
                        message: format!(
                            "step budget ({}) reached; pausing with a resumable checkpoint",
                            self.cfg.max_steps
                        ),
                    },
                );
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

            // ── one provider request (combo-aware) ────────────────────────────────
            // Context guard first: summarize older exchanges if the next request would
            // cross the compaction threshold.
            self.maybe_compact().await;
            let request_id = loon_events::EvId::now_v7();
            let max_output_tokens = match self.effort.as_deref() {
                Some("low") => Some(2_048),
                Some("high") => Some(32_768),
                _ => None,
            };
            let req = loon_providers::ChatRequest {
                model: self.cfg.model_ref.clone(),
                messages: self.assemble_messages(),
                tools: self.tools.provider_definitions(),
                max_output_tokens,
                temperature: None,
            };
            let est_tokens = est::estimate_messages(&req.messages);
            let (tx, mut rx) = mpsc::channel::<AttemptEvent>(512);
            let registry = self.registry.clone();
            let model_ref = self.cfg.model_ref.clone();
            let allow_partial = self.cfg.allow_retry_after_partial_stream;
            let mut runner = ComboRunner::new(registry);
            runner.max_retries = self.cfg.max_retries;
            runner.allow_retry_after_partial_stream = allow_partial;
            let req_for_task = req.clone();
            let handle =
                tokio::spawn(async move { runner.run(&model_ref, &req_for_task, tx).await });

            let mut step_text = String::new();
            let mut mutated_this_step = false;
            // Ultra Loop step 1: the baseline is taken BEFORE any edit of this turn —
            // a red baseline means pre-existing failures, not the agent's doing.
            if self.baseline.is_none() {
                if let Some(cmd) = self.cfg.check_command.clone() {
                    let runner = crate::verify::shell_runner(
                        self.ctx.ws.root().to_path_buf(),
                        std::time::Duration::from_secs(600),
                    );
                    let mut verifier = crate::verify::Verifier::new(cmd, runner);
                    let b = verifier.establish_baseline().await;
                    self.emit(AgentEvent::MemberNote(format!(
                        "ultra loop baseline: {}",
                        if b.ok {
                            "green".into()
                        } else {
                            format!("RED — {} ", b.output_excerpt.lines().last().unwrap_or(""))
                        }
                    )));
                    self.baseline = Some(b);
                }
            }
            let mut tool_calls = Vec::new();
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
                                self.emit(AgentEvent::TextDelta(text));
                            }
                            ProviderEvent::ReasoningDelta { text } => {
                                let _ = self.journal.append(Some(request_id), EventKind::ReasoningDelta { request: request_id, text });
                            }
                            ProviderEvent::ToolCallComplete { call } => {
                                let _ = self.journal.append(
                                    Some(request_id),
                                    EventKind::ToolCallObserved {
                                        request: request_id,
                                        call: loon_events::EvId::now_v7(),
                                        wire_id: call.id.clone(),
                                        name: call.name.clone(),
                                        arguments_json: call.arguments_json.clone(),
                                    },
                                );
                                tool_calls.push(call);
                            }
                            ProviderEvent::Usage(u) => {
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
                            ProviderEvent::ToolCallDelta { .. } | ProviderEvent::Finished { .. } => {}
                        }
                    }
                    _ = cancel_changed(&mut self.cancel_rx) => {
                        self.cancel();
                    }
                }
            }
            let combo_outcome: Option<Result<ComboOutcome, String>> = match handle.await {
                Ok(r) => Some(r),
                Err(e) => Some(Err(format!("request task panicked: {e}"))),
            };

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
                        self.emit(AgentEvent::MemberNote(format!("skipped {m}: {reason}")));
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
                        self.emit(AgentEvent::Warning(format!("request failed: {error}")));
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

            if !step_text.is_empty() {
                assistant_text.push_str(&step_text);
                self.history
                    .insert_tool_aware(loon_providers::ChatMessage::assistant(std::mem::take(
                        &mut step_text,
                    )));
            }

            // ── tool execution phase ──────────────────────────────────────────────
            if tool_calls.is_empty() {
                break; // conversational finish
            }
            let assistant_with_calls = loon_providers::ChatMessage {
                role: loon_providers::Role::Assistant,
                text: String::new(),
                tool_calls: tool_calls.clone(),
                tool_call_id: None,
            };
            self.history.insert_tool_aware(assistant_with_calls);

            for call in tool_calls {
                if self.is_cancelled() {
                    status = TurnStatus::Cancelled;
                    break;
                }
                let call_id = loon_events::EvId::now_v7();
                let result = self.execute_tool(&call_id, &call).await;
                if result.mutated {
                    mutated_this_step = true;
                }
                self.history.push(loon_providers::ChatMessage::tool_result(
                    call.id.clone(),
                    result.model_text.clone(),
                ));
            }
            // ── Ultra Loop: verify after every edit batch ────────────────────────
            if mutated_this_step {
                if let Some(cmd) = self.cfg.check_command.clone() {
                    let cwd = self.ctx.ws.root().to_path_buf();
                    let runner =
                        crate::verify::shell_runner(cwd, std::time::Duration::from_secs(600));
                    let mut verifier = crate::verify::Verifier::new(cmd, runner);
                    let (check, class) = verifier.verify().await;
                    let verdict = format!("ultra loop: {} → {}", check.ok, class.describe());
                    let _ = self.journal.append(
                        None,
                        EventKind::Warning {
                            message: verdict.clone(),
                        },
                    );
                    self.emit(AgentEvent::MemberNote(verdict));
                    if class == crate::verify::Classification::Introduced {
                        self.history.push(loon_providers::ChatMessage::user(format!(
                            "[ultra loop] your last edit batch broke the check command (`{}`). \
                             Re-read the failure, fix it, and verify again before continuing.",
                            self.cfg.check_command.clone().unwrap_or_default()
                        )));
                    }
                }
            }
            if status == TurnStatus::Cancelled {
                break;
            }
        }

        let _ = self.journal.append(
            None,
            EventKind::TurnFinished {
                turn: turn_id,
                reason: format!("{status:?}"),
            },
        );
        self.emit(AgentEvent::TurnFinished {
            status: status.clone(),
        });
        TurnResult {
            status,
            assistant_text,
            steps,
            usage: total_usage,
        }
    }

    async fn execute_tool(
        &mut self,
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
                return ToolRunOutcome {
                    model_text: format!("ERROR: {msg}"),
                    ok: false,
                    mutated: false,
                };
            }
        };
        let args: serde_json::Value = match serde_json::from_str(&call.arguments_json) {
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
                return ToolRunOutcome {
                    model_text: format!("ERROR: {msg}"),
                    ok: false,
                    mutated: false,
                };
            }
        };
        if let Err(e) = validate_args(&spec.spec(), &args) {
            let _ = self.journal.append(
                Some(*call_event_id),
                EventKind::ToolCallRejected {
                    request: *call_event_id,
                    name: call.name.clone(),
                    reason: e.to_string(),
                },
            );
            return ToolRunOutcome {
                model_text: format!("ERROR: invalid arguments: {e}"),
                ok: false,
                mutated: false,
            };
        }

        // ── Permission gate: policy decides before the tool ever runs. ──────────────
        let args_summary: String = serde_json::to_string(&args).unwrap_or_default();
        let mut decision = self.permissions.evaluate(
            &call.name,
            args.get("path").and_then(serde_json::Value::as_str),
            spec.spec().effects == Effects::Mutating,
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
            self.emit(AgentEvent::Warning(reason));
            return ToolRunOutcome {
                model_text: decision.refusal_text(),
                ok: false,
                mutated: false,
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
        self.emit(AgentEvent::ToolStarted {
            name: call.name.clone(),
            args_excerpt: args_excerpt.clone(),
        });
        let started = Instant::now();
        let result = spec.run(args, &self.ctx).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(out) => {
                self.emit(AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    ok: out.ok,
                    excerpt: out.text.clone(),
                });
                let blob = out
                    .full
                    .as_ref()
                    .and_then(|b| self.journal.put_blob(b).ok());
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
                ToolRunOutcome {
                    model_text: out.text,
                    ok: out.ok,
                    // file-mutating tools attach before/after hashes; detect via that
                    mutated: out.structured.get("after_sha256").is_some(),
                }
            }
            Err(e) => {
                self.emit(AgentEvent::ToolFinished {
                    name: call.name.clone(),
                    ok: false,
                    excerpt: format!("tool error: {e}"),
                });
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
                    model_text: format!("ERROR: {e}"),
                    ok: false,
                    mutated: false,
                }
            }
        }
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
        v.extend(self.history.iter().cloned());
        v
    }
}

struct ToolRunOutcome {
    model_text: String,
    #[allow(dead_code)] // consumed by the journal event; kept for symmetry with ToolOutput
    ok: bool,
    /// True when this call mutated files (drives the Ultra Loop verify cycle).
    mutated: bool,
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

trait InsertToolAware {
    /// Insert an assistant message *before* any trailing tool results, keeping
    /// tool-call → result adjacency intact for both wire protocols.
    fn insert_tool_aware(&mut self, msg: loon_providers::ChatMessage);
}

impl InsertToolAware for Vec<loon_providers::ChatMessage> {
    fn insert_tool_aware(&mut self, msg: loon_providers::ChatMessage) {
        if msg.role == loon_providers::Role::Assistant {
            if let Some(pos) = self
                .iter()
                .rposition(|m| m.role != loon_providers::Role::Tool)
            {
                self.insert(pos + 1, msg);
                return;
            }
        }
        self.push(msg);
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
                self.emit(AgentEvent::MemberNote(format!(
                    "auto review: {tool} allowed by reviewer"
                )));
                true
            }
            verdict => {
                self.emit(AgentEvent::MemberNote(format!(
                    "auto review: {tool} {}— asking you",
                    match verdict {
                        Some(false) => "DENIED ",
                        _ => "unclear ",
                    }
                )));
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
        self.emit(AgentEvent::ApprovalRequested {
            tool: tool.to_string(),
            args_excerpt: args_excerpt.clone(),
        });
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
