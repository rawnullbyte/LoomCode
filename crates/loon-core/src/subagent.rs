//! Subagents: `delegate` runs a scoped task in a fresh child session and blocks until
//! it answers; `background_spawn` starts the same child without blocking and hands the
//! parent a handle it can message, list, or kill.
//!
//! Boundaries (deliberate):
//! - the child never gets the `delegate` tool — no recursive delegation;
//! - the child shares the parent's provider registry (so combos/mock scripts behave
//!   identically) and gets its own session journal under the same store;
//! - one turn only: the child answers from its own context, not the parent's;
//! - cancellation propagates: the child stops when the parent is cancelled.

use crate::memory::MemoryTool;
use crate::{memory, Agent, AgentConfig, AgentEvent};
use async_trait::async_trait;
use loon_config::LoonPaths;
use loon_providers::ProviderRegistry;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Markdown agent definitions under global `agents/` and `.loon/agents/`.
#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_turns: u32,
    pub tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub isolation: bool,
    pub body: String,
}
pub fn profiles(data: &std::path::Path, cwd: &std::path::Path) -> Vec<Profile> {
    let mut profiles = std::collections::BTreeMap::new();
    for dir in [data.join("agents"), cwd.join(".loon/agents")] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (fields, body) = crate::guidance::frontmatter(&text);
            let name = fields.get("name").cloned().unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
            {
                continue;
            }
            let list = |key: &str| {
                fields
                    .get(key)
                    .map(|s| crate::guidance::list(s))
                    .unwrap_or_default()
            };
            profiles.insert(
                name.clone(),
                Profile {
                    name,
                    description: fields.get("description").cloned().unwrap_or_default(),
                    model: fields.get("model").cloned(),
                    effort: fields.get("effort").cloned(),
                    max_turns: fields
                        .get("maxTurns")
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(20)
                        .clamp(1, 1000),
                    tools: list("tools"),
                    disallowed_tools: list("disallowedTools"),
                    isolation: fields.get("isolation").is_some_and(|s| s == "worktree"),
                    body: body.to_string(),
                },
            );
        }
    }
    profiles.into_values().collect()
}

/// Resolve an `"id"` tool argument to a real agent id. The parameter schema declares
/// `"type": "integer"`, which includes negative numbers — they pass validation
/// (`serde_json` integers are `i64`) and then `as_u64()` yields `None`. Answering that
/// `None` with "id required" misdirects: the call DID supply an id, and only a negative
/// id can reach that line (a string, float, null or missing field is rejected by
/// argument validation before the body runs). Naming the unusable value tells the caller
/// the number was the problem, so it corrects the number instead of retrying the same
/// call or hunting for a missing field that is present.
fn background_agent_id(args: &Value, tool: &'static str) -> Result<u64, loon_tools::ToolError> {
    match args.get("id") {
        None => Err(loon_tools::ToolError::InvalidArgs(format!(
            "{tool} requires an id: pass the background agent id from background_spawn"
        ))),
        Some(raw) => raw.as_u64().ok_or_else(|| {
            loon_tools::ToolError::InvalidArgs(format!(
                "id must be a non-negative {tool} id, got {raw}"
            ))
        }),
    }
}

/// A short label for a child: the first line of its task, trimmed, capped.
fn task_label(task: &str) -> String {
    let first = task.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "subagent".to_string();
    }
    let mut label: String = first.chars().take(40).collect();
    if first.chars().count() > 40 {
        label.push('…');
    }
    label
}

/// Everything needed to construct a child session. Shared by `delegate` (blocking)
/// and `background_spawn` (non-blocking) so the two cannot drift apart in tool
/// filtering, isolation or cancellation behaviour.
#[derive(Clone)]
pub struct ChildSpec {
    pub paths: LoonPaths,
    pub cfg: AgentConfig,
    pub registry: Arc<ProviderRegistry>,
    pub cwd: PathBuf,
    /// The parent's durable task log, cloned so parent and children share one task graph:
    /// a clone holds the same `Arc<Mutex<()>>`, so read-modify-write is serialised. A child
    /// can mark its own task; plan/notes stay per-session by not being passed.
    /// `None` for children of an uncapped, task-free hub (tests, default constructor).
    pub tasks: Option<crate::worklog::WorkLog>,
    /// The parent session id, recorded in the child's journal so resume can filter child
    /// sessions out — a child is not a resumable conversation.
    pub parent: Option<loon_events::EvId>,
}

/// A child session built and ready for exactly one turn.
pub struct Child {
    pub agent: Agent,
    pub label: String,
    pub workspace: PathBuf,
}

/// Resolve the arguments of a delegation call into a ready-to-run child session.
async fn build_child(
    spec: &ChildSpec,
    args: &Value,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<(Child, String), loon_tools::ToolError> {
    let task = args["task"]
        .as_str()
        .ok_or_else(|| loon_tools::ToolError::InvalidArgs("task required".into()))?
        .to_string();
    let mut cwd = match args.get("cwd").and_then(Value::as_str) {
        Some(c) => spec.cwd.join(c),
        None => spec.cwd.clone(),
    };
    let root = spec.cwd.canonicalize().map_err(loon_tools::ToolError::Io)?;
    cwd = cwd.canonicalize().map_err(loon_tools::ToolError::Io)?;
    if !cwd.starts_with(&root)
        && !spec.cfg.permissions.extra_write_roots.iter().any(|p| {
            PathBuf::from(p)
                .canonicalize()
                .is_ok_and(|root| cwd.starts_with(root))
        })
    {
        return Err(loon_tools::ToolError::InvalidArgs(
            "subagent cwd is outside the parent's allowed roots".into(),
        ));
    }
    let profile = if let Some(name) = args["agent"].as_str() {
        Some(
            profiles(&spec.paths.data_dir, &spec.cwd)
                .into_iter()
                .find(|p| p.name == name)
                .ok_or_else(|| {
                    loon_tools::ToolError::InvalidArgs(format!(
                        "unknown agent profile {name}; use /agents"
                    ))
                })?,
        )
    } else {
        None
    };
    if profile.as_ref().is_some_and(|p| p.isolation) {
        let directory = spec
            .paths
            .data_dir
            .join("worktrees")
            .join(loon_events::EvId::now_v7().to_string());
        if let Some(parent) = directory.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let result = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["worktree", "add", "--detach"])
            .arg(&directory)
            .arg("HEAD")
            .kill_on_drop(true)
            .output()
            .await?;
        if !result.status.success() {
            return Err(loon_tools::ToolError::Other(format!(
                "worktree: {}",
                String::from_utf8_lossy(&result.stderr)
            )));
        }
        cwd = directory;
    }
    let model = profile
        .as_ref()
        .and_then(|p| p.model.clone())
        .unwrap_or_else(|| spec.cfg.model_ref.clone());
    spec.registry
        .resolve(&model)
        .map_err(loon_tools::ToolError::Other)?;
    let child_cfg = AgentConfig {
        model_ref: model,
        tool_output_bytes: spec.cfg.tool_output_bytes,
        max_retries: spec.cfg.max_retries,
        allow_retry_after_partial_stream: spec.cfg.allow_retry_after_partial_stream,
        // Children verify their own work only when explicitly asked to; inheriting
        // the parent's check command would run it once per subagent.
        check_command: None,
        ultra_rounds: 0,
        permissions: spec.cfg.permissions.clone(),
        context_window: spec.cfg.context_window,
        extra_tools: spec.cfg.extra_tools.clone(),
    };
    // The child's toolset mirrors the parent's minus delegation (no recursion):
    // background children are leaves as well, because a child owns a fresh hub
    // whose reports nobody would collect.
    let mut tools = loon_tools::ToolRegistry::standard()
        .with_extra(spec.cfg.extra_tools.clone())
        .with_extra(vec![Arc::new(MemoryTool {
            paths: Arc::new(memory::MemoryPaths::resolve(&spec.paths.data_dir, &cwd)),
        })]);
    // A task-linked child works on the parent's task graph: it can read its assignment and
    // mark progress. The shared WorkLog clone serialises writes with the parent.
    if let Some(log) = &spec.tasks {
        tools = tools.with_extra(vec![Arc::new(crate::tasks::TaskTool { log: log.clone() })]);
    }
    let workspace = cwd.clone();
    let mut child = Agent::new_session_with_parent(
        &spec.paths,
        child_cfg,
        spec.registry.clone(),
        tools,
        cwd,
        true,
        spec.parent,
    )
    .map_err(|e| loon_tools::ToolError::Other(format!("subagent session failed: {e}")))?;
    child.adopt_cancel_channel(cancel);
    if let Some(profile) = &profile {
        let tools = child
            .tools
            .specs()
            .into_iter()
            .filter(|s| {
                (profile.tools.is_empty() || profile.tools.contains(&s.name))
                    && !profile.disallowed_tools.contains(&s.name)
            })
            .filter_map(|s| child.tools.get(&s.name))
            .collect();
        child.tools = loon_tools::ToolRegistry::new(tools);
        child.refresh_tool_search();
        child.max_turn_steps = Some(profile.max_turns);
        if let Some(effort) = &profile.effort {
            if loon_config::Effort::parse(effort).is_none() {
                return Err(loon_tools::ToolError::InvalidArgs(format!(
                    "invalid agent effort {effort}"
                )));
            }
            child.effort = Some(effort.clone());
        }
    }
    // The label names the child in every surface that shows it (the live panel, the
    // transcript, background_list). A bare "subagent" for every child made three parallel
    // jobs indistinguishable; the first line of the task tells them apart.
    let label = match profile.as_ref() {
        Some(p) => p.name.clone(),
        None => task_label(&task),
    };
    let task = if let Some(profile) = &profile {
        format!(
            "Agent profile {}:\n{}\n\nTask:\n{task}",
            profile.name, profile.body
        )
    } else {
        task
    };
    Ok((
        Child {
            agent: child,
            label,
            workspace,
        },
        task,
    ))
}

/// Build the `delegate` tool.
pub fn delegate_tool(
    paths: LoonPaths,
    cfg: AgentConfig,
    registry: Arc<ProviderRegistry>,
    cwd: PathBuf,
    tasks: Option<crate::worklog::WorkLog>,
) -> Arc<dyn loon_tools::Tool> {
    Arc::new(DelegateTool {
        spec: ChildSpec {
            paths,
            cfg,
            registry,
            cwd,
            tasks,
            parent: None,
        },
    })
}

struct DelegateTool {
    spec: ChildSpec,
}

#[async_trait]
impl loon_tools::Tool for DelegateTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "delegate".into(),
            description: "Run a self-contained subtask in a fresh subagent session with the same tools and models, and return its final report. The subagent sees ONLY the task text you pass — include every file path, constraint, and acceptance criterion it needs. Use for bounded or context-heavy subtasks (survey a directory, draft a migration plan) that would flood this session's context.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "minLength": 1, "description": "Complete, self-contained task description."},
                    "cwd": {"type": "string", "description": "Optional workspace directory inside the parent's allowed roots."},
                    "agent": {"type":"string","description":"Optional configured profile name; /agents lists definitions."}
                },
                "required": ["task"],
                "additionalProperties": false
            }),
            effects: loon_tools::Effects::Mutating, // child tools can mutate files
        }
    }

    async fn run(
        &self,
        args: Value,
        ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        let (mut child, task) = build_child(&self.spec, &args, ctx.cancel.clone()).await?;
        let result = child.agent.run_turn(&task).await;
        child.agent.finish_session().await;
        let ok = matches!(result.status, crate::TurnStatus::Completed);
        let status = status_text(&result.status);
        let text = format!(
            "[subagent {status}; session {}; workspace {}]\n{}",
            child.agent.session_id(),
            child.workspace.display(),
            result.assistant_text
        );
        let (text, full, truncated) = ctx.bound(text);
        Ok(loon_tools::ToolOutput {
            ok,
            text,
            full,
            truncated,
            structured: Value::Null,
        })
    }
}

/// One-line status for a finished turn, shared by the blocking and background paths.
fn status_text(status: &crate::TurnStatus) -> String {
    match status {
        crate::TurnStatus::Completed => "completed".to_string(),
        crate::TurnStatus::Cancelled => "cancelled".to_string(),
        crate::TurnStatus::Failed(e) => format!("failed: {e}"),
    }
}

/// `loop_cancel`: lets the AGENT cancel loop tasks the user created.
pub struct LoopCancelTool {
    pub registry: Arc<crate::loops::LoopRegistry>,
}

#[async_trait::async_trait]
impl loon_tools::Tool for LoopCancelTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "loop_cancel".into(),
            description: "Cancel one recurring loop task (created by /loop). Pass the numeric task id; /loops in the session lists live ids. The user sees the cancellation.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Loop task id to cancel."}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            effects: loon_tools::Effects::Mutating,
        }
    }

    async fn run(
        &self,
        args: serde_json::Value,
        _ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        let id = background_agent_id(&args, "loop task")?;
        if self.registry.cancel(id) {
            Ok(loon_tools::ToolOutput::ok(format!(
                "loop #{id} cancelled (the user has been notified through the session)"
            )))
        } else {
            Err(loon_tools::ToolError::Other(format!("no loop task #{id}")))
        }
    }
}

/// A background subagent the parent started and can message, inspect or kill.
struct Job {
    label: String,
    status: JobStatus,
    /// When the child was spawned, so a listing can show how long it has run: a parent
    /// deciding between waiting and killing needs that, and "running" alone says nothing
    /// about a child that has been stuck for ten minutes.
    started: std::time::Instant,
    /// Queued messages reach the child at its next step boundary (the same channel
    /// steering uses), so a long-running child can be redirected mid-task.
    steering: mpsc::Sender<String>,
    cancel: tokio::sync::watch::Sender<bool>,
    /// The child's report once it finished. `None` while it is still running, and also
    /// after `trim_reports` releases it to keep retained memory bounded — callers must
    /// distinguish those two cases by the job's status, not by this field alone.
    report: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Failed => "failed",
        }
    }
}

/// One announcement the entry bound dropped before it could be delivered: the child's id
/// and whether its report text is still retained. A `false` readability means the text
/// bound released the same report, so promising the parent it can read that id would send
/// it after text that no longer exists.
pub type DroppedAnnouncement = (u64, bool);

/// A finished child's report as it is handed to the parent turn: id, label, status and
/// the report text (empty when the text bound released it).
pub type DeliveredReport = (u64, String, String, String);

/// Registry of background subagents, shared between the parent's tools and the tasks
/// those tools spawn. Deliberately plain: a mutex-guarded map, no runtime of its own.
///
/// Default cap on concurrently running background children. Eight is enough to saturate
/// most rate limits and to fan out over a real task graph, while bounding the blast radius
/// of a parent that decides to spawn forty investigate-the-parser children.
pub const DEFAULT_MAX_RUNNING: usize = 8;

/// The parent learns outcomes two ways: `background_list` for the model, and the
/// event sink for the user — `SubagentUpdate` when a child starts, is messaged or is
/// killed (its steering is otherwise invisible), `SubagentFinished` when one reports.
#[derive(Clone)]
pub struct BackgroundHub {
    inner: Arc<Mutex<HubState>>,
    /// The parent's durable task log, when the hub should link children to tasks. Set for
    /// real sessions; `None` keeps the hub task-free (tests, default constructor).
    tasks: Option<crate::worklog::WorkLog>,
}

impl Default for BackgroundHub {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HubState::default())),
            tasks: None,
        }
    }
}

#[derive(Default)]
struct HubState {
    next_id: u64,
    /// Concurrent-children cap. `None` means unbounded, which keeps the default
    /// constructor compatible; real sessions build the hub capped.
    max_running: Option<usize>,
    jobs: std::collections::BTreeMap<u64, Job>,
    /// Where finished reports are announced; set by the UI/headless owner.
    sink: Option<mpsc::Sender<AgentEvent>>,
    /// Reports not yet handed to the parent model. Drained at step boundaries.
    pending: Vec<(u64, String, String, String)>,
    /// Announcements the entry bound dropped since the last drain. Only the ids are kept:
    /// whether a report is still readable is decided at DRAIN time, because the text bound
    /// keeps running in between (a later child finishing can release a report that was
    /// still retained when its announcement was dropped). Recording the answer at drop time
    /// made the notice name ids whose text had since been released.
    dropped_reports: Vec<u64>,
}

impl BackgroundHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// A hub that refuses to run more than `max` children at once. Parallel children are
    /// useful up to a point and a resource problem past it: each is a full model loop, so
    /// the cap is the difference between "fan out over my tasks" and "forty concurrent
    /// inference sessions". Checked under the hub lock at spawn, so a fan-out of N cannot
    /// overshoot it.
    pub fn new_capped(max: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HubState {
                max_running: Some(max.max(1)),
                ..HubState::default()
            })),
            tasks: None,
        }
    }

    /// Link this hub to the parent's task log: spawned children may carry a task id, and
    /// finishing updates that task's status so the parent reads state from the task list
    /// instead of correlating reports by memory.
    pub fn with_tasks(mut self, log: crate::worklog::WorkLog) -> Self {
        self.tasks = Some(log);
        self
    }

    /// Mark the task assigned to agent `id`, if the hub is task-linked. `completed` on
    /// success, `blocked` on a failed/cancelled child — either way the parent sees the
    /// state change on the task list. Fail-open: no linked task is not an error.
    fn update_task_for_agent(&self, id: u64, ok: bool) {
        let Some(log) = &self.tasks else { return };
        let _ = log.update(|state| {
            if let Some(task) = state
                .tasks
                .iter_mut()
                .find(|t| t.assignee == Some(id) && t.status == "in_progress")
            {
                task.status = if ok { "completed" } else { "blocked" }.into();
            }
            Ok(())
        });
    }

    /// Total report bytes the hub will retain for finished jobs, counting each report
    /// once. The hub keeps up to two copies of a surviving report — `job.report` (read by
    /// `background_list`) and the `pending` queue (drained into the parent's context at
    /// the next step boundary) — and both are released together, so peak retention is
    /// bounded by twice this figure while the amount of child output that survives is
    /// bounded by this figure. A long session that spawns many children must not
    /// accumulate every report for the hub's whole lifetime; the oldest reports are
    /// released first, and the job entries themselves stay listed so the record that the
    /// work happened (and its status) survives.
    ///
    /// The newest report is exempt: it is never released, even when it alone exceeds this
    /// figure. One child answering with more than the budget had its ONLY result taken from
    /// both surfaces the moment it arrived, so the parent was handed "released" for work it
    /// had asked for and could never read it again. The budget bounds accumulation, so the
    /// documented peak is `2 * RETAINED_REPORT_BYTES`, or twice the newest report when that
    /// single report is larger than the budget.
    const RETAINED_REPORT_BYTES: usize = 64 * 1024;

    /// How many finished reports the announcement queue will carry into one step
    /// boundary. `RETAINED_REPORT_BYTES` bounds report *text*, but every drained entry
    /// also pays a `[background #id label status]` header when the parent turn injects
    /// it, and that header is added at injection time — so a flood of tiny reports
    /// stays under the byte budget while the headers alone grow without limit (8 000
    /// eight-byte reports measured 357 783 injected bytes against a 64 KB budget).
    /// Bounding the count is what actually bounds the step. The job entries themselves
    /// stay listed, so `background_list` still reports every outcome.
    ///
    /// 32 is chosen from the header cost: a `[background #N label status]` header with a
    /// realistic label measures roughly 30-45 bytes, so 32 announcements cost on the
    /// order of a kilobyte even when every report body has been released — small next to
    /// any step's tool output, while still covering the ordinary case (a handful of
    /// children) with no dropping at all. The bound only engages on a burst, and when it
    /// does the drain reports the count instead of hiding it.
    const RETAINED_REPORT_ENTRIES: usize = 32;

    /// Attach the parent's event sink so finishes show up in the transcript as they
    /// happen, instead of only when someone calls `background_list`.
    pub fn set_sink(&self, sink: mpsc::Sender<AgentEvent>) {
        self.lock().sink = Some(sink);
    }

    /// True when both handles refer to the same hub. Used to pin that a session switch
    /// carries the hub rather than silently replacing it: a fresh hub loses sight of the
    /// running children, whose reports then land where nothing reads them.
    pub fn same_hub(&self, other: &BackgroundHub) -> bool {
        std::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        // A panicked child task must not poison the registry for the parent.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a job and start it. Returns the id the parent uses to manage it.
    async fn spawn(
        &self,
        spec: ChildSpec,
        args: Value,
        cancel: tokio::sync::watch::Receiver<bool>,
        task_id: Option<u64>,
    ) -> Result<u64, loon_tools::ToolError> {
        // Build before returning: a bad cwd/profile/model must fail the tool call (the
        // parent needs to see it), not surface later as a background failure.
        let (mut child, task) = build_child(&spec, &args, cancel).await?;
        let id = {
            let mut state = self.lock();
            // The cap is checked under the same lock that assigns the id, so a fan-out of
            // N parallel spawns cannot race past it: each spawn sees the registrations of
            // the others. Checked AFTER build_child so a failed build does not consume a
            // slot, but BEFORE the id is assigned and the task launched.
            if let Some(max) = state.max_running {
                let running = state
                    .jobs
                    .values()
                    .filter(|j| j.status == JobStatus::Running)
                    .count();
                if running >= max {
                    return Err(loon_tools::ToolError::Other(format!(
                        "background agent limit reached ({max} running) — wait for one to \
                         finish or background_kill one before spawning more"
                    )));
                }
            }
            state.next_id += 1;
            state.next_id
        };
        let label = child.label.clone();
        let steering = child.agent.steering_tx.clone();
        let cancel_tx = child.agent.cancel_handle();
        let hub = self.clone();
        {
            // Register BEFORE the task can run. `finish` looks the job up by id, so if the
            // task were started first a child that completed inside that gap would find no
            // entry, drop its own report, and sit at "running" forever while the parent
            // waited for a result that had already arrived.
            let mut state = self.lock();
            state.jobs.insert(
                id,
                Job {
                    label: label.clone(),
                    status: JobStatus::Running,
                    started: std::time::Instant::now(),
                    steering,
                    cancel: cancel_tx,
                    report: None,
                },
            );
            if let Some(task_id) = task_id {
                // The task graph is the parent's; the child reports into it. `ok` is
                // decided at finish; here we only claim the assignment.
                if let Some(log) = &self.tasks {
                    let _ = log.update(|state| {
                        if let Some(t) = state.tasks.iter_mut().find(|t| t.id == task_id) {
                            t.status = "in_progress".into();
                            t.assignee = Some(id);
                        }
                        Ok(())
                    });
                }
            }
            if let Some(sink) = state.sink.clone() {
                let _ = sink.try_send(AgentEvent::SubagentUpdate {
                    id,
                    label: label.clone(),
                    status: JobStatus::Running.as_str().to_string(),
                    note: match task_id {
                        Some(t) => format!("started (task #{t})"),
                        None => "started".into(),
                    },
                });
            }
        }
        tokio::spawn(async move {
            let result = child.agent.run_turn(&task).await;
            child.agent.finish_session().await;
            let status = match &result.status {
                crate::TurnStatus::Completed => JobStatus::Completed,
                crate::TurnStatus::Cancelled => JobStatus::Cancelled,
                crate::TurnStatus::Failed(_) => JobStatus::Failed,
            };
            let text = if result.assistant_text.trim().is_empty() {
                status_text(&result.status)
            } else {
                result.assistant_text.clone()
            };
            hub.finish(
                id,
                label,
                status,
                text,
                matches!(status, JobStatus::Completed),
            );
        });
        Ok(id)
    }

    /// Record a child's finish and tell the parent. Called from the child's task.
    /// The `label` travels with the task (rather than being read back from the map) so a
    /// finish can never be dropped even if the entry were somehow absent: it is parked as
    /// a pending report with an `unknown` entry rather than silently discarded.
    fn finish(&self, id: u64, label: String, status: JobStatus, text: String, ok_status: bool) {
        let mut state = self.lock();
        let label = match state.jobs.get_mut(&id) {
            Some(job) => {
                job.status = status;
                job.report = Some(text.clone());
                job.label.clone()
            }
            None => {
                // Defensive only: `spawn` inserts before the task starts, so this should
                // not happen. If it ever did, dropping the report here would leave the
                // child "running" forever while its result was already in hand — park it
                // visibly instead.
                state.jobs.insert(
                    id,
                    Job {
                        label: label.clone(),
                        status,
                        started: std::time::Instant::now(),
                        steering: mpsc::channel(1).0,
                        cancel: tokio::sync::watch::channel(false).0,
                        report: Some(text.clone()),
                    },
                );
                label
            }
        };
        state
            .pending
            .push((id, label.clone(), status.as_str().into(), text.clone()));
        // Keep total retained report bytes bounded, oldest-first, across BOTH copies.
        // The entry itself stays (its status is the record that the work happened);
        // only the report text is released, so `background_list` degrades to "finished,
        // report released" and the next step boundary still injects the child's id and
        // status (with empty text) instead of silently saying nothing.
        Self::trim_reports(&mut state);
        drop(state);
        // A finished child marks its task, so the parent reads state from the task list
        // and content from the report. Outside the hub lock: the task log has its own.
        self.update_task_for_agent(id, ok_status);
        let state = self.lock();
        if let Some(sink) = state.sink.clone() {
            // try_send: a full or dropped channel must not stall a finishing child.
            let _ = sink.try_send(AgentEvent::SubagentFinished {
                id,
                label,
                status: status.as_str().to_string(),
                text,
            });
        }
    }

    /// Release the oldest finished reports until the retained total fits the budget.
    /// The budget covers `job.report` AND the matching `pending` queue entry jointly:
    /// they are two copies of the same text in the same age order, so releasing one
    /// without the other would either under-report the real retained memory or leave
    /// the two surfaces disagreeing about what survived. A released queue entry keeps
    /// its (id, label, status) so the parent still learns the outcome at the next step
    /// boundary — only the undelivered text is dropped. Newest-first scan so the most
    /// recent result — the one the parent is most likely to read next — is the last
    /// thing released. Running jobs are never touched: their report is `None` and
    /// dropping it would lose the result entirely.
    fn trim_reports(state: &mut HubState) {
        // Each report is counted ONCE even though the hub may hold it twice: the budget
        // is a promise about how much child output survives, not about how many copies
        // the bookkeeping happens to keep. Counting the duplicates would evict a single
        // large report from both surfaces the instant it arrived.
        let mut total: usize = state
            .jobs
            .values()
            .filter_map(|job| job.report.as_ref().map(|r| r.len()))
            .sum();
        if total > Self::RETAINED_REPORT_BYTES {
            // BTreeMap iterates by ascending id, i.e. oldest first — exactly the eviction
            // order we want, and the queue is push-ordered so the same pass finds the same
            // report's second copy.
            //
            // The newest report is never evicted, even when it alone exceeds the budget.
            // Without this, one child answering with more than `RETAINED_REPORT_BYTES` had
            // its ONLY result taken from both surfaces the moment it arrived: the parent was
            // left with "released" and `background_list` could never return the text again
            // (measured at 65 536 bytes and above; 65 000 was still fine). The child had done
            // the work and the parent could not read a byte of it — the exact loss this
            // mechanism exists to prevent. The budget is a bound on ACCUMULATION, so it may be
            // exceeded by at most one report: peak retention is
            // `2 * RETAINED_REPORT_BYTES`, or twice the newest report if that alone is larger.
            let mut reports_left = state
                .jobs
                .values()
                .filter(|job| job.report.is_some())
                .count();
            for (id, job) in state.jobs.iter_mut() {
                if total <= Self::RETAINED_REPORT_BYTES || reports_left <= 1 {
                    break;
                }
                if let Some(report) = job.report.take() {
                    // Counted once, subtracted once. The queued copy is released in the
                    // same pass but is NOT part of `total`, so subtracting it as well
                    // drove the running total below zero as soon as two reports differed
                    // in size: 50 KB + 30 KB panicked with "attempt to subtract with
                    // overflow", and in release the wrapped total evicted every survivor.
                    total -= report.len();
                    reports_left -= 1;
                    for (queued_id, _, _, queued_text) in state.pending.iter_mut() {
                        if *queued_id == *id {
                            queued_text.clear();
                        }
                    }
                }
            }
        }
        // Bound how many entries reach a single step boundary. The text bound above
        // cannot see the per-entry `[background #id label status]` header the parent adds
        // when it injects a report, so thousands of tiny reports slip past it: 8 000
        // eight-byte reports are 64 000 bytes of text but 357 783 bytes injected.
        let excess = state
            .pending
            .len()
            .saturating_sub(Self::RETAINED_REPORT_ENTRIES);
        if excess > 0 {
            // Oldest first: the most recent outcome is the one the parent acts on next.
            // The job entries stay listed, so `background_list` still reports them; only
            // this queue's announcement is dropped, and the drain says how many.
            // Remember WHICH announcements were dropped, not just how many: the parent
            // is told to recover them with `background_list`, so the notice has to name
            // ids that are actually readable. Readability is NOT recorded here — the text
            // bound may release this report before the drain, so the answer is computed
            // when the notice is built (see `drain_reports`).
            state
                .dropped_reports
                .extend(state.pending.drain(..excess).map(|(id, ..)| id));
        }
    }

    /// Reports produced since the last drain, as (id, label, status, text), plus the
    /// announcements the entry bound dropped before they could be delivered — each with
    /// whether its text is still retained. The parent turn loop drains this at step
    /// boundaries so the model sees what its agents did; the dropped entries are returned
    /// rather than swallowed so a released result is never mistaken for a child that
    /// finished and said nothing, and so the notice can name only ids that can be read.
    pub fn drain_reports(&self) -> (Vec<DeliveredReport>, Vec<DroppedAnnouncement>) {
        let mut state = self.lock();
        // Decide readability NOW, against the state the notice will be built from. A report
        // that was retained when its announcement was dropped may have been released by a
        // later `trim_reports` (another child finishing is enough), so an answer recorded at
        // drop time could name an id that `background_list` can no longer return.
        let dropped = std::mem::take(&mut state.dropped_reports)
            .into_iter()
            .map(|id| {
                let readable = state.jobs.get(&id).is_some_and(|job| job.report.is_some());
                (id, readable)
            })
            .collect();
        (std::mem::take(&mut state.pending), dropped)
    }

    fn message(&self, id: u64, text: String) -> Result<(), String> {
        let state = self.lock();
        let job = state
            .jobs
            .get(&id)
            .ok_or_else(|| format!("no background agent #{id}"))?;
        if job.status != JobStatus::Running {
            return Err(format!(
                "background agent #{id} already {}: it cannot take new messages",
                job.status.as_str()
            ));
        }
        // try_send reports a full queue instead of blocking the parent's tool call.
        job.steering
            .try_send(text)
            .map_err(|_| format!("background agent #{id} is not accepting messages yet; retry"))?;
        // The user typed nothing here — the parent model sent it — so without this the
        // transcript would show a child changing course for no visible reason.
        if let Some(sink) = state.sink.clone() {
            let _ = sink.try_send(AgentEvent::SubagentUpdate {
                id,
                label: job.label.clone(),
                status: JobStatus::Running.as_str().to_string(),
                note: "message delivered".into(),
            });
        }
        Ok(())
    }

    fn kill(&self, id: u64) -> Result<String, String> {
        let mut state = self.lock();
        let job = state
            .jobs
            .get_mut(&id)
            .ok_or_else(|| format!("no background agent #{id}"))?;
        if job.status != JobStatus::Running {
            return Err(format!(
                "background agent #{id} already {}",
                job.status.as_str()
            ));
        }
        // Cooperative: the child stops at its next step boundary and journals itself
        // as cancelled. Aborting the task would skip that bookkeeping and could race
        // the finish handler, leaving the job reported as running forever.
        let _ = job.cancel.send(true);
        job.status = JobStatus::Cancelled;
        job.report = Some("killed before reporting".into());
        // Announce it here rather than waiting for the child's task to unwind: the
        // user asked for this, and the cancellation only takes effect at the child's
        // next step boundary, which may be a while.
        let label = job.label.clone();
        if let Some(sink) = state.sink.clone() {
            let _ = sink.try_send(AgentEvent::SubagentUpdate {
                id,
                label,
                status: JobStatus::Cancelled.as_str().to_string(),
                note: "killed".into(),
            });
        }
        Ok(format!("background agent #{id} killed"))
    }

    /// Snapshot of every job: id, label, status, the report once finished, and how long
    /// the child has run (or ran) in whole seconds. Frozen at completion, so a finished
    /// entry keeps reporting its lifetime instead of growing forever.
    pub fn listing(&self) -> Vec<(u64, String, JobStatus, Option<String>, u64)> {
        self.lock()
            .jobs
            .iter()
            .map(|(id, job)| {
                let secs = job.started.elapsed().as_secs();
                (*id, job.label.clone(), job.status, job.report.clone(), secs)
            })
            .collect()
    }

    /// The jobs currently running: id, label and seconds elapsed. This is the live panel's
    /// data source — a finished child belongs in the transcript, not on a status row.
    pub fn running(&self) -> Vec<(u64, String, u64)> {
        self.lock()
            .jobs
            .iter()
            .filter(|(_, job)| job.status == JobStatus::Running)
            .map(|(id, job)| (*id, job.label.clone(), job.started.elapsed().as_secs()))
            .collect()
    }

    /// Seconds the given job has run (or ran), or `None` when no such job exists.
    pub fn elapsed_secs(&self, id: u64) -> Option<u64> {
        self.lock()
            .jobs
            .get(&id)
            .map(|job| job.started.elapsed().as_secs())
    }
}

/// `background_spawn`: start a subagent without blocking the parent turn.
pub struct BackgroundSpawnTool {
    pub spec: ChildSpec,
    pub hub: BackgroundHub,
}

#[async_trait]
impl loon_tools::Tool for BackgroundSpawnTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "background_spawn".into(),
            description: "Start a subagent in the background and return immediately with its id. The child runs one turn with the same tools and models, sees ONLY the task text you pass, and reports when done: you get its result via `background_list`, and the user sees it in the transcript. Use `background_message` to steer a running child and `background_kill` to stop one. Prefer this over `delegate` when the subtask is long or independent; use `delegate` when you need the answer before continuing.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "minLength": 1, "description": "Complete, self-contained task description."},
                    "cwd": {"type": "string", "description": "Optional workspace directory inside the parent's allowed roots."},
                    "agent": {"type": "string", "description": "Optional configured profile name; /agents lists definitions."}
                },
                "required": ["task"],
                "additionalProperties": false
            }),
            effects: loon_tools::Effects::Mutating,
        }
    }

    async fn run(
        &self,
        args: Value,
        ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        let id = self
            .hub
            .spawn(self.spec.clone(), args, ctx.cancel.clone(), None)
            .await?;
        Ok(loon_tools::ToolOutput::ok(format!(
            "background agent #{id} started; it reports when done. Use background_list to read its result, background_message to steer it, background_kill to stop it."
        )))
    }
}

/// The first `limit` bytes of `text`, retreated to a character boundary so the result is
/// always valid UTF-8. Mirrors `ToolCtx::bound`'s retreat: slicing a multibyte character
/// in half would panic.
fn clip_to_char_boundary(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// `task_fanout`: one call that spawns a child per task from the shared task graph and
/// links each child to its task. This is what makes an orchestrating parent one tool call
/// instead of N: split the work into tasks, fan out, then read the task list for state and
/// `background_list` for content.
pub struct TaskFanoutTool {
    pub spec: ChildSpec,
    pub hub: BackgroundHub,
}

#[async_trait]
impl loon_tools::Tool for TaskFanoutTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "task_fanout".into(),
            description: "Spawn one background subagent per task from the task graph, linking each child to its task: the child is marked in_progress with the agent id as assignee, and finishing updates the task's status automatically. Tasks must exist (use `task create`) and be `pending`. Concurrency is capped — extra tasks are refused and can be fanned out in a later call as slots free. Use this to run many independent tasks in parallel; use `delegate` when you need one answer before continuing and `background_spawn` for a single child.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_ids": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 16,
                        "items": {"type": "integer", "minimum": 1},
                        "description": "Task ids from the task graph to fan out over."
                    },
                    "agent": {
                        "type": "string",
                        "maxLength": 64,
                        "description": "Optional agent profile name from /agents."
                    }
                },
                "required": ["task_ids"],
                "additionalProperties": false
            }),
            effects: loon_tools::Effects::Mutating,
        }
    }

    async fn run(
        &self,
        args: Value,
        ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        let Some(log) = &self.hub.tasks else {
            return Err(loon_tools::ToolError::Other(
                "task_fanout requires a task-linked hub (this session has no shared task graph)"
                    .into(),
            ));
        };
        let ids: Vec<u64> = args["task_ids"]
            .as_array()
            .ok_or_else(|| loon_tools::ToolError::InvalidArgs("task_ids array required".into()))?
            .iter()
            .map(|v| {
                v.as_u64().ok_or_else(|| {
                    loon_tools::ToolError::InvalidArgs("task ids must be positive integers".into())
                })
            })
            .collect::<Result<_, _>>()?;
        let profile = args["agent"].as_str().map(str::to_string);

        // Snapshot the tasks first: subjects become the children's labels, and a missing
        // or non-pending task is reported per task rather than failing the whole batch.
        let state = log
            .read()
            .map_err(|e| loon_tools::ToolError::Other(e.to_string()))?;
        let mut spawns = Vec::new();
        let mut skipped: Vec<(u64, String)> = Vec::new();
        for id in &ids {
            match state.tasks.iter().find(|t| t.id == *id) {
                None => skipped.push((*id, "no such task".into())),
                Some(t) if t.status != "pending" => {
                    skipped.push((*id, format!("already {}", t.status)));
                }
                Some(t) => spawns.push((*id, t.subject.clone())),
            }
        }

        let mut spawned: Vec<(u64, u64)> = Vec::new();
        let mut errors: Vec<(u64, String)> = Vec::new();
        for (task_id, subject) in spawns {
            // Each child's task is its own text: the subject names the row, the task text
            // is what the child actually works on.
            let task_text = {
                let state = log
                    .read()
                    .map_err(|e| loon_tools::ToolError::Other(e.to_string()))?;
                state
                    .tasks
                    .iter()
                    .find(|t| t.id == task_id)
                    .map(|t| {
                        if t.description.trim().is_empty() {
                            t.subject.clone()
                        } else {
                            format!("{}\n\n{}", t.subject, t.description)
                        }
                    })
                    .unwrap_or_else(|| subject.clone())
            };
            let mut child_args = serde_json::json!({"task": task_text});
            if let Some(name) = &profile {
                child_args["agent"] = Value::String(name.clone());
            }
            match self
                .hub
                .spawn(
                    self.spec.clone(),
                    child_args,
                    ctx.cancel.clone(),
                    Some(task_id),
                )
                .await
            {
                Ok(agent_id) => spawned.push((task_id, agent_id)),
                Err(e) => errors.push((task_id, e.to_string())),
            }
        }

        let mut out = String::new();
        for (task_id, agent_id) in &spawned {
            out.push_str(&format!(
                "task #{task_id} -> background agent #{agent_id}\n"
            ));
        }
        for (task_id, reason) in &skipped {
            out.push_str(&format!("task #{task_id} skipped: {reason}\n"));
        }
        for (task_id, error) in &errors {
            out.push_str(&format!("task #{task_id} FAILED: {error}\n"));
        }
        if spawned.is_empty() {
            out.push_str("no tasks were fanned out — resolve the errors above first\n");
        } else {
            out.push_str(&format!(
                "\n{} agent(s) running; task statuses update automatically as they finish. \
                 Read reports with background_list, state with task list.",
                spawned.len()
            ));
        }
        Ok(loon_tools::ToolOutput::ok(out))
    }
}

/// `background_list`: the parent's view of the agents it started.
pub struct BackgroundListTool {
    pub hub: BackgroundHub,
}

impl BackgroundListTool {
    /// How much of one child's report the listing renders inline. The listing is bounded
    /// as a whole by the tool-output limit, and that limit cuts the end of the text, so
    /// one oversized body would otherwise crowd out every other child's result —
    /// including the newest, which is what the parent is waiting on. Clipping a single
    /// body costs part of one report; letting it run unbounded costs all the others.
    const LISTING_REPORT_BYTES: usize = 4096;
}

#[async_trait]
impl loon_tools::Tool for BackgroundListTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "background_list".into(),
            description: "List the background subagents this session started, with their status and, once finished, the report each one produced. Use it to collect results and to decide whether to wait, message, or kill. Pass `id` to read one specific child: the full listing is bounded, so in a long session it cannot show every child and a named child is the only way to reach an older result.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Optional: read only this background agent, by the id background_spawn returned."}
                },
                "additionalProperties": false
            }),
            effects: loon_tools::Effects::ReadOnly,
        }
    }

    async fn run(
        &self,
        args: Value,
        ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        let mut jobs = self.hub.listing();
        // A bounded listing cannot show every child in a long session, so a named child
        // must be reachable by itself: otherwise the older results are unreachable
        // forever, since re-listing always cuts the same tail. Answer a named id before
        // the empty-hub shortcut, so "no background agent #N" is what a bad id gets
        // rather than a blanket "no background agents" that hides the mistake.
        //
        // `id` is declared `"type": "integer"`, so a NEGATIVE number is a valid argument and
        // passes argument validation — yet it is not a real agent id, and `as_u64()` yields
        // `None` for it. Folding that `None` into "no filter" answered a single-child request
        // with the WHOLE listing: the model asked to read one report and silently received
        // every child's report, with no error and nothing saying its filter was discarded.
        // `background_message` and `background_kill` refuse an unusable id; a named read must
        // not quietly do something else, so an unrepresentable id is reported instead.
        let named = match args.get("id") {
            None => None,
            Some(raw) => Some(raw.as_u64().ok_or_else(|| {
                loon_tools::ToolError::InvalidArgs(format!(
                    "id must be a non-negative background agent id, got {raw}"
                ))
            })?),
        };
        if let Some(id) = named {
            jobs.retain(|job| job.0 == id);
            if jobs.is_empty() {
                return Ok(loon_tools::ToolOutput::ok(format!(
                    "no background agent #{id}; use background_list with no arguments to see the ids this session started"
                )));
            }
        }
        if jobs.is_empty() {
            return Ok(loon_tools::ToolOutput::ok("no background agents"));
        }
        // Newest first. Job entries are retained for the session's whole life (the
        // retention bound releases only their report text), so a long session's listing
        // outgrows the tool-output bound below — and that bound cuts the END of the
        // text. Rendering oldest-first therefore truncated away exactly the result the
        // parent was waiting for and kept the stale ones it had already read.
        jobs.sort_by_key(|job| std::cmp::Reverse(job.0));
        let mut out = String::new();
        for (id, label, status, report, secs) in jobs {
            // Elapsed time is part of the decision the parent is making: "running" plus
            // "3s" means wait, "running" plus "10m0s" means steer or kill.
            out.push_str(&format!("#{id} {label} [{}] {secs}s\n", status.as_str()));
            match report {
                Some(report) => {
                    // One long report must not starve every other outcome out of the
                    // listing: without a per-entry cap a single 50 KB body fills the whole
                    // tool-output budget, so the other children's results never reach the
                    // model at all. Clipping one body loses less than hiding the rest.
                    //
                    // A named read is the opposite case: the parent asked for this one
                    // child precisely because the listing could not show it, so the small
                    // listing clip would defeat the request. Leave the body whole and let
                    // the tool-output bound below cut it honestly — that path sets
                    // `truncated` and keeps a blob of the full text, whereas the clip
                    // silently capped a deliberate single-child read at 4 KiB while
                    // reporting `truncated: false`.
                    //
                    // Because `id` exists, a clipped listing body is not lost: the marker
                    // names the exact call that returns the rest, so the model recovers it
                    // in one step instead of abandoning a report it could have read.
                    let cap = if named.is_some() {
                        report.len()
                    } else {
                        Self::LISTING_REPORT_BYTES
                    };
                    let body = clip_to_char_boundary(&report, cap);
                    for line in body.lines() {
                        out.push_str(&format!("  {line}\n"));
                    }
                    if body.len() < report.len() {
                        out.push_str(&format!(
                            "  [clipped: this report is {} bytes; {} shown. Call \
                             background_list with id {id} to read the whole report.]\n",
                            report.len(),
                            body.len()
                        ));
                    }
                }
                // A finished job with no report is not a child that returned nothing: the
                // retention bound released the text. Saying so keeps the model from
                // waiting on a result that will never arrive.
                None if status != JobStatus::Running => {
                    out.push_str(
                        "  (report released to keep memory bounded; it is no longer readable)\n",
                    );
                }
                None => {}
            }
        }
        let (text, full, truncated) = ctx.bound(out);
        Ok(loon_tools::ToolOutput {
            ok: true,
            text,
            full,
            truncated,
            structured: Value::Null,
        })
    }
}

/// `background_message`: steer a running subagent.
pub struct BackgroundMessageTool {
    pub hub: BackgroundHub,
}

#[async_trait]
impl loon_tools::Tool for BackgroundMessageTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "background_message".into(),
            description: "Send a message to a running background subagent. It is applied at the child's next step boundary, so a long task can be redirected without killing it. Fails if the agent already finished — read its report with background_list instead.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Background agent id from background_spawn."},
                    "message": {"type": "string", "minLength": 1, "description": "Instruction or context for the running child."}
                },
                "required": ["id", "message"],
                "additionalProperties": false
            }),
            effects: loon_tools::Effects::ReadOnly,
        }
    }

    async fn run(
        &self,
        args: Value,
        _ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        let id = background_agent_id(&args, "background agent")?;
        let message = args["message"]
            .as_str()
            .filter(|m| !m.trim().is_empty())
            .ok_or_else(|| loon_tools::ToolError::InvalidArgs("message required".into()))?;
        match self.hub.message(id, message.to_string()) {
            Ok(()) => Ok(loon_tools::ToolOutput::ok(format!(
                "message queued for background agent #{id}"
            ))),
            Err(e) => Err(loon_tools::ToolError::Other(e)),
        }
    }
}

/// `background_kill`: stop a running subagent.
pub struct BackgroundKillTool {
    pub hub: BackgroundHub,
}

#[async_trait]
impl loon_tools::Tool for BackgroundKillTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "background_kill".into(),
            description: "Stop a running background subagent. Its session is journaled as cancelled and its report reads 'killed before reporting'. Use when a subtask is no longer needed or has gone wrong.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Background agent id from background_spawn."}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            effects: loon_tools::Effects::Mutating,
        }
    }

    async fn run(
        &self,
        args: Value,
        _ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        let id = background_agent_id(&args, "background agent")?;
        match self.hub.kill(id) {
            Ok(note) => Ok(loon_tools::ToolOutput::ok(note)),
            Err(e) => Err(loon_tools::ToolError::Other(e)),
        }
    }
}

/// The host-level default toolset: workspace tools + memory + delegate + background tools.
pub fn default_tools(
    paths: &LoonPaths,
    cwd: &std::path::Path,
    cfg: &AgentConfig,
    registry: Arc<ProviderRegistry>,
    loops: Option<Arc<crate::loops::LoopRegistry>>,
) -> loon_tools::ToolRegistry {
    default_tools_with_hub(
        paths,
        cwd,
        cfg,
        registry,
        loops,
        BackgroundHub::new(),
        None,
        None,
    )
}

/// `default_tools` with an explicit hub, so the caller and the tools share one registry.
#[allow(clippy::too_many_arguments)] // one per wiring input; a params struct would hide the shape
pub fn default_tools_with_hub(
    paths: &LoonPaths,
    cwd: &std::path::Path,
    cfg: &AgentConfig,
    registry: Arc<ProviderRegistry>,
    loops: Option<Arc<crate::loops::LoopRegistry>>,
    hub: BackgroundHub,
    tasks: Option<crate::worklog::WorkLog>,
    parent: Option<loon_events::EvId>,
) -> loon_tools::ToolRegistry {
    let mut extra: Vec<Arc<dyn loon_tools::Tool>> = vec![Arc::new(MemoryTool {
        paths: Arc::new(memory::MemoryPaths::resolve(&paths.data_dir, cwd)),
    })];
    if let Some(loops) = loops {
        extra.push(Arc::new(LoopCancelTool { registry: loops }));
    }
    let spec = ChildSpec {
        paths: paths.clone(),
        cfg: cfg.clone(),
        registry,
        cwd: cwd.to_path_buf(),
        tasks: tasks.clone(),
        parent,
    };
    extra.push(Arc::new(BackgroundSpawnTool {
        spec: spec.clone(),
        hub: hub.clone(),
    }));
    extra.push(Arc::new(BackgroundListTool { hub: hub.clone() }));
    extra.push(Arc::new(BackgroundMessageTool { hub: hub.clone() }));
    extra.push(Arc::new(BackgroundKillTool { hub: hub.clone() }));
    if tasks.is_some() {
        // Fan-out is only offered when there is a task graph to fan out over.
        extra.push(Arc::new(TaskFanoutTool {
            spec: spec.clone(),
            hub: hub.clone(),
        }));
    }
    loon_tools::ToolRegistry::standard()
        .with_extra(extra)
        .with_extra(vec![delegate_tool(
            paths.clone(),
            cfg.clone(),
            spec.registry,
            cwd.to_path_buf(),
            tasks,
        )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_declares_task_and_optional_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LoonPaths {
            config_dir: tmp.path().join("c"),
            data_dir: tmp.path().join("d"),
            state_dir: tmp.path().join("s"),
            cache_dir: tmp.path().join("h"),
            runtime_dir: tmp.path().join("r"),
        };
        let cfg = AgentConfig::from(&loon_config::SessionCfg::default());
        let registry = Arc::new(ProviderRegistry::with_mock(Arc::new(
            loon_providers::MockProvider::new(),
        )));
        let tool = delegate_tool(paths, cfg, registry, tmp.path().to_path_buf(), None);
        assert_eq!(tool.spec().name, "delegate");
        let params = &tool.spec().parameters;
        assert_eq!(params["required"][0], "task");
    }

    #[test]
    fn a_finish_without_a_registered_entry_is_parked_not_dropped() {
        // Regression for the spawn/insert ordering window: `finish` looked the job up by
        // id and returned early when it was absent, so a child fast enough to complete
        // before `jobs.insert` ran would silently lose its report and sit "running".
        let hub = BackgroundHub::new();
        hub.finish(
            7,
            "orphaned work".into(),
            JobStatus::Completed,
            "done".into(),
            true,
        );

        let jobs = hub.listing();
        assert_eq!(
            jobs.len(),
            1,
            "the orphaned report must be parked, not dropped"
        );
        let (_, label, status, report, _) = &jobs[0];
        assert_eq!(label, "orphaned work");
        assert_eq!(*status, JobStatus::Completed);
        assert_eq!(report.as_deref(), Some("done"));

        let (pending, _dropped) = hub.drain_reports();
        assert_eq!(pending.len(), 1, "the parent must still receive the report");
        assert_eq!(pending[0].2, "completed");
        assert_eq!(pending[0].3, "done");

        assert!(
            hub.message(7, "late steering".into()).is_err(),
            "the parked entry is terminal: it must not accept steering"
        );
    }

    #[test]
    fn reports_of_different_sizes_stay_within_the_budget() {
        // The budget counts each report ONCE (the hub may hold two copies of it), so the
        // eviction must also account for it once. Subtracting the queued copy as well
        // drove the running total below zero as soon as two reports had different
        // sizes — a panic in a debug build, and in release a wrapped total that evicted
        // every remaining report. Equal-sized fixtures hid it, which is why every
        // earlier retention test passed: 50 KB + 30 KB is the ordinary case.
        let hub = BackgroundHub::new();
        hub.finish(
            1,
            "verbose".into(),
            JobStatus::Completed,
            "x".repeat(50_000),
            true,
        );
        hub.finish(
            2,
            "modest".into(),
            JobStatus::Completed,
            "y".repeat(30_000),
            true,
        );

        let listed: usize = hub
            .listing()
            .iter()
            .filter_map(|(_, _, _, r, _)| r.as_ref().map(|r| r.len()))
            .sum();
        let queued: usize = hub
            .drain_reports()
            .0
            .iter()
            .map(|(_, _, _, t)| t.len())
            .sum();
        assert!(
            listed + queued <= 2 * BackgroundHub::RETAINED_REPORT_BYTES,
            "unequal reports left {} bytes resident (listed {listed} + queued {queued})",
            listed + queued
        );
        // The older, larger report is the one released; the newest must still read.
        let newest = hub.listing().last().cloned().expect("a job remains");
        assert_eq!(newest.0, 2);
        assert_eq!(
            newest.3.as_deref(),
            Some("y".repeat(30_000).as_str()),
            "the newest report must survive an eviction triggered by an older one"
        );
    }

    #[test]
    fn a_flood_of_tiny_reports_cannot_overflow_one_step_boundary() {
        // The byte budget bounds report TEXT, but every drained entry also pays a
        // `[background #id label status]` header at injection time, and that header is
        // never counted. Thousands of tiny reports therefore stay under the text budget
        // while the parent absorbs hundreds of kilobytes of headers in a single step.
        let hub = BackgroundHub::new();
        for i in 0..8_000u64 {
            hub.finish(
                i + 1,
                format!("job{i}"),
                JobStatus::Completed,
                "x".repeat(8),
                true,
            );
        }

        let (drained, _dropped) = hub.drain_reports();
        let injected: usize = drained
            .iter()
            .map(|(id, label, status, text)| {
                format!("[background #{id} {label} {status}]\n{text}").len()
            })
            .sum();
        assert!(
            injected <= 2 * BackgroundHub::RETAINED_REPORT_BYTES,
            "one step boundary absorbed {injected} bytes from {} entries",
            drained.len()
        );
        // The newest outcome must still be delivered in full: a bound that drops the
        // most recent result is worse than the flood it prevents.
        let newest = drained.last().expect("at least one report is delivered");
        assert_eq!(newest.0, 8_000, "the newest report must be delivered last");
        assert_eq!(
            newest.3,
            "x".repeat(8),
            "the newest report must keep its text"
        );
    }
}
