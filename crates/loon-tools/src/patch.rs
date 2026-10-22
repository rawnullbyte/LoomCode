//! Apply a single-file unified diff with exact context and compare-and-swap writes.
use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
pub struct PatchTool;
#[async_trait]
impl Tool for PatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec{name:"apply_patch".into(),description:"Apply a standard unified diff to ONE explicit path. Include @@ hunk headers and context/removal/addition lines. Hunk context must match exactly and uniquely; stale or ambiguous patches are refused atomically. Supports creating a file with an additions-only hunk. File deletion and multi-file patches are not supported; use separate explicit operations. Changes are recorded for undo/redo.".into(),parameters:json!({"type":"object","properties":{"path":{"type":"string","minLength":1},"patch":{"type":"string","minLength":1}},"required":["path","patch"],"additionalProperties":false}),effects:Effects::Mutating}
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("path required".into()))?;
        let patch = args["patch"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("patch required".into()))?;
        let resolved = ctx.ws.resolve(path)?;
        let (bytes, hash) = if resolved.exists() {
            ctx.ws.read_file(&resolved)?
        } else {
            (Vec::new(), loon_workspace::EMPTY_HASH.to_string())
        };
        let mut text = String::from_utf8(bytes)
            .map_err(|_| ToolError::Other("cannot patch non-UTF-8 file".into()))?;
        let mut hunks: Vec<(String, String)> = Vec::new();
        let mut current: Option<(String, String)> = None;
        let mut headers = 0;
        let mut previous_kind = None;
        for line in patch.split_inclusive('\n') {
            if line.starts_with("@@ ") {
                if let Some(h) = current.take() {
                    hunks.push(h);
                }
                current = Some((String::new(), String::new()));
                continue;
            }
            if line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line.starts_with("diff --git ")
                || line.starts_with("index ")
            {
                if current.is_some() {
                    return Err(ToolError::InvalidArgs(
                        "multi-file patches are not supported".into(),
                    ));
                }
                if line.starts_with("+++ ") {
                    headers += 1;
                    if headers > 1 {
                        return Err(ToolError::InvalidArgs("one path per patch".into()));
                    }
                }
                continue;
            }
            if line.starts_with("\\ No newline") {
                if let Some((old, new)) = current.as_mut() {
                    // The marker applies to the immediately preceding side.
                    if previous_kind != Some(b'+') && old.ends_with('\n') {
                        old.pop();
                    }
                    if previous_kind != Some(b'-') && new.ends_with('\n') {
                        new.pop();
                    }
                }
                continue;
            }
            let Some((old, new)) = current.as_mut() else {
                if line.trim().is_empty() {
                    continue;
                }
                return Err(ToolError::InvalidArgs(
                    "expected a unified @@ hunk header".into(),
                ));
            };
            previous_kind = line.as_bytes().first().copied();
            match line.as_bytes().first() {
                Some(b' ') => {
                    old.push_str(&line[1..]);
                    new.push_str(&line[1..]);
                }
                Some(b'-') => old.push_str(&line[1..]),
                Some(b'+') => new.push_str(&line[1..]),
                _ => {
                    return Err(ToolError::InvalidArgs(
                        "hunk lines must begin with space, + or -".into(),
                    ))
                }
            }
        }
        if let Some(h) = current {
            hunks.push(h);
        }
        if hunks.is_empty() {
            return Err(ToolError::InvalidArgs("patch contains no hunks".into()));
        }
        for (old, new) in &hunks {
            if old.is_empty() {
                if !text.is_empty() {
                    return Err(ToolError::InvalidArgs("an additions-only hunk can only create an empty/new file; provide context to insert".into()));
                }
                text = new.clone();
            } else {
                let matches = text.match_indices(old).map(|(i, _)| i).collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(ToolError::Other(format!("patch context matches {} locations; re-read file and provide unique context",matches.len())));
                }
                text.replace_range(matches[0]..matches[0] + old.len(), new);
            }
        }
        let mut out = crate::fs_tools::WriteTool
            .run(
                json!({"path":path,"content":text,"expect_sha256":hash}),
                ctx,
            )
            .await?;
        out.structured["tool"] = json!("apply_patch");
        out.text = format!("applied {} hunk(s): {}", hunks.len(), out.text);
        Ok(out)
    }
}
