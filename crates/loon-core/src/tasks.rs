//! Durable task graph. Mutations are atomic with the session worklog.
use crate::worklog::WorkLog;
use loon_tools::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub subject: String,
    pub description: String,
    pub status: String,
    #[serde(default)]
    pub blocked_by: Vec<u64>,
}
pub struct TaskTool {
    pub log: WorkLog,
}
impl TaskTool {
    pub fn apply(&self, args: Value) -> Result<String, ToolError> {
        let action = args["action"].as_str().unwrap_or("list");
        let state = if ["list", "get"].contains(&action) {
            self.log.read()?
        } else {
            self.log.update(|state| {
                let id = args["id"].as_u64().unwrap_or(0);
                match action {
                    "create" => {
                        if state.tasks.len() >= 200 {
                            return Err(ToolError::InvalidArgs("task limit reached (200)".into()));
                        }
                        let subject = args["subject"].as_str().unwrap_or("").trim();
                        if subject.is_empty() {
                            return Err(ToolError::InvalidArgs("subject required".into()));
                        }
                        state.next_task_id = state
                            .next_task_id
                            .max(state.tasks.iter().map(|t| t.id).max().unwrap_or(0))
                            + 1;
                        state.tasks.push(Task {
                            id: state.next_task_id,
                            subject: subject.into(),
                            description: args["description"].as_str().unwrap_or("").into(),
                            status: "pending".into(),
                            blocked_by: Vec::new(),
                        });
                    }
                    "update" => {
                        let mut task = state
                            .tasks
                            .iter()
                            .find(|t| t.id == id)
                            .cloned()
                            .ok_or_else(|| ToolError::InvalidArgs(format!("no task #{id}")))?;
                        if let Some(status) = args["status"].as_str() {
                            if !["pending", "in_progress", "completed", "blocked"].contains(&status)
                            {
                                return Err(ToolError::InvalidArgs("invalid status".into()));
                            }
                            task.status = status.into();
                        }
                        if let Some(subject) = args["subject"].as_str() {
                            if subject.trim().is_empty() {
                                return Err(ToolError::InvalidArgs(
                                    "subject cannot be empty".into(),
                                ));
                            }
                            task.subject = subject.into();
                        }
                        if let Some(desc) = args["description"].as_str() {
                            task.description = desc.into();
                        }
                        if let Some(deps) = args["blocked_by"].as_array() {
                            task.blocked_by = deps
                                .iter()
                                .map(|v| {
                                    v.as_u64().ok_or_else(|| {
                                        ToolError::InvalidArgs(
                                            "dependency ids must be positive integers".into(),
                                        )
                                    })
                                })
                                .collect::<Result<_, _>>()?;
                            for dep in &task.blocked_by {
                                if *dep == id || !state.tasks.iter().any(|t| t.id == *dep) {
                                    return Err(ToolError::InvalidArgs(
                                        "dependency must be an existing different task".into(),
                                    ));
                                }
                                let mut pending = vec![*dep];
                                let mut visited = std::collections::BTreeSet::new();
                                while let Some(next) = pending.pop() {
                                    if next == id {
                                        return Err(ToolError::InvalidArgs(
                                            "dependency cycle".into(),
                                        ));
                                    }
                                    if visited.insert(next) {
                                        if let Some(t) = state.tasks.iter().find(|t| t.id == next) {
                                            pending.extend(t.blocked_by.iter());
                                        }
                                    }
                                }
                            }
                        }
                        if ["completed", "in_progress"].contains(&task.status.as_str())
                            && task.blocked_by.iter().any(|dep| {
                                state
                                    .tasks
                                    .iter()
                                    .any(|t| t.id == *dep && t.status != "completed")
                            })
                        {
                            return Err(ToolError::InvalidArgs("finish dependencies first".into()));
                        }
                        *state.tasks.iter_mut().find(|t| t.id == id).unwrap() = task;
                    }
                    "remove" => {
                        if state.tasks.iter().any(|t| t.blocked_by.contains(&id)) {
                            return Err(ToolError::InvalidArgs(
                                "remove dependencies from other tasks first".into(),
                            ));
                        }
                        let n = state.tasks.len();
                        state.tasks.retain(|t| t.id != id);
                        if state.tasks.len() == n {
                            return Err(ToolError::InvalidArgs(format!("no task #{id}")));
                        }
                    }
                    _ => {
                        return Err(ToolError::InvalidArgs(
                            "action must be create/update/get/list/remove".into(),
                        ))
                    }
                }
                Ok(())
            })?
        };
        if action == "get" {
            let id = args["id"].as_u64().unwrap_or(0);
            let task = state
                .tasks
                .iter()
                .find(|t| t.id == id)
                .ok_or_else(|| ToolError::InvalidArgs(format!("no task #{id}")))?;
            return serde_json::to_string_pretty(task).map_err(|e| ToolError::Other(e.to_string()));
        }
        serde_json::to_string_pretty(&state.tasks).map_err(|e| ToolError::Other(e.to_string()))
    }
}
#[async_trait::async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec{name:"task".into(),description:"Create, update, get, list, or remove durable session tasks. Dependencies must finish before starting/completing a task. Tasks survive compaction and resume. Report evidence in description; never mark unchecked work complete.".into(),effects:Effects::Mutating,parameters:json!({"type":"object","properties":{"action":{"type":"string","enum":["create","update","get","list","remove"]},"id":{"type":"integer","minimum":1},"subject":{"type":"string","maxLength":500},"description":{"type":"string","maxLength":4000},"status":{"type":"string","enum":["pending","in_progress","completed","blocked"]},"blocked_by":{"type":"array","maxItems":100,"items":{"type":"integer","minimum":1}}},"required":["action"],"additionalProperties":false})}
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let (text, full, truncated) = ctx.bound(self.apply(args)?);
        Ok(ToolOutput {
            ok: true,
            text,
            full,
            truncated,
            structured: Value::Null,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependencies_cycles_and_resume() {
        let d = tempfile::tempdir().unwrap();
        let id = loon_events::EvId::now_v7();
        let tool = TaskTool {
            log: WorkLog::new(d.path(), id),
        };
        tool.apply(json!({"action":"create","subject":"build"}))
            .unwrap();
        tool.apply(json!({"action":"create","subject":"test"}))
            .unwrap();
        tool.apply(json!({"action":"update","id":2,"blocked_by":[1]}))
            .unwrap();
        assert!(tool
            .apply(json!({"action":"update","id":1,"blocked_by":[2]}))
            .is_err());
        assert!(tool
            .apply(json!({"action":"update","id":2,"status":"completed"}))
            .is_err());
        tool.apply(json!({"action":"update","id":1,"status":"completed"}))
            .unwrap();
        tool.apply(json!({"action":"update","id":2,"status":"completed"}))
            .unwrap();
        assert_eq!(
            WorkLog::new(d.path(), id).read().unwrap().tasks[1].status,
            "completed"
        );
    }
}
