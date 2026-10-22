//! Durable per-session plans and notes, separate from the lossy context summary.
use loon_tools::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkState {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub ultraloop: bool,
    /// 0 means unlimited reviews until completion or cancellation.
    #[serde(default)]
    pub ultraloop_limit: u32,
    #[serde(default)]
    pub supervisor_model: Option<String>,
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub active_skills: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<crate::tasks::Task>,
    #[serde(default)]
    pub next_task_id: u64,
    #[serde(default)]
    pub plan_mode: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}
#[derive(Clone)]
pub struct WorkLog {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}
impl WorkLog {
    pub fn new(data: &Path, session: loon_events::EvId) -> Self {
        Self {
            path: data.join("worklogs").join(format!("{session}.json")),
            lock: Arc::new(Mutex::new(())),
        }
    }
    pub fn read(&self) -> Result<WorkState, ToolError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| ToolError::Other(format!("worklog: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WorkState::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn update(
        &self,
        change: impl FnOnce(&mut WorkState) -> Result<(), ToolError>,
    ) -> Result<WorkState, ToolError> {
        let _lock = self
            .lock
            .lock()
            .map_err(|_| ToolError::Other("worklog lock poisoned".into()))?;
        let mut state = self.read()?;
        change(&mut state)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes =
            serde_json::to_vec_pretty(&state).map_err(|e| ToolError::Other(e.to_string()))?;
        loon_workspace::write_atomic(&self.path, &bytes)?;
        Ok(state)
    }
    pub fn prompt(&self) -> String {
        self.read()
            .map(|mut s| {
                if s.notes.len() > 12 {
                    s.notes = s.notes.split_off(s.notes.len() - 12);
                }
                let text = s.render();
                if text.chars().count() > 16_000 {
                    text.chars().take(16_000).collect::<String>()
                        + "\n[working state excerpt; /notes lists full notes]"
                } else {
                    text
                }
            })
            .unwrap_or_else(|e| format!("worklog unavailable: {e}"))
    }
}
impl WorkState {
    pub fn render(&self) -> String {
        let mut text = String::new();
        if self.plan_mode {
            text.push_str("Plan mode: inspect and propose. Workspace mutations are blocked until the user runs /plan off.\n");
        }
        if self.ultraloop {
            text.push_str("Ultraloop: active; independent supervisor checks completion.\n");
        }
        if let Some(goal) = &self.goal {
            text.push_str(&format!("Goal: {goal}\n"));
        }
        for (i, p) in self.plan.iter().enumerate() {
            text.push_str(&format!("{}. [{}] {}\n", i + 1, p.status, p.step));
        }
        if !self.tasks.is_empty() {
            text.push_str("Tasks:\n");
            for task in &self.tasks {
                text.push_str(&format!(
                    "#{} [{}] {} (depends: {:?})\n",
                    task.id, task.status, task.subject, task.blocked_by
                ));
                if !task.description.trim().is_empty() {
                    text.push_str(&format!("  Evidence/context: {}\n", task.description));
                }
            }
        }
        if !self.notes.is_empty() {
            text.push_str("Notes:\n");
            for n in &self.notes {
                text.push_str(&format!("- {n}\n"));
            }
        }
        text
    }
}
pub struct PlanTool {
    pub log: WorkLog,
}
#[async_trait::async_trait]
impl Tool for PlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
        name:"update_plan".into(), description:"Create or update the session todo plan. Supply the complete ordered plan with pending/in_progress/completed/blocked statuses. At most one step may be in progress. Persists across compaction and resume. Use for multi-step tasks; mark steps complete only with evidence.".into(),
        parameters:serde_json::json!({"type":"object","properties":{
            "plan":{"type":"array","minItems":1,"maxItems":40,"items":{"type":"object","properties":{"step":{"type":"string","minLength":1,"maxLength":500},"status":{"type":"string","enum":["pending","in_progress","completed","blocked"]}},"required":["step","status"],"additionalProperties":false}},
            "explanation":{"type":"string","maxLength":1000}},"required":["plan"],"additionalProperties":false}), effects:Effects::Mutating,
    }
    }
    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let plan: Vec<PlanStep> = serde_json::from_value(args["plan"].clone())
            .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if plan.is_empty()
            || plan.len() > 40
            || plan.iter().any(|s| {
                s.step.trim().is_empty()
                    || s.step.chars().count() > 500
                    || !["pending", "in_progress", "completed", "blocked"]
                        .contains(&s.status.as_str())
            })
            || plan.iter().filter(|s| s.status == "in_progress").count() > 1
        {
            return Err(ToolError::InvalidArgs(
                "provide 1–40 nonempty steps with valid statuses and at most one in_progress"
                    .into(),
            ));
        }
        let state = self.log.update(|s| {
            s.plan = plan;
            Ok(())
        })?;
        Ok(ToolOutput::ok(state.render()))
    }
}
pub struct NoteTool {
    pub log: WorkLog,
}
#[async_trait::async_trait]
impl Tool for NoteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {name:"note".into(),description:"Save or inspect session working notes: file paths, findings, decisions, failed attempts, verification and blockers. Notes survive compaction/resume. Use memory for cross-session preferences instead. Never store secrets.".into(),parameters:serde_json::json!({"type":"object","properties":{"action":{"type":"string","enum":["add","list","remove"]},"text":{"type":"string","maxLength":2000},"index":{"type":"integer","minimum":1}},"required":["action"],"additionalProperties":false}),effects:Effects::Mutating}
    }
    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if args["action"] == "list" {
            return Ok(ToolOutput::ok(self.log.prompt()));
        }
        let state = self.log.update(|s| {
            match args["action"].as_str() {
                Some("add") => {
                    let text = args["text"].as_str().unwrap_or("").trim();
                    if text.is_empty() || text.chars().count() > 2000 {
                        return Err(ToolError::InvalidArgs(
                            "text must be 1–2000 characters".into(),
                        ));
                    }
                    if !s.notes.iter().any(|n| n == text) {
                        if s.notes.len() >= 100 {
                            return Err(ToolError::InvalidArgs(
                                "100-note limit; remove obsolete notes first".into(),
                            ));
                        }
                        s.notes.push(text.into());
                    }
                }
                Some("remove") => {
                    let i = args["index"].as_u64().unwrap_or(0) as usize;
                    if i == 0 || i > s.notes.len() {
                        return Err(ToolError::InvalidArgs("index out of range".into()));
                    }
                    s.notes.remove(i - 1);
                }
                _ => {
                    return Err(ToolError::InvalidArgs(
                        "action must be add/list/remove".into(),
                    ))
                }
            }
            Ok(())
        })?;
        Ok(ToolOutput::ok(state.render()))
    }
}

impl crate::Agent {
    pub(crate) fn worklog_command(
        &mut self,
        notes: bool,
        args: Vec<String>,
    ) -> Result<String, String> {
        let state = self.worklog.read().map_err(|e| e.to_string())?;
        let prefix = if notes { "notes" } else { "todos" };
        if self.events.is_some() && (args.is_empty() || args[0] == "actions") {
            let mut items = Vec::new();
            if args.is_empty() {
                items.push(crate::catalog::MenuItem {
                    label: format!("+ Add {}", if notes { "note" } else { "todo" }),
                    detail: "Saved in this session; survives compaction and resume".into(),
                    command: format!("/{prefix} add"),
                });
                if notes {
                    for (i, n) in state.notes.iter().enumerate() {
                        items.push(crate::catalog::MenuItem {
                            label: format!("{}. {n}", i + 1),
                            detail: "Enter to edit or remove".into(),
                            command: format!("/{prefix} actions {}", i + 1),
                        });
                    }
                } else {
                    for (i, p) in state.plan.iter().enumerate() {
                        items.push(crate::catalog::MenuItem {
                            label: format!("{}. {}", i + 1, p.step),
                            detail: p.status.clone(),
                            command: format!("/{prefix} actions {}", i + 1),
                        });
                    }
                }
            } else {
                let n = args.get(1).ok_or("missing index")?;
                let ops = if notes {
                    vec!["edit", "remove"]
                } else {
                    vec!["start", "done", "block", "pending", "edit", "remove"]
                };
                for op in ops {
                    items.push(crate::catalog::MenuItem {
                        label: op.into(),
                        detail: format!("{prefix} #{n}"),
                        command: format!("/{prefix} {op} {n}"),
                    });
                }
            }
            self.emit(crate::AgentEvent::OpenMenu {
                title: if notes {
                    "Session notes"
                } else {
                    "Session todos"
                }
                .into(),
                items,
            });
            return Ok(String::new());
        }
        if self.events.is_some()
            && ((args.first().map(String::as_str) == Some("add") && args.len() == 1)
                || (args.first().map(String::as_str) == Some("edit") && args.len() == 2))
        {
            let (id, text) = if args[0] == "edit" {
                let n = args[1].parse::<usize>().map_err(|_| "invalid index")?;
                let text = if notes {
                    state.notes.get(n.saturating_sub(1)).cloned()
                } else {
                    state.plan.get(n.saturating_sub(1)).map(|p| p.step.clone())
                }
                .ok_or("index out of range")?;
                (n.to_string(), text)
            } else {
                ("new".into(), String::new())
            };
            self.emit(crate::AgentEvent::OpenEditor(crate::catalog::Editor {
                kind: if notes { "note" } else { "todo" }.into(),
                values: vec![id, text],
                editing: true,
                fields: vec![
                    crate::catalog::FieldSpec::text(),
                    crate::catalog::FieldSpec::text(),
                ],
            }));
            return Ok(String::new());
        }
        if args.is_empty() {
            let text = state.render();
            return Ok(if text.is_empty() {
                "No session tasks or notes yet. /todos add TEXT · /notes add TEXT".into()
            } else {
                text
            });
        }
        let op = args[0].as_str();
        let state=self.worklog.update(|s|{
            let text=args.get(1..).unwrap_or_default().join(" ");
            if op=="add" {
                if text.trim().is_empty() || text.chars().count()>2000 {return Err(ToolError::InvalidArgs("provide 1–2000 characters".into()));}
                if notes {if s.notes.len()>=100 {return Err(ToolError::InvalidArgs("100-note limit".into()));}s.notes.push(text);}
                else {if s.plan.len()>=40 || text.chars().count()>500 {return Err(ToolError::InvalidArgs("40-step / 500-character limit".into()));}s.plan.push(PlanStep {step:text,status:"pending".into()});}
            } else {
                let i=args.get(1).and_then(|v|v.parse::<usize>().ok()).unwrap_or(0);
                let len=if notes{s.notes.len()}else{s.plan.len()};
                if i==0 || i>len {return Err(ToolError::InvalidArgs("index out of range".into()));}
                match op {
                    "edit"=>{
                        let text=args.get(2..).unwrap_or_default().join(" ");
                        if text.trim().is_empty() || text.chars().count()>if notes{2000}else{500} {return Err(ToolError::InvalidArgs("text is empty or too long".into()));}
                        if notes{s.notes[i-1]=text;}else{s.plan[i-1].step=text;}
                    },
                    "remove"=>{if notes{s.notes.remove(i-1);}else{s.plan.remove(i-1);}},
                    "start"|"done"|"block"|"pending" if !notes=>{
                        if op=="start" {for p in &mut s.plan {if p.status=="in_progress" {p.status="pending".into();}}}
                        s.plan[i-1].status=match op{"start"=>"in_progress","done"=>"completed","block"=>"blocked",_=>"pending"}.into();
                    },
                    _=>return Err(ToolError::InvalidArgs("use add TEXT, remove N; todos also supports start/done/block/pending N".into())),
                }
            } Ok(())
        }).map_err(|e|e.to_string())?;
        self.emit(crate::AgentEvent::WorkStateChanged {
            state: state.clone(),
        });
        Ok(state.render())
    }
    pub(crate) fn emit_session_loaded(&self) {
        let blocks = self
            .history
            .iter()
            .filter_map(|m| match m.role {
                loon_providers::Role::User => Some(("user".into(), m.text.clone())),
                loon_providers::Role::Assistant if !m.text.is_empty() => {
                    Some(("assistant".into(), m.text.clone()))
                }
                _ => None,
            })
            .collect();
        self.emit(crate::AgentEvent::SessionLoaded { blocks });
        self.emit(crate::AgentEvent::ModelChanged {
            model: self.cfg.model_ref.clone(),
        });
        self.emit(crate::AgentEvent::EffortChanged {
            level: self.effort.clone().unwrap_or("medium".into()),
        });
        self.emit(crate::AgentEvent::ContextUpdated {
            tokens: self.estimated_context_tokens(),
        });
        if let Ok(state) = self.worklog.read() {
            self.emit(crate::AgentEvent::WorkStateChanged { state });
        }
    }
}

/// Familiar todo surfaces backed by the same durable plan used by supervision.
pub struct TodoTool {
    pub log: WorkLog,
    pub read_only: bool,
}
#[async_trait::async_trait]
impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        if self.read_only {
            return ToolSpec{name:"todo_read".into(),description:"Read the durable todo plan, task graph and working notes before resuming or deciding what remains. Survives compaction/restarts.".into(),effects:Effects::ReadOnly,parameters:serde_json::json!({"type":"object","properties":{},"additionalProperties":false})};
        }
        ToolSpec{name:"todo_write".into(),description:"Write the complete ordered todo checklist for the current request. Use before multi-step coding, update after meaningful steps, mark blockers explicitly and verify before completed. At most one in_progress. Empty list explicitly clears the checklist. Save detailed findings with note and task descriptions.".into(),effects:Effects::Mutating,parameters:serde_json::json!({"type":"object","properties":{"todos":{"type":"array","maxItems":40,"items":{"type":"object","properties":{"content":{"type":"string","minLength":1,"maxLength":500},"status":{"type":"string","enum":["pending","in_progress","completed","blocked"]}},"required":["content","status"],"additionalProperties":false}},"explanation":{"type":"string","maxLength":1000}},"required":["todos"],"additionalProperties":false})}
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if self.read_only {
            let text = self.log.read()?.render();
            let (text, full, truncated) = ctx.bound(text);
            return Ok(ToolOutput {
                ok: true,
                text,
                full,
                truncated,
                structured: serde_json::Value::Null,
            });
        }
        let todos = args["todos"]
            .as_array()
            .filter(|v| v.len() <= 40)
            .ok_or_else(|| {
                ToolError::InvalidArgs("todos must be an array of at most 40 items".into())
            })?;
        if todos.is_empty() {
            let state = self.log.update(|s| {
                s.plan.clear();
                Ok(())
            })?;
            return Ok(ToolOutput::ok(state.render()));
        }
        let plan: Vec<_> = todos
            .iter()
            .map(|v| serde_json::json!({"step":v["content"],"status":v["status"]}))
            .collect();
        PlanTool {
            log: self.log.clone(),
        }
        .run(
            serde_json::json!({"plan":plan,"explanation":args["explanation"]}),
            ctx,
        )
        .await
    }
}
