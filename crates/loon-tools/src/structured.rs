//! Hash-protected structured edits reuse the normal write tool and its snapshots.
use crate::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
pub struct JsonEditTool;
pub struct NotebookEditTool;
async fn save(
    ctx: &ToolCtx,
    path: &str,
    original_sha: &str,
    expected: Option<&str>,
    value: &Value,
) -> Result<ToolOutput, ToolError> {
    if expected.is_some_and(|sha| sha != original_sha) {
        return Err(ToolError::InvalidArgs(
            "stale content hash; read the file again before editing".into(),
        ));
    }
    let content =
        serde_json::to_string_pretty(value).map_err(|e| ToolError::Other(e.to_string()))? + "\n";
    crate::fs_tools::WriteTool
        .run(
            json!({"path":path,"content":content,"expect_sha256":original_sha}),
            ctx,
        )
        .await
}
fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args[key]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArgs(format!("{key} required")))
}
#[async_trait]
impl Tool for JsonEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec{name:"json_edit".into(),description:"Set, remove or append a JSON value using an RFC 6901 pointer. Parent containers must exist. Rewrites formatted JSON with hash protection and normal undo snapshots. Use expect_sha256 from read_json to reject stale edits.".into(),effects:Effects::Mutating,parameters:json!({"type":"object","properties":{"path":{"type":"string"},"pointer":{"type":"string"},"action":{"type":"string","enum":["set","remove","append"]},"value":{},"expect_sha256":{"type":"string"}},"required":["path","pointer","action"],"additionalProperties":false})}
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = required(&args, "path")?;
        let pointer = required(&args, "pointer")?;
        let action = required(&args, "action")?;
        let (mut value, sha) = crate::inspect::read_json(ctx, path)?;
        if action == "append" {
            let target = value
                .pointer_mut(pointer)
                .and_then(Value::as_array_mut)
                .ok_or_else(|| {
                    ToolError::InvalidArgs("append requires an existing array pointer".into())
                })?;
            target.push(
                args.get("value")
                    .cloned()
                    .ok_or_else(|| ToolError::InvalidArgs("value required".into()))?,
            );
        } else if pointer.is_empty() {
            if action != "set" {
                return Err(ToolError::InvalidArgs(
                    "root pointer supports set or append".into(),
                ));
            }
            value = args
                .get("value")
                .cloned()
                .ok_or_else(|| ToolError::InvalidArgs("value required".into()))?;
        } else {
            let (parent, key) = pointer
                .rsplit_once('/')
                .ok_or_else(|| ToolError::InvalidArgs("pointer must start with /".into()))?;
            // Reject malformed escapes rather than silently editing a different key.
            let mut chars = key.chars();
            while let Some(c) = chars.next() {
                if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
                    return Err(ToolError::InvalidArgs("invalid JSON pointer escape".into()));
                }
            }
            let key = key.replace("~1", "/").replace("~0", "~");
            let target = value
                .pointer_mut(parent)
                .ok_or_else(|| ToolError::InvalidArgs("parent pointer not found".into()))?;
            match target {
                Value::Object(map) => match action {
                    "set" => {
                        map.insert(
                            key,
                            args.get("value")
                                .cloned()
                                .ok_or_else(|| ToolError::InvalidArgs("value required".into()))?,
                        );
                    }
                    "remove" => {
                        if map.remove(&key).is_none() {
                            return Err(ToolError::InvalidArgs("key not found".into()));
                        }
                    }
                    _ => {
                        return Err(ToolError::InvalidArgs(
                            "action must be set/remove/append".into(),
                        ))
                    }
                },
                Value::Array(array) => {
                    let i = key.parse::<usize>().map_err(|_| {
                        ToolError::InvalidArgs("array pointer requires an index".into())
                    })?;
                    if i >= array.len() {
                        return Err(ToolError::InvalidArgs(
                            "array index out of range; use append to add".into(),
                        ));
                    }
                    match action {
                        "set" => {
                            array[i] = args
                                .get("value")
                                .cloned()
                                .ok_or_else(|| ToolError::InvalidArgs("value required".into()))?
                        }
                        "remove" => {
                            array.remove(i);
                        }
                        _ => return Err(ToolError::InvalidArgs("invalid action".into())),
                    }
                }
                _ => {
                    return Err(ToolError::InvalidArgs(
                        "parent is not an object or array".into(),
                    ))
                }
            }
        }
        save(ctx, path, &sha, args["expect_sha256"].as_str(), &value).await
    }
}
#[async_trait]
impl Tool for NotebookEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec{name:"notebook_edit".into(),description:"Insert, replace or delete a zero-based Jupyter notebook cell. Editing code clears stale outputs/execution_count. Preserves notebook metadata, records undo snapshots and supports expect_sha256 from notebook_read.".into(),effects:Effects::Mutating,parameters:json!({"type":"object","properties":{"path":{"type":"string"},"cell":{"type":"integer","minimum":0},"action":{"type":"string","enum":["insert","replace","delete"]},"source":{"type":"string"},"cell_type":{"type":"string","enum":["code","markdown","raw"]},"expect_sha256":{"type":"string"}},"required":["path","cell","action"],"additionalProperties":false})}
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = required(&args, "path")?;
        let action = required(&args, "action")?;
        let index = args["cell"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidArgs("cell required".into()))?
            as usize;
        let (mut value, sha) = crate::inspect::read_json(ctx, path)?;
        let minor = value["nbformat_minor"].as_u64().unwrap_or(0);
        let cells = value["cells"]
            .as_array_mut()
            .ok_or_else(|| ToolError::InvalidArgs("notebook requires cells array".into()))?;
        if cells.iter().any(|cell| !cell.is_object()) {
            return Err(ToolError::InvalidArgs(
                "notebook cells must be objects".into(),
            ));
        }
        if index > cells.len() || (action != "insert" && index == cells.len()) {
            return Err(ToolError::InvalidArgs("cell index out of range".into()));
        }
        if action == "delete" {
            cells.remove(index);
        } else if ["insert", "replace"].contains(&action) {
            let source = required(&args, "source")?;
            let kind = args["cell_type"]
                .as_str()
                .or_else(|| cells.get(index).and_then(|c| c["cell_type"].as_str()))
                .unwrap_or("code");
            if !["code", "markdown", "raw"].contains(&kind) {
                return Err(ToolError::InvalidArgs("invalid cell_type".into()));
            }
            let mut cell = if action == "replace" {
                cells[index].clone()
            } else {
                json!({"metadata":{}})
            };
            cell["cell_type"] = json!(kind);
            cell["source"] = json!(source.split_inclusive('\n').collect::<Vec<_>>());
            if minor >= 5 && cell.get("id").is_none() {
                let id = loon_workspace::sha256_hex(format!("{sha}:{index}:{source}").as_bytes());
                cell["id"] = json!(&id[..16]);
            }
            if kind == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
                cell.as_object_mut().unwrap().remove("attachments");
            } else if let Some(obj) = cell.as_object_mut() {
                obj.remove("outputs");
                obj.remove("execution_count");
            }
            if action == "insert" {
                cells.insert(index, cell);
            } else {
                cells[index] = cell;
            }
        } else {
            return Err(ToolError::InvalidArgs("invalid action".into()));
        }
        save(ctx, path, &sha, args["expect_sha256"].as_str(), &value).await
    }
}
