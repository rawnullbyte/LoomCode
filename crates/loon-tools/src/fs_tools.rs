//! File tools: read, write, edit (exact), fuzzy_edit (whitespace-normalized), multi_edit.
//!
//! Editing discipline enforced here:
//! - edits are exact/normalized substring replacements with ambiguity refusal,
//! - every mutation goes through the workspace CAS (`expect_sha256` optional but recorded),
//! - multi_edit is all-or-nothing (abort leaves the file untouched),
//! - invalid UTF-8 is rejected rather than silently re-encoded.

use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};

fn param_path() -> Value {
    json!({"type": "string", "description": "Workspace-relative or absolute path."})
}

fn hash_note(rec: &loon_workspace::MutationRecord) -> String {
    format!(
        "(sha256 {} → {})",
        &rec.before_sha256[..8],
        &rec.after_sha256[..8]
    )
}

// ─── read ────────────────────────────────────────────────────────────────────────

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a text file with line numbers. Returns sha256 of full content — pass it as expect_sha256 to mutating tools to protect against concurrent edits. Binary files are detected and not dumped.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": param_path(),
                    "offset": {"type": "integer", "description": "1-based first line to read.", "minimum": 1},
                    "limit": {"type": "integer", "description": "Max lines to read.", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            effects: Effects::ReadOnly,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("path required".into()))?;
        let p = ctx.ws.resolve(path)?;
        let (bytes, hash) = ctx.ws.read_file(&p)?;
        // binary sniff: NUL in the first 8 KiB
        let sniff = &bytes[..bytes.len().min(8192)];
        if sniff.contains(&0) {
            return Ok(ToolOutput::ok(format!(
                "[binary file: {} bytes, sha256 {hash} — not rendered; use exec tools for binary inspection]",
                bytes.len()
            )));
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => {
                return Ok(ToolOutput::ok(format!(
                "[file is not valid UTF-8: {} bytes, sha256 {hash}; refusing to decode lossily]",
                bytes.len()
            )))
            }
        };
        let total_lines = text.lines().count().max(1);
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(2000);
        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        }
        let shown_end = (offset - 1 + limit).min(total_lines);
        out.push_str(&format!(
            "\n[sha256 {hash} | lines {offset}–{shown_end} of {total_lines} | {} bytes]\n",
            bytes.len()
        ));
        let (excerpt, full, truncated) = ctx.bound(out);
        Ok(ToolOutput {
            ok: true,
            text: excerpt,
            full,
            truncated,
            structured: json!({"sha256": hash, "total_lines": total_lines, "path": path}),
        })
    }
}

// ─── write ───────────────────────────────────────────────────────────────────────

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Create or fully replace a file. Pass expect_sha256 (from read) to refuse overwriting concurrent changes; omit to assert without check.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": param_path(),
                    "content": {"type": "string"},
                    "expect_sha256": {"type": "string", "description": "Hash from read; write is refused if the file changed since."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            effects: Effects::Mutating,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().unwrap_or_default();
        let content = args["content"].as_str().unwrap_or_default();
        let expect = args
            .get("expect_sha256")
            .and_then(Value::as_str)
            .map(str::to_string);
        let p = ctx.ws.resolve(path)?;
        let prior = std::fs::read(&p).ok();
        let before_blob = prior.as_deref().and_then(|b| snap(ctx, b));
        let g = ctx.ws.write_lock();
        let rec = ctx
            .ws
            .cas_write(&p, content.as_bytes(), expect.as_deref(), "write", &g)?;
        let after_blob = snap(ctx, content.as_bytes());
        Ok(ToolOutput {
            ok: true,
            text: format!(
                "wrote {} ({} bytes) {}",
                p.display(),
                content.len(),
                hash_note(&rec)
            ),
            full: None,
            truncated: false,
            structured: attach_mutation_fields(
                serde_json::to_value(&rec).unwrap_or(Value::Null),
                before_blob,
                after_blob,
            ),
        })
    }
}

// ─── shared edit machinery ───────────────────────────────────────────────────────

fn line_of_offset(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count() + 1
}

/// Whitespace-normalized view of `s` plus, for each byte of the normalized string, the
/// original byte offset it came from.
fn normalize_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    let mut in_ws = false;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                map.push(i);
                in_ws = true;
            }
        } else {
            out.push(ch);
            map.resize(map.len() + ch.len_utf8(), i);
            in_ws = false;
        }
    }
    (out, map)
}

#[derive(Debug)]
enum Match {
    Exact(usize),
    Fuzzy(usize, usize), // original span
}

/// Find `old` in `text`; exact first, then whitespace-normalized. Refuses ambiguity.
fn find_match(text: &str, old: &str) -> Result<Match, ToolError> {
    let occurrences: Vec<usize> = text.match_indices(old).map(|(i, _)| i).collect();
    match occurrences.as_slice() {
        [only] => return Ok(Match::Exact(*only)),
        [..] if occurrences.len() > 1 => {
            let lines: Vec<String> = occurrences
                .iter()
                .map(|&i| format!("line {}", line_of_offset(text, i)))
                .collect();
            return Err(ToolError::Other(format!(
                "old_string matches {} locations ({}); pass replace_all=true or include more surrounding context to disambiguate",
                occurrences.len(),
                lines.join(", ")
            )));
        }
        _ => {}
    }
    // fuzzy pass
    let (norm_text, map) = normalize_with_map(text);
    let (norm_old, _) = normalize_with_map(old);
    if norm_old.is_empty() {
        return Err(ToolError::Other("old_string is empty".into()));
    }
    let norm_matches: Vec<usize> = norm_text.match_indices(&norm_old).map(|(i, _)| i).collect();
    match norm_matches.as_slice() {
        [only] => {
            let s = *only;
            let e = s + norm_old.len();
            let orig_start = map.get(s).copied().unwrap_or(0);
            let orig_end = map.get(e).copied().unwrap_or(text.len());
            Ok(Match::Fuzzy(orig_start, orig_end))
        }
        [] => Err(ToolError::Other(
            "old_string not found (exact or whitespace-normalized). Re-read the file and copy the text exactly, or use write for a full replacement.".into(),
        )),
        [..] => {
            let lines: Vec<String> = norm_matches
                .iter()
                .map(|&i| format!("line {}", line_of_offset(text, map.get(i).copied().unwrap_or(0))))
                .collect();
            Err(ToolError::Other(format!(
                "whitespace-normalized old_string matches {} locations ({}); add context to disambiguate",
                norm_matches.len(),
                lines.join(", ")
            )))
        }
    }
}

fn apply_one(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, bool), ToolError> {
    if old == new {
        return Err(ToolError::Other(
            "old_string and new_string are identical; nothing to do".into(),
        ));
    }
    if replace_all {
        if text.contains(old) {
            return Ok((text.replace(old, new), false));
        }
        // fall through to fuzzy for a single normalized match? No: replace_all with a
        // fuzzy span is ambiguous by definition — refuse.
        return Err(ToolError::Other(
            "old_string not found for replace_all (exact match required)".into(),
        ));
    }
    match find_match(text, old)? {
        Match::Exact(i) => {
            let mut s = String::with_capacity(text.len() + new.len());
            s.push_str(&text[..i]);
            s.push_str(new);
            s.push_str(&text[i + old.len()..]);
            Ok((s, true))
        }
        Match::Fuzzy(a, b) => {
            let mut s = String::with_capacity(text.len() + new.len());
            s.push_str(&text[..a]);
            s.push_str(new);
            s.push_str(&text[b..]);
            Ok((s, true))
        }
    }
}

fn decode_utf8(bytes: &[u8], path: &str) -> Result<String, ToolError> {
    std::str::from_utf8(bytes).map(str::to_string).map_err(|_| {
        ToolError::Other(format!(
            "{path} is not valid UTF-8; refusing to edit (unsupported encoding)"
        ))
    })
}

// ─── edit ────────────────────────────────────────────────────────────────────────

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Replace one exact occurrence of old_string with new_string in a file. Falls back to a single whitespace-normalized match. Ambiguous or missing matches are refused with locations.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": param_path(),
                    "old_string": {"type": "string", "minLength": 1},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"},
                    "expect_sha256": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
            effects: Effects::Mutating,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        run_edit(args, ctx, false).await
    }
}

async fn run_edit(args: Value, ctx: &ToolCtx, multi: bool) -> Result<ToolOutput, ToolError> {
    let _ = multi;
    let path = args["path"].as_str().unwrap_or_default();
    let old = args["old_string"].as_str().unwrap_or_default();
    let new = args["new_string"].as_str().unwrap_or_default();
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expect = args
        .get("expect_sha256")
        .and_then(Value::as_str)
        .map(str::to_string);
    let p = ctx.ws.resolve(path)?;
    let (bytes, _) = ctx.ws.read_file(&p)?;
    let text = decode_utf8(&bytes, path)?;
    let (next, _matched) = apply_one(&text, old, new, replace_all)?;
    let g = ctx.ws.write_lock();
    let cas = expect.unwrap_or_else(|| bytes_hash(&bytes));
    let before_blob = snap(ctx, &bytes);
    let rec = ctx
        .ws
        .cas_write(&p, next.as_bytes(), Some(cas.as_str()), "edit", &g)?;
    let after_blob = snap(ctx, next.as_bytes());
    Ok(ToolOutput {
        ok: true,
        text: format!(
            "edited {}{} {}",
            p.display(),
            if replace_all {
                " (replace_all)"
            } else if matched_exact_span(&text, old) {
                ""
            } else {
                " (whitespace-normalized match)"
            },
            hash_note(&rec)
        ),
        full: None,
        truncated: false,
        structured: attach_mutation_fields(
            serde_json::to_value(&rec).unwrap_or(Value::Null),
            before_blob,
            after_blob,
        ),
    })
}

fn matched_exact_span(text: &str, old: &str) -> bool {
    let occurrences = text.matches(old).count();
    occurrences == 1
}

fn bytes_hash(b: &[u8]) -> String {
    loon_workspace::sha256_hex(b)
}

/// Snapshot `bytes` through the ctx sink (if any) for undo/redo; returns the blob sha.
fn snap(ctx: &ToolCtx, bytes: &[u8]) -> Option<String> {
    ctx.snapshot(bytes).map(|b| b.sha256)
}

fn attach_mutation_fields(
    mut structured: serde_json::Value,
    before_blob: Option<String>,
    after_blob: Option<String>,
) -> serde_json::Value {
    if let Some(b) = before_blob {
        structured["before_blob_sha256"] = serde_json::json!(b);
    }
    if let Some(a) = after_blob {
        structured["after_blob_sha256"] = serde_json::json!(a);
    }
    structured
}

// ─── fuzzy_edit ──────────────────────────────────────────────────────────────────

pub struct FuzzyEditTool;

#[async_trait]
impl Tool for FuzzyEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fuzzy_edit".into(),
            description: "Whitespace-insensitive single replacement. Matches exactly one region after collapsing whitespace; multiple candidates are reported, never guessed.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": param_path(),
                    "old_string": {"type": "string", "minLength": 1},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
            effects: Effects::Mutating,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        run_edit(args, ctx, true).await
    }
}

// ─── multi_edit ──────────────────────────────────────────────────────────────────

pub struct MultiEditTool;

#[async_trait]
impl Tool for MultiEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "multi_edit".into(),
            description: "Apply several replacements to one file as a single transaction: if any fails, nothing is written.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": param_path(),
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string", "minLength": 1},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            },
                            "required": ["old_string", "new_string"],
                            "additionalProperties": false
                        }
                    },
                    "expect_sha256": {"type": "string"}
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
            effects: Effects::Mutating,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().unwrap_or_default();
        let expect = args
            .get("expect_sha256")
            .and_then(Value::as_str)
            .map(str::to_string);
        let p = ctx.ws.resolve(path)?;
        let (bytes, _) = ctx.ws.read_file(&p)?;
        let mut text = decode_utf8(&bytes, path)?;
        let edits = args["edits"].as_array().cloned().unwrap_or_default();
        let mut applied = 0usize;
        for e in &edits {
            let old = e["old_string"].as_str().unwrap_or_default();
            let new = e["new_string"].as_str().unwrap_or_default();
            let ra = e
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (next, _) = apply_one(&text, old, new, ra)?;
            text = next;
            applied += 1;
        }
        let g = ctx.ws.write_lock();
        let cas = expect.unwrap_or_else(|| bytes_hash(&bytes));
        let before_blob = snap(ctx, &bytes);
        let rec = ctx
            .ws
            .cas_write(&p, text.as_bytes(), Some(cas.as_str()), "multi_edit", &g)?;
        let after_blob = snap(ctx, text.as_bytes());
        Ok(ToolOutput {
            ok: true,
            text: format!(
                "multi_edit: {applied} replacement(s) applied to {} {}",
                p.display(),
                hash_note(&rec)
            ),
            full: None,
            truncated: false,
            structured: attach_mutation_fields(
                serde_json::to_value(&rec).unwrap_or(Value::Null),
                before_blob,
                after_blob,
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_ambiguity_refused_with_lines() {
        let text = "alpha\nbeta\nalpha\n";
        let err = find_match(text, "alpha").unwrap_err();
        assert!(err.to_string().contains("2 locations"));
        assert!(err.to_string().contains("line 1"));
        assert!(err.to_string().contains("line 3"));
    }

    #[test]
    fn fuzzy_matches_collapsed_whitespace_uniquely() {
        let text = "fn  main() {\n    let x = 1;\n}";
        let old = "fn main() {\n  let x = 1;\n}";
        match find_match(text, old).unwrap() {
            Match::Fuzzy(a, b) => {
                assert_eq!(&text[a..b], "fn  main() {\n    let x = 1;\n}");
            }
            other => panic!("expected fuzzy, got {other:?}"),
        }
    }

    #[test]
    fn fuzzy_ambiguity_refused() {
        let text = "let   x = 1;\nlet  x = 1;\n";
        assert!(find_match(text, "let x = 1;").is_err());
    }

    #[test]
    fn apply_all_replaces_every_occurrence() {
        let (out, _) = apply_one("a-b-a", "a", "z", true).unwrap();
        assert_eq!(out, "z-b-z");
    }
}
