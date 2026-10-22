//! Search tools: glob, grep (gitignore-aware walk), tree.

use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

// ─── glob ────────────────────────────────────────────────────────────────────────

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "List files matching a glob pattern (relative to the workspace root, e.g. \"src/**/*.rs\"). Results are sorted and capped.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"pattern": {"type": "string", "minLength": 1}},
                "required": ["pattern"],
                "additionalProperties": false
            }),
            effects: Effects::ReadOnly,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let pattern = args["pattern"].as_str().unwrap_or_default();
        let matcher = glob::Pattern::new(pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad glob pattern: {e}")))?;
        let options = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        let mut builder = ignore::WalkBuilder::new(ctx.ws.root());
        builder
            .hidden(false)
            .require_git(false)
            .git_global(false)
            .filter_entry(|e| e.file_name() != ".git");
        let mut hits: Vec<String> = builder
            .build()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| {
                let rel = e.path().strip_prefix(ctx.ws.root()).ok()?;
                matcher
                    .matches_path_with(rel, options)
                    .then(|| rel.to_string_lossy().to_string())
            })
            .collect();
        hits.sort();
        let cap = 1000;
        let truncated = hits.len() > cap;
        let body = hits
            .iter()
            .take(cap)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let note = if truncated {
            format!("\n[{} of {} matches shown]", cap, hits.len())
        } else {
            String::new()
        };
        let (excerpt, full_payload, trunc) = ctx.bound(format!("{body}{note}"));
        Ok(ToolOutput {
            ok: true,
            text: excerpt,
            full: full_payload,
            truncated: trunc,
            structured: json!({"count": hits.len(), "truncated": truncated}),
        })
    }
}

// ─── grep ────────────────────────────────────────────────────────────────────────

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Regex search across the workspace (respects .gitignore, skips .git and binaries). Returns file:line: text matches.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "minLength": 1},
                    "path": {"type": "string", "description": "Subdirectory to search; defaults to the workspace root."},
                    "glob": {"type": "string", "description": "Filename glob filter, e.g. \"*.rs\"."},
                    "max_results": {"type": "integer", "minimum": 1}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            effects: Effects::ReadOnly,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let pattern = args["pattern"].as_str().unwrap_or_default();
        let re = regex::Regex::new(pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad regex: {e}")))?;
        let base = match args.get("path").and_then(Value::as_str) {
            Some(p) => ctx.ws.resolve(p)?,
            None => ctx.ws.root().to_path_buf(),
        };
        let glob_filter = args.get("glob").and_then(Value::as_str).map(str::to_string);
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(200) as usize;

        let mut out = String::new();
        let mut count = 0usize;
        let mut files_scanned = 0usize;
        let mut binaries_skipped = 0usize;
        let mut truncated = false;
        let mut builder = ignore::WalkBuilder::new(&base);
        builder
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .require_git(false)
            .filter_entry(|e| e.file_name() != ".git");
        for entry in builder.build().flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            if let Some(gf) = &glob_filter {
                let name = entry.file_name().to_string_lossy();
                let matched = glob::Pattern::new(gf)
                    .map(|p| p.matches(&name))
                    .unwrap_or(false);
                if !matched {
                    continue;
                }
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            if bytes[..bytes.len().min(4096)].contains(&0) {
                binaries_skipped += 1;
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            files_scanned += 1;
            let rel = Path::new(entry.path())
                .strip_prefix(ctx.ws.root())
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    count += 1;
                    if count <= max_results {
                        out.push_str(&format!("{rel}:{}: {}\n", i + 1, line.trim_end()));
                    } else {
                        truncated = true;
                    }
                }
            }
            if count > max_results && out.len() > ctx.output_limit {
                break; // already truncating; stop scanning
            }
        }
        if truncated {
            out.push_str(&format!("\n[{count} matches; first {max_results} shown]\n"));
        }
        out.push_str(&format!(
            "[{files_scanned} text files scanned, {binaries_skipped} binary skipped]\n"
        ));
        let (excerpt, full, trunc) = ctx.bound(out);
        Ok(ToolOutput {
            ok: true,
            text: excerpt,
            full,
            truncated: trunc,
            structured: json!({"matches": count, "files_scanned": files_scanned, "truncated": truncated}),
        })
    }
}

// ─── tree ────────────────────────────────────────────────────────────────────────

pub struct TreeTool;

#[async_trait]
impl Tool for TreeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "tree".into(),
            description:
                "Compact directory listing with types and sizes (does not follow symlinks).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "depth": {"type": "integer", "minimum": 1},
                    "max_entries": {"type": "integer", "minimum": 1}
                },
                "additionalProperties": false
            }),
            effects: Effects::ReadOnly,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let base = match args.get("path").and_then(Value::as_str) {
            Some(p) => ctx.ws.resolve(p)?,
            None => ctx.ws.root().to_path_buf(),
        };
        let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(3) as usize;
        let max_entries = args
            .get("max_entries")
            .and_then(Value::as_u64)
            .unwrap_or(500) as usize;
        let mut out = String::new();
        let mut count = 0usize;
        fn walk(
            dir: &Path,
            rel: &str,
            depth: usize,
            max_entries: usize,
            out: &mut String,
            count: &mut usize,
        ) -> std::io::Result<()> {
            if depth == 0 || *count >= max_entries {
                return Ok(());
            }
            let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                if *count >= max_entries {
                    out.push_str("[entry cap reached]\n");
                    return Ok(());
                }
                let ft = e.file_type()?;
                let name = e.file_name().to_string_lossy().to_string();
                if ft.is_dir()
                    && [".git", "node_modules", "target", ".venv", "__pycache__"]
                        .contains(&name.as_str())
                {
                    continue;
                }
                let r = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                if ft.is_dir() {
                    out.push_str(&format!("{r}/\n"));
                    *count += 1;
                    walk(&e.path(), &r, depth - 1, max_entries, out, count)?;
                } else if ft.is_symlink() {
                    let target = std::fs::read_link(e.path())
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    out.push_str(&format!("{r} → {target}\n"));
                    *count += 1;
                } else {
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    out.push_str(&format!("{r} ({} bytes)\n", size));
                    *count += 1;
                }
            }
            Ok(())
        }
        walk(&base, "", depth, max_entries, &mut out, &mut count).map_err(ToolError::Io)?;
        let (excerpt, full, trunc) = ctx.bound(out);
        Ok(ToolOutput {
            ok: true,
            text: excerpt,
            full,
            truncated: trunc,
            structured: json!({"entries": count}),
        })
    }
}
