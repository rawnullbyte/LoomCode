//! Subagents: the `delegate` tool runs a scoped task in a fresh child session with
//! its own journal, tools and budget, and returns the child's final text.
//!
//! Boundaries (deliberate):
//! - the child never gets the `delegate` tool — no recursive delegation;
//! - the child shares the parent's provider registry (so combos/mock scripts behave
//!   identically) and gets its own session journal under the same store;
//! - one turn only: the child answers from its own context, not the parent's;
//! - cancellation propagates: the child stops when the parent is cancelled.

use crate::memory::MemoryTool;
use crate::{memory, Agent, AgentConfig};
use async_trait::async_trait;
use loon_config::LoonPaths;
use loon_providers::ProviderRegistry;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Build the `delegate` tool. `max_steps` bounds the child's loop.
pub fn delegate_tool(
    paths: LoonPaths,
    cfg: AgentConfig,
    registry: Arc<ProviderRegistry>,
    cwd: PathBuf,
    max_steps: u32,
) -> Arc<dyn loon_tools::Tool> {
    Arc::new(DelegateTool {
        paths,
        cfg,
        registry,
        cwd,
        max_steps,
    })
}

struct DelegateTool {
    paths: LoonPaths,
    cfg: AgentConfig,
    registry: Arc<ProviderRegistry>,
    cwd: PathBuf,
    max_steps: u32,
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
                    "cwd": {"type": "string", "description": "Optional workspace root for the subagent; defaults to this session's workspace."}
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
        let task = args["task"]
            .as_str()
            .ok_or_else(|| loon_tools::ToolError::InvalidArgs("task required".into()))?
            .to_string();
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(c) => PathBuf::from(c),
            None => self.cwd.clone(),
        };
        let child_cfg = AgentConfig {
            model_ref: self.cfg.model_ref.clone(),
            max_steps: self.max_steps.min(24),
            tool_output_bytes: self.cfg.tool_output_bytes,
            max_retries: self.cfg.max_retries,
            allow_retry_after_partial_stream: self.cfg.allow_retry_after_partial_stream,
            permissions: self.cfg.permissions.clone(),
            context_window: self.cfg.context_window,
            extra_tools: Vec::new(), // children never inherit MCP tools
            check_command: None,
        };
        // The child's toolset mirrors the parent's minus delegation (no recursion).
        let tools = loon_tools::ToolRegistry::standard().with_extra(vec![Arc::new(MemoryTool {
            paths: Arc::new(memory::MemoryPaths::resolve(&self.paths.data_dir, &cwd)),
        })]);
        let mut child = Agent::new_session(
            &self.paths,
            child_cfg,
            self.registry.clone(),
            tools,
            cwd,
            true,
        )
        .map_err(|e| loon_tools::ToolError::Other(format!("subagent session failed: {e}")))?;
        child.adopt_cancel_channel(ctx.cancel.clone());
        let result = child.run_turn(&task).await;
        let status = match result.status {
            crate::TurnStatus::Completed => "completed".to_string(),
            crate::TurnStatus::Cancelled => "cancelled".to_string(),
            crate::TurnStatus::Failed(e) => format!("failed: {e}"),
            crate::TurnStatus::BudgetPaused { steps_used } => {
                format!("budget paused at {steps_used} steps")
            }
        };
        Ok(loon_tools::ToolOutput::ok(format!(
            "[subagent {status}]\n{}",
            result.assistant_text
        )))
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
        let id = args["id"]
            .as_u64()
            .ok_or_else(|| loon_tools::ToolError::InvalidArgs("id required".into()))?;
        if self.registry.cancel(id) {
            Ok(loon_tools::ToolOutput::ok(format!(
                "loop #{id} cancelled (the user has been notified through the session)"
            )))
        } else {
            Err(loon_tools::ToolError::Other(format!("no loop task #{id}")))
        }
    }
}

/// The host-level default toolset: workspace tools + memory + delegate.
pub fn default_tools(
    paths: &LoonPaths,
    cwd: &std::path::Path,
    cfg: &AgentConfig,
    registry: Arc<ProviderRegistry>,
    loops: Option<Arc<crate::loops::LoopRegistry>>,
) -> loon_tools::ToolRegistry {
    let mut extra: Vec<Arc<dyn loon_tools::Tool>> = vec![Arc::new(MemoryTool {
        paths: Arc::new(memory::MemoryPaths::resolve(&paths.data_dir, cwd)),
    })];
    if let Some(loops) = loops {
        extra.push(Arc::new(LoopCancelTool { registry: loops }));
    }
    loon_tools::ToolRegistry::standard()
        .with_extra(extra)
        .with_extra(vec![delegate_tool(
            paths.clone(),
            cfg.clone(),
            registry,
            cwd.to_path_buf(),
            24,
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
        let tool = delegate_tool(paths, cfg, registry, tmp.path().to_path_buf(), 4);
        assert_eq!(tool.spec().name, "delegate");
        let params = &tool.spec().parameters;
        assert_eq!(params["required"][0], "task");
    }
}
