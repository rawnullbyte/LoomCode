//! Persistent memory: durable facts that outlive sessions.
//!
//! Two scopes, both plain Markdown so users can read and edit them directly:
//! - **global** — `<data_dir>/memory/MEMORY.md` (user preferences, cross-project facts);
//! - **workspace** — `<cwd>/.loon/memory/MEMORY.md` (project decisions, conventions).
//!
//! Both are loaded into the system prompt at session start (workspace after global;
//! nearest scope wins on conflict by convention). The `memory` tool lets the model
//! persist a fact mid-session; writes are append-only with a one-line summary index.

use std::path::{Path, PathBuf};

pub const MEMORY_HEADER: &str =
    "# Loon memory\n\nDurable facts below are loaded into every session for this scope.\n";

pub struct MemoryPaths {
    pub global: PathBuf,
    pub workspace: PathBuf,
}

impl MemoryPaths {
    pub fn resolve(data_dir: &Path, cwd: &Path) -> MemoryPaths {
        MemoryPaths {
            global: data_dir.join("memory").join("MEMORY.md"),
            workspace: cwd.join(".loon").join("memory").join("MEMORY.md"),
        }
    }
}

/// Load both scopes for the system prompt. Missing files are empty; entries are
/// truncated defensively so a runaway memory file cannot eat the context window.
pub fn load_for_prompt(paths: &MemoryPaths) -> String {
    let mut out = String::new();
    for (label, path) in [("global", &paths.global), ("workspace", &paths.workspace)] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let body = text
                .lines()
                .filter(|l| !l.starts_with("# Loon memory"))
                .collect::<Vec<_>>()
                .join("\n");
            if !body.trim().is_empty() {
                out.push_str(&format!("\n[{label} memory]\n{}\n", body.trim()));
            }
        }
    }
    out
}

/// Append one fact to a scope, creating the file with its header when absent.
/// Returns the path written.
pub fn save(paths: &MemoryPaths, scope: Scope, fact: &str) -> std::io::Result<PathBuf> {
    let path = match scope {
        Scope::Global => &paths.global,
        Scope::Workspace => &paths.workspace,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    if !path.exists() {
        body.push_str(MEMORY_HEADER);
    }
    body.push_str(&format!("- {fact}\n"));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(body.as_bytes())?;
    Ok(path.to_path_buf())
}

/// Read one scope's raw file (for `loon memory` display and the /memory command).
pub fn read_scope(paths: &MemoryPaths, scope: Scope) -> String {
    let path = match scope {
        Scope::Global => &paths.global,
        Scope::Workspace => &paths.workspace,
    };
    std::fs::read_to_string(path).unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Workspace,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Workspace => "workspace",
        }
    }
}

// ─── the memory tool ────────────────────────────────────────────────────────────

/// `memory` tool: persist one durable fact. Available in the default toolset; ask-mode
/// gates it as mutating.
pub struct MemoryTool {
    pub paths: Arc<MemoryPaths>,
}

#[async_trait::async_trait]
impl loon_tools::Tool for MemoryTool {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "memory".into(),
            description: "Persist one durable fact for future sessions. Scope `workspace` for project-specific decisions and conventions, `global` for user preferences. One sentence per call; do not store transient task state.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "fact": {"type": "string", "minLength": 1, "description": "The durable fact, one sentence."},
                    "scope": {"type": "string", "enum": ["global", "workspace"], "description": "Defaults to workspace."}
                },
                "required": ["fact"],
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
        let fact = args["fact"]
            .as_str()
            .ok_or_else(|| loon_tools::ToolError::InvalidArgs("fact required".into()))?;
        if fact.trim().is_empty() {
            return Err(loon_tools::ToolError::InvalidArgs("fact is empty".into()));
        }
        let scope = match args.get("scope").and_then(serde_json::Value::as_str) {
            Some("global") => Scope::Global,
            _ => Scope::Workspace,
        };
        let path = save(&self.paths, scope, fact.trim())
            .map_err(|e| loon_tools::ToolError::Other(format!("memory write failed: {e}")))?;
        Ok(loon_tools::ToolOutput::ok(format!(
            "saved to {} memory ({})",
            scope.as_str(),
            path.display()
        )))
    }
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tmp: &Path) -> MemoryPaths {
        MemoryPaths::resolve(&tmp.join("data"), &tmp.join("proj"))
    }

    #[test]
    fn save_then_load_round_trips_both_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        save(&p, Scope::Global, "prefers concise answers").unwrap();
        save(&p, Scope::Workspace, "tests run via cargo nextest").unwrap();
        let prompt = load_for_prompt(&p);
        assert!(prompt.contains("[global memory]"));
        assert!(prompt.contains("prefers concise answers"));
        assert!(prompt.contains("[workspace memory]"));
        assert!(prompt.contains("cargo nextest"));
        // raw read keeps the header
        assert!(read_scope(&p, Scope::Global).starts_with("# Loon memory"));
    }

    #[test]
    fn load_is_empty_when_no_files_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        assert!(load_for_prompt(&p).is_empty());
        assert!(read_scope(&p, Scope::Workspace).is_empty());
    }

    #[test]
    fn appends_accumulate() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        save(&p, Scope::Workspace, "one").unwrap();
        save(&p, Scope::Workspace, "two").unwrap();
        let text = read_scope(&p, Scope::Workspace);
        assert!(text.contains("- one\n"));
        assert!(text.contains("- two\n"));
        assert_eq!(text.matches("# Loon memory").count(), 1);
    }
}
