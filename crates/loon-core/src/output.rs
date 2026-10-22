//! Re-read stored tool output in bounded slices instead of repeating expensive commands.
use loon_tools::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use std::sync::Arc;
pub struct OutputTool {
    pub journal: Arc<loon_events::Journal>,
}
#[async_trait::async_trait]
impl Tool for OutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
        name:"read_output".into(),description:"Read or search full tool output saved in a blob. Use the sha256 from a truncated tool result, a 1-based offset and bounded limit, or contains to filter lines. Avoid rerunning commands to retrieve omitted logs.".into(),
        parameters:serde_json::json!({"type":"object","properties":{"sha256":{"type":"string","minLength":64,"maxLength":64},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":1000},"contains":{"type":"string"}},"required":["sha256"],"additionalProperties":false}),effects:Effects::ReadOnly,
    }
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let sha = args["sha256"].as_str().unwrap_or("");
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ToolError::InvalidArgs(
                "sha256 must be a 64-character hex digest".into(),
            ));
        }
        let bytes = self
            .journal
            .blob_by_sha(sha)
            .map_err(|e| ToolError::Other(e.to_string()))?
            .ok_or_else(|| ToolError::Other("output blob not found".into()))?;
        let text = String::from_utf8_lossy(&bytes);
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(120).clamp(1, 1000) as usize;
        let query = args["contains"].as_str().unwrap_or("").to_lowercase();
        let lines = text
            .lines()
            .enumerate()
            .skip(offset - 1)
            .filter(|(_, l)| l.to_lowercase().contains(&query))
            .take(limit)
            .map(|(i, l)| format!("{}\t{}", i + 1, l))
            .collect::<Vec<_>>();
        let result = format!(
            "{}\n[{} lines shown; full output {} bytes; sha256 {sha}]",
            lines.join("\n"),
            lines.len(),
            bytes.len()
        );
        let (text, full, truncated) = ctx.bound(result);
        Ok(ToolOutput {
            ok: true,
            text,
            full,
            truncated,
            structured: serde_json::json!({"sha256":sha,"lines_shown":lines.len()}),
        })
    }
}
