//! Subagents: the `delegate` tool runs a scoped task in a fresh child session with
//! its own journal and tools, and returns the child's final text.
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

/// Build the `delegate` tool.
pub fn delegate_tool(
    paths: LoonPaths,
    cfg: AgentConfig,
    registry: Arc<ProviderRegistry>,
    cwd: PathBuf,
) -> Arc<dyn loon_tools::Tool> {
    Arc::new(DelegateTool {
        paths,
        cfg,
        registry,
        cwd,
    })
}

struct DelegateTool {
    paths: LoonPaths,
    cfg: AgentConfig,
    registry: Arc<ProviderRegistry>,
    cwd: PathBuf,
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
        let task = args["task"]
            .as_str()
            .ok_or_else(|| loon_tools::ToolError::InvalidArgs("task required".into()))?
            .to_string();
        let mut cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(c) => self.cwd.join(c),
            None => self.cwd.clone(),
        };
        let root = self.cwd.canonicalize().map_err(loon_tools::ToolError::Io)?;
        cwd = cwd.canonicalize().map_err(loon_tools::ToolError::Io)?;
        if !cwd.starts_with(&root)
            && !self.cfg.permissions.extra_write_roots.iter().any(|p| {
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
                profiles(&self.paths.data_dir, &self.cwd)
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
            let directory = self
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
            .unwrap_or_else(|| self.cfg.model_ref.clone());
        self.registry
            .resolve(&model)
            .map_err(loon_tools::ToolError::Other)?;
        let child_cfg = AgentConfig {
            model_ref: model,
            tool_output_bytes: self.cfg.tool_output_bytes,
            max_retries: self.cfg.max_retries,
            allow_retry_after_partial_stream: self.cfg.allow_retry_after_partial_stream,
            // Children verify their own work only when explicitly asked to; inheriting
            // the parent's check command would run it once per subagent.
            check_command: None,
            ultra_rounds: 0,
            permissions: self.cfg.permissions.clone(),
            context_window: self.cfg.context_window,
            extra_tools: self.cfg.extra_tools.clone(),
        };
        // The child's toolset mirrors the parent's minus delegation (no recursion).
        let tools = loon_tools::ToolRegistry::standard()
            .with_extra(self.cfg.extra_tools.clone())
            .with_extra(vec![Arc::new(MemoryTool {
                paths: Arc::new(memory::MemoryPaths::resolve(&self.paths.data_dir, &cwd)),
            })]);
        let child_cwd = cwd.clone();
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
        let task = if let Some(profile) = &profile {
            format!(
                "Agent profile {}:\n{}\n\nTask:\n{task}",
                profile.name, profile.body
            )
        } else {
            task
        };
        let result = child.run_turn(&task).await;
        child.finish_session().await;
        let ok = matches!(result.status, crate::TurnStatus::Completed);
        let status = match result.status {
            crate::TurnStatus::Completed => "completed".to_string(),
            crate::TurnStatus::Cancelled => "cancelled".to_string(),
            crate::TurnStatus::Failed(e) => format!("failed: {e}"),
        };
        let text = format!(
            "[subagent {status}; session {}; workspace {}]\n{}",
            child.session_id(),
            child_cwd.display(),
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
        let tool = delegate_tool(paths, cfg, registry, tmp.path().to_path_buf());
        assert_eq!(tool.spec().name, "delegate");
        let params = &tool.spec().parameters;
        assert_eq!(params["required"][0], "task");
    }
}
