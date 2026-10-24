//! Bounded repository/file inspection without shell pipelines.
use crate::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
#[derive(Clone, Copy)]
pub enum Kind {
    Directory,
    Info,
    Many,
    Symbol,
    Project,
    Json,
    Notebook,
    Compare,
    Diagnose,
}
pub struct InspectTool(pub Kind);
pub(crate) fn bounded(ctx: &ToolCtx, text: String) -> ToolOutput {
    let (text, full, truncated) = ctx.bound(text);
    ToolOutput {
        ok: true,
        text,
        full,
        truncated,
        structured: Value::Null,
    }
}
pub(crate) fn read_bytes(path: &Path) -> Result<Vec<u8>, ToolError> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(ToolError::InvalidArgs(
            "file exceeds 8 MiB; use bounded read/grep instead".into(),
        ));
    }
    Ok(bytes)
}
pub(crate) fn read_json(ctx: &ToolCtx, path: &str) -> Result<(Value, String), ToolError> {
    let bytes = read_bytes(&ctx.ws.resolve(path)?)?;
    let sha = loon_workspace::sha256_hex(&bytes);
    let value = serde_json::from_slice(&bytes)
        .map_err(|e| ToolError::InvalidArgs(format!("invalid JSON: {e}")))?;
    Ok((value, sha))
}
fn read_text(path: &Path) -> Result<String, ToolError> {
    String::from_utf8(read_bytes(path)?)
        .map_err(|_| ToolError::InvalidArgs("file is not UTF-8 text".into()))
}

/// JSON syntax errors with line:column, computed from the parser's byte offset.
fn json_diagnostics(text: &str) -> Vec<serde_json::Value> {
    let Err(e) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    // serde_json reports `line L column C` (1-based line, 1-based column) in its message;
    // parse those out so the shape matches the Rust diagnostics.
    let msg = e.to_string();
    let mut line = 1usize;
    let mut column = 0usize;
    if let Some(rest) = msg.split("line ").nth(1) {
        let mut parts = rest.split(|c: char| !c.is_ascii_digit());
        if let Some(l) = parts.next().and_then(|p| p.parse::<usize>().ok()) {
            line = l;
            if let Some(c) = rest.split("column ").nth(1).and_then(|r| {
                r.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<usize>()
                    .ok()
            }) {
                column = c.saturating_sub(1);
            }
        }
    }
    vec![serde_json::json!({"line": line, "column": column, "message": msg})]
}

/// TOML syntax errors with line:column, same convention.
fn toml_diagnostics(text: &str) -> Vec<serde_json::Value> {
    let Err(e) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let msg = e.to_string();
    // toml reports errors as `line: column` (1-based) when a span is known.
    let mut line = 1usize;
    let mut column = 0usize;
    if let Some(rest) = msg.split(" line ").nth(1) {
        let mut parts = rest.split(|c: char| !c.is_ascii_digit());
        if let Some(l) = parts.next().and_then(|p| p.parse::<usize>().ok()) {
            line = l;
            if let Some(c) = rest.split(" column ").nth(1).and_then(|r| {
                r.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<usize>()
                    .ok()
            }) {
                column = c.saturating_sub(1);
            }
        }
    }
    vec![serde_json::json!({"line": line, "column": column, "message": msg})]
}
fn paths(ctx: &ToolCtx, path: &str) -> Result<Vec<PathBuf>, ToolError> {
    let root = ctx.ws.resolve(path)?;
    Ok(ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .git_global(false)
        .max_depth(Some(25))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .take(10000)
        .collect())
}
/// Fast project inventory for `project_info`: bounded and gitignore-aware.
/// Heavy generated trees are pruned structurally before descent, and `target/` is cut
/// at the entry level regardless of gitignore so build artifacts never dominate a scan.
fn fast_inventory(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .git_global(false)
        .parents(true)
        .filter_entry(|e| match e.file_name().to_str() {
            Some(name) => {
                ![".git", "target", "node_modules", ".venv", "dist", "build"].contains(&name)
            }
            None => true,
        })
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .take(10_000)
        .collect();
    files.sort();
    files
}
#[async_trait]
impl Tool for InspectTool {
    fn spec(&self) -> ToolSpec {
        let(name,description,properties,required)=match self.0 {
   Kind::Directory=>("list_directory","List one directory with entry types and sizes; optional hidden files. Does not recursively traverse or follow symlink entries.",json!({"path":{"type":"string"},"hidden":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":1000}}),vec![]),
   Kind::Info=>("file_info","Inspect file metadata, size, type, modification time and SHA-256 for files up to 8 MiB.",json!({"path":{"type":"string"}}),vec!["path"]),
   Kind::Many=>("read_many","Read bounded line ranges from up to 20 UTF-8 files in one tool call. Reports individual errors and content hashes.",json!({"paths":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"string"}},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":500}}),vec!["paths"]),
   Kind::Symbol=>("find_symbol","Find where a symbol is declared and referenced. Rust files are parsed in-process, so matches carry kind, column, visibility and container (definitions) plus reference rows — no language server needed. Other languages fall back to a declaration-keyword heuristic. Honors ignore files.",json!({"symbol":{"type":"string","minLength":1,"maxLength":200},"path":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":200}}),vec!["symbol"]),
   Kind::Diagnose=>("diagnostics","Report syntax errors in source files, with line and column, computed in-process (Rust via syn; JSON and TOML via their parsers). An empty list means the files parse. Run it after edits to catch breakage before running tests.",json!({"path":{"type":"string"}}),vec![]),
   Kind::Project=>("project_info","Detect project manifests, package scripts and a bounded source inventory before choosing build/test/lint commands. Honors ignore files.",json!({"path":{"type":"string"}}),vec![]),
   Kind::Json=>("read_json","Read a JSON file or RFC 6901 JSON pointer with its original content hash. Preserve the hash when making an edit.",json!({"path":{"type":"string"},"pointer":{"type":"string"}}),vec!["path"]),
   Kind::Notebook=>("notebook_read","Inspect Jupyter notebook cells, source and output counts without flooding context with embedded images. Select one zero-based cell or all cells.",json!({"path":{"type":"string"},"cell":{"type":"integer","minimum":0}}),vec!["path"]),
   Kind::Compare=>("file_compare","Compare two UTF-8 files and return a single valid unified diff hunk plus content hashes. Does not change either file.",json!({"left":{"type":"string"},"right":{"type":"string"}}),vec!["left","right"]),
  };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            effects: Effects::ReadOnly,
        }
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().unwrap_or(".");
        let text = match self.0 {
            Kind::Directory => {
                let mut entries = Vec::new();
                let mut clipped = false;
                for e in std::fs::read_dir(ctx.ws.resolve(path)?)? {
                    let e = e?;
                    if !args["hidden"].as_bool().unwrap_or(false)
                        && e.file_name().to_string_lossy().starts_with('.')
                    {
                        continue;
                    }
                    if entries.len() >= 10000 {
                        clipped = true;
                        break;
                    }
                    let meta = e.path().symlink_metadata()?;
                    entries.push(json!({"name":e.file_name().to_string_lossy(),"type":if meta.is_symlink(){"symlink"}else if meta.is_dir(){"directory"}else{"file"},"bytes":meta.len()}));
                }
                entries.sort_by_key(|e| e["name"].as_str().unwrap_or("").to_lowercase());
                let limit = args["limit"].as_u64().unwrap_or(200).clamp(1, 1000) as usize;
                clipped |= entries.len() > limit;
                entries.truncate(limit);
                json!({"path":path,"entries":entries,"truncated":clipped}).to_string()
            }
            Kind::Info => {
                let p = ctx.ws.resolve(path)?;
                let m = p.metadata()?;
                let sha = if m.is_file() && m.len() <= 8 * 1024 * 1024 {
                    Some(loon_workspace::sha256_hex(&read_bytes(&p)?))
                } else {
                    None
                };
                json!({"path":p,"type":if m.is_dir(){"directory"}else{"file"},"bytes":m.len(),"sha256":sha,"modified_unix":m.modified().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|t|t.as_secs()),"readonly":m.permissions().readonly()}).to_string()
            }
            Kind::Many => {
                let inputs = args["paths"]
                    .as_array()
                    .ok_or_else(|| ToolError::InvalidArgs("paths required".into()))?;
                if inputs.is_empty() || inputs.len() > 20 {
                    return Err(ToolError::InvalidArgs("provide 1–20 paths".into()));
                }
                let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
                let limit = args["limit"].as_u64().unwrap_or(120).clamp(1, 500) as usize;
                let rows:Vec<_>=inputs.iter().map(|value|{let path=value.as_str().unwrap_or("");match ctx.ws.resolve(path).map_err(ToolError::from).and_then(|p|read_text(&p)) {
     Ok(text)=>json!({"path":path,"sha256":loon_workspace::sha256_hex(text.as_bytes()),"offset":offset,"lines":text.lines().skip(offset-1).take(limit).enumerate().map(|(i,line)|format!("{}: {}",offset+i,line)).collect::<Vec<_>>()}),Err(e)=>json!({"path":path,"error":e.to_string()})}}).collect();
                json!(rows).to_string()
            }
            Kind::Symbol => {
                let symbol = args["symbol"].as_str().unwrap_or("");
                if symbol.is_empty() {
                    return Err(ToolError::InvalidArgs("symbol required".into()));
                }
                let needle = regex::Regex::new(&format!(r"\b{}\b", regex::escape(symbol)))?;
                let declaration = regex::Regex::new(
                    r"\b(fn|def|class|struct|enum|trait|interface|function|type|const|let|var|func|impl|typedef)\b",
                )?;
                let max = args["limit"].as_u64().unwrap_or(60).clamp(1, 200) as usize;
                let mut matches = Vec::new();
                let mut parsed_files = 0usize;
                let mut heuristic_files = 0usize;
                'files: for p in paths(ctx, path)? {
                    // Rust files go through the in-process parser: real definition records
                    // with kind, column and doc, not a regex that can only guess.
                    let is_rust = p.extension().is_some_and(|e| e == "rs");
                    if is_rust {
                        if let Ok(text) = read_text(&p) {
                            let analysis = crate::lsp::analyse_rust(&text);
                            if analysis.parsed {
                                parsed_files += 1;
                                let hits: Vec<_> = analysis
                                    .symbols
                                    .iter()
                                    .filter(|s| s.name == symbol)
                                    .collect();
                                if !hits.is_empty() {
                                    for s in hits {
                                        matches.push(json!({
                                            "path": p,
                                            "line": s.line,
                                            "column": s.column,
                                            "end_line": s.end_line,
                                            "kind": s.kind.as_str(),
                                            "visibility": s.visibility,
                                            "definition": true,
                                            "doc": !s.doc.is_empty(),
                                            "container": s.container,
                                            "text": text.lines().nth(s.line - 1).map(|l| l.chars().take(400).collect::<String>()).unwrap_or_default(),
                                        }));
                                        if matches.len() >= max {
                                            break 'files;
                                        }
                                    }
                                }
                                // References in parsed files still come from the word scan
                                // below (fall through), flagged as references.
                                for (i, line) in text.lines().enumerate() {
                                    if needle.is_match(line)
                                        && !analysis
                                            .symbols
                                            .iter()
                                            .any(|s| s.name == symbol && s.line == i + 1)
                                        && (declaration.is_match(line)
                                            || line.contains(&format!("{symbol}(")))
                                    {
                                        matches.push(json!({"path":p,"line":i+1,"reference":true,"text":line.chars().take(400).collect::<String>()}));
                                        if matches.len() >= max {
                                            break 'files;
                                        }
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    heuristic_files += 1;
                    let Ok(text) = read_text(&p) else { continue };
                    for (i, line) in text.lines().enumerate() {
                        if needle.is_match(line)
                            && (declaration.is_match(line) || line.contains(&format!("{symbol}(")))
                        {
                            matches.push(json!({"path":p,"line":i+1,"text":line.chars().take(400).collect::<String>()}));
                            if matches.len() >= max {
                                break 'files;
                            }
                        }
                    }
                }
                // `heuristic: true` only when nothing was parsed, so a caller can tell real
                // symbol records from guesses. Mixed results are the honest middle case.
                json!({
                    "symbol": symbol,
                    "matches": matches,
                    "limit_reached": matches.len() >= max,
                    "heuristic": parsed_files == 0,
                    "parsed_files": parsed_files,
                    "heuristic_files": heuristic_files,
                })
                .to_string()
            }
            Kind::Project => {
                let root = ctx.ws.resolve(path)?;
                // Bounded, parallel, gitignore-aware inventory: heavy build artifacts
                // (target, node_modules, .git) are skipped structurally before the walk
                // so repeated project analysis stays fast on large trees.
                let started = std::time::Instant::now();
                let files = fast_inventory(&root);
                let mut manifests = Vec::new();
                for name in [
                    "Cargo.toml",
                    "package.json",
                    "pyproject.toml",
                    "requirements.txt",
                    "go.mod",
                    "Makefile",
                    "CMakeLists.txt",
                    "build.gradle",
                    "pom.xml",
                    "composer.json",
                    "Gemfile",
                ] {
                    let p = root.join(name);
                    if p.is_file() {
                        let body = read_text(&p).unwrap_or_default();
                        manifests.push(json!({"path":name,"content":body.chars().take(6000).collect::<String>()}));
                    }
                }
                let mut languages = std::collections::BTreeMap::<String, usize>::new();
                for p in &files {
                    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                        *languages.entry(ext.to_string()).or_default() += 1;
                    }
                }
                json!({"root":root,"manifests":manifests,"file_count_up_to_10000":files.len(),"extensions":languages,"scan_ms":started.elapsed().as_millis() as u64,"sample_paths":files.iter().take(60).map(|p|p.strip_prefix(&root).unwrap_or(p).to_string_lossy()).collect::<Vec<_>>()} ).to_string()
            }
            Kind::Json => {
                let (value, sha) = read_json(ctx, path)?;
                let pointer = args["pointer"].as_str().unwrap_or("");
                let selected = value.pointer(pointer).ok_or_else(|| {
                    ToolError::InvalidArgs(format!("JSON pointer not found: {pointer}"))
                })?;
                format!(
                    "sha256 {sha}\n{}",
                    serde_json::to_string_pretty(selected)
                        .map_err(|e| ToolError::Other(e.to_string()))?
                )
            }
            Kind::Notebook => {
                let (value, sha) = read_json(ctx, path)?;
                let cells = value["cells"].as_array().ok_or_else(|| {
                    ToolError::InvalidArgs("notebook requires a cells array".into())
                })?;
                let selected = args["cell"].as_u64().map(|n| n as usize);
                if selected.is_some_and(|n| n >= cells.len()) {
                    return Err(ToolError::InvalidArgs("cell index out of range".into()));
                }
                let rows:Vec<_>=cells.iter().enumerate().filter(|(i,_)|selected.is_none_or(|n|n==*i)).map(|(i,c)|json!({"cell":i,"type":c["cell_type"],"source":c["source"],"execution_count":c["execution_count"],"output_count":c["outputs"].as_array().map(Vec::len).unwrap_or(0)})).collect();
                json!({"path":path,"sha256":sha,"nbformat":value["nbformat"],"cells":rows})
                    .to_string()
            }
            Kind::Compare => {
                let left = args["left"].as_str().unwrap_or("");
                let right = args["right"].as_str().unwrap_or("");
                let a = read_text(&ctx.ws.resolve(left)?)?;
                let b = read_text(&ctx.ws.resolve(right)?)?;
                if a == b {
                    format!(
                        "identical · sha256 {}",
                        loon_workspace::sha256_hex(a.as_bytes())
                    )
                } else {
                    let x: Vec<_> = a.lines().collect();
                    let y: Vec<_> = b.lines().collect();
                    let newline_diff = a.ends_with('\n') != b.ends_with('\n');
                    let mut prefix = x.iter().zip(&y).take_while(|(a, b)| a == b).count();
                    if newline_diff && prefix == x.len() && prefix == y.len() {
                        prefix = prefix.saturating_sub(1);
                    }
                    let suffix = if newline_diff {
                        0
                    } else {
                        x[prefix..]
                            .iter()
                            .rev()
                            .zip(y[prefix..].iter().rev())
                            .take_while(|(a, b)| a == b)
                            .count()
                    };
                    let nx = x.len() - prefix - suffix;
                    let ny = y.len() - prefix - suffix;
                    let mut text = format!(
                        "--- {left}\n+++ {right}\n@@ -{},{} +{},{} @@\n",
                        if nx == 0 { prefix } else { prefix + 1 },
                        nx,
                        if ny == 0 { prefix } else { prefix + 1 },
                        ny
                    );
                    for (i, line) in x[prefix..x.len() - suffix].iter().enumerate() {
                        text.push_str(&format!("-{line}\n"));
                        if suffix == 0 && i + 1 == nx && !a.ends_with('\n') {
                            text.push_str("\\ No newline at end of file\n");
                        }
                    }
                    for (i, line) in y[prefix..y.len() - suffix].iter().enumerate() {
                        text.push_str(&format!("+{line}\n"));
                        if suffix == 0 && i + 1 == ny && !b.ends_with('\n') {
                            text.push_str("\\ No newline at end of file\n");
                        }
                    }
                    text
                }
            }
            Kind::Diagnose => {
                // Syntax errors across the selected files, computed in-process. An empty
                // result is a meaningful answer (the files parse), so it is reported as
                // such rather than as silence.
                let mut files = Vec::new();
                let mut error_count = 0usize;
                let mut scanned = 0usize;
                let mut skipped = 0usize;
                for p in paths(ctx, path)? {
                    let ext = p
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    let kind = match ext.as_str() {
                        "rs" => Some("rust"),
                        "json" | "jsonc" => Some("json"),
                        "toml" => Some("toml"),
                        _ => None,
                    };
                    let Some(kind) = kind else { continue };
                    let Ok(text) = read_text(&p) else {
                        skipped += 1;
                        continue;
                    };
                    scanned += 1;
                    let diagnostics: Vec<serde_json::Value> = match kind {
                        "rust" => crate::lsp::diagnostics_to_json(
                            &crate::lsp::analyse_rust(&text).diagnostics,
                        )
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                        "json" => json_diagnostics(&text),
                        "toml" => toml_diagnostics(&text),
                        _ => Vec::new(),
                    };
                    if !diagnostics.is_empty() {
                        error_count += diagnostics.len();
                        files
                            .push(json!({"path": p, "language": kind, "diagnostics": diagnostics}));
                    }
                    if files.len() >= 200 {
                        break;
                    }
                }
                json!({
                    "path": path,
                    "scanned": scanned,
                    "skipped_not_utf8": skipped,
                    "error_count": error_count,
                    "clean": error_count == 0,
                    "files": files,
                })
                .to_string()
            }
        };
        Ok(bounded(ctx, text))
    }
}
