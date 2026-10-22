//! Deferred MCP schemas: native tools stay available, remote tools are loaded by search.
use loon_tools::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
pub type Loaded = Arc<Mutex<BTreeSet<String>>>;
pub struct SearchTool {
    pub specs: Vec<ToolSpec>,
    pub loaded: Loaded,
}
#[async_trait::async_trait]
impl Tool for SearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec{name:"tool_search".into(),description:"Discover MCP tools by name or description; load matching schemas for subsequent calls. Empty query lists names/descriptions only. Search before using remote tools. Native tools are already loaded.".into(),effects:Effects::ReadOnly,parameters:json!({"type":"object","properties":{"query":{"type":"string","maxLength":300},"limit":{"type":"integer","minimum":1,"maximum":10}},"additionalProperties":false})}
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let query = args["query"].as_str().unwrap_or("").to_lowercase();
        let words: Vec<_> = query.split_whitespace().collect();
        let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 10) as usize;
        let mut specs: Vec<_> = self
            .specs
            .iter()
            .map(|s| {
                let text = format!("{} {}", s.name, s.description).to_lowercase();
                let score = words.iter().filter(|w| text.contains(**w)).count();
                (score, s)
            })
            .filter(|(score, _)| words.is_empty() || *score > 0)
            .collect();
        specs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
        let mut values = vec![];
        for (_, spec) in specs
            .into_iter()
            .take(if words.is_empty() { 100 } else { limit })
        {
            if words.is_empty() {
                values.push(json!({"name":spec.name,"description":spec.description.chars().take(200).collect::<String>()}));
            } else {
                self.loaded
                    .lock()
                    .map_err(|_| ToolError::Other("tool search lock poisoned".into()))?
                    .insert(spec.name.clone());
                values.push(loon_tools::tool::to_provider_format(spec));
            }
        }
        let text = serde_json::to_string(&values).map_err(|e| ToolError::Other(e.to_string()))?;
        let (text, full, truncated) = ctx.bound(text);
        Ok(ToolOutput {
            ok: true,
            text,
            full,
            truncated,
            structured: Value::Null,
        })
    }
}
impl crate::Agent {
    pub(crate) fn exposed_specs(&self) -> Vec<ToolSpec> {
        let loaded = self.loaded_mcp.lock().unwrap_or_else(|e| e.into_inner());
        let plan_mode = self.worklog.read().map(|s| s.plan_mode).unwrap_or(false);
        self.tools
            .specs()
            .into_iter()
            .filter(|s| {
                !plan_mode
                    || s.effects == Effects::ReadOnly
                    || ["update_plan", "note", "task", "todo_write"].contains(&s.name.as_str())
            })
            .filter(|s| {
                !self.permissions.deny_rules.iter().any(|r| {
                    r.tool == s.name
                        && r.path_prefix.is_none()
                        && r.command_prefix.is_none()
                        && r.host.is_none()
                })
            })
            .filter(|s| !s.name.starts_with("mcp_") || loaded.contains(&s.name))
            .collect()
    }
    pub(crate) fn provider_tools(&self) -> Vec<Value> {
        self.exposed_specs()
            .iter()
            .map(loon_tools::tool::to_provider_format)
            .collect()
    }
    pub(crate) fn refresh_tool_search(&mut self) {
        let specs = self
            .tools
            .specs()
            .into_iter()
            .filter(|s| {
                s.name.starts_with("mcp_")
                    && !self.permissions.deny_rules.iter().any(|r| {
                        r.tool == s.name
                            && r.path_prefix.is_none()
                            && r.command_prefix.is_none()
                            && r.host.is_none()
                    })
            })
            .collect();
        self.tools = self
            .tools
            .clone()
            .without(&["tool_search"])
            .with_extra(vec![Arc::new(SearchTool {
                specs,
                loaded: self.loaded_mcp.clone(),
            })]);
    }
}
