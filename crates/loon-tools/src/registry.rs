//! Tool registry: construction, lookup, schema exposure to providers.

use crate::tool::{to_provider_format, Tool, ToolSpec};
use std::sync::Arc;

#[derive(Clone)]
pub struct ToolRegistry {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Extend with additional tools (used by hosts adding e.g. the memory tool).
    pub fn with_extra(self, extra: Vec<Arc<dyn Tool>>) -> ToolRegistry {
        let mut tools = self.tools;
        tools.extend(extra);
        ToolRegistry { tools }
    }

    pub fn without(self, names: &[&str]) -> Self {
        Self {
            tools: self
                .tools
                .into_iter()
                .filter(|t| !names.contains(&t.spec().name.as_str()))
                .collect(),
        }
    }

    pub fn new(tools: Vec<Arc<dyn Tool>>) -> ToolRegistry {
        ToolRegistry { tools }
    }

    /// The standard workspace toolset, in stable order (prompt-cache friendly).
    pub fn standard() -> ToolRegistry {
        use crate::exec::BashTool;
        use crate::fs_tools::*;
        use crate::search_tools::*;
        ToolRegistry::new(vec![
            Arc::new(ReadTool),
            Arc::new(WriteTool),
            Arc::new(EditTool),
            Arc::new(crate::patch::PatchTool),
            Arc::new(FuzzyEditTool),
            Arc::new(MultiEditTool),
            Arc::new(GlobTool),
            Arc::new(GrepTool),
            Arc::new(TreeTool),
            Arc::new(BashTool),
            Arc::new(crate::session::ShellSessionTool::default()),
            Arc::new(crate::monitor::MonitorTool::default()),
            Arc::new(crate::ssh::SshTool::default()),
            Arc::new(crate::fetch::FetchTool),
            Arc::new(crate::process::ProcessTool::default()),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Directory)),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Info)),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Many)),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Symbol)),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Project)),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Json)),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Notebook)),
            Arc::new(crate::inspect::InspectTool(crate::inspect::Kind::Compare)),
            Arc::new(crate::structured::JsonEditTool),
            Arc::new(crate::structured::NotebookEditTool),
            Arc::new(crate::git_tools::GitTool(crate::git_tools::Kind::Status)),
            Arc::new(crate::git_tools::GitTool(crate::git_tools::Kind::Diff)),
            Arc::new(crate::git_tools::GitTool(crate::git_tools::Kind::Log)),
            Arc::new(crate::git_tools::GitTool(crate::git_tools::Kind::Show)),
            Arc::new(crate::git_tools::GitTool(crate::git_tools::Kind::Blame)),
            Arc::new(crate::git_tools::GitTool(crate::git_tools::Kind::Branches)),
            Arc::new(crate::checks::CheckTool(crate::checks::Check::Tests)),
            Arc::new(crate::checks::CheckTool(crate::checks::Check::Build)),
            Arc::new(crate::checks::CheckTool(crate::checks::Check::Lint)),
        ])
        .with_extra(crate::context7::Context7Tool::defaults())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        // Exact names, including MCP names, always take priority over aliases.
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .cloned()
            .or_else(|| {
                let canonical = match name {
                    "Bash" | "exec_command" | "shell" | "run_command" => "bash",
                    "Read" | "read_file" => "read",
                    "Write" | "write_file" => "write",
                    "Edit" | "edit_file" => "edit",
                    "Grep" | "search_files" => "grep",
                    "Glob" | "list_files" => "glob",
                    "MultiEdit" | "multiedit" => "multi_edit",
                    "TodoWrite" | "write_todos" => "todo_write",
                    "TodoRead" | "read_todos" => "todo_read",
                    "ShellSession" | "shell_session" => "shell_session",
                    "SSH" | "remote" => "ssh",
                    "Monitor" | "watch" => "monitor",
                    "NotebookEdit" => "notebook_edit",
                    "NotebookRead" => "notebook_read",
                    "LS" => "list_directory",
                    "resolve-library-id" => "context7_resolve_library",
                    "query-docs" => "context7_query_docs",
                    _ => return None,
                };
                self.tools
                    .iter()
                    .find(|t| t.spec().name == canonical)
                    .cloned()
            })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn provider_definitions(&self) -> Vec<serde_json::Value> {
        self.specs().iter().map(to_provider_format).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::validate_args;
    use serde_json::json;

    #[test]
    fn standard_registry_specs_are_valid_and_unique() {
        let reg = ToolRegistry::standard();
        assert!(reg.len() >= 8);
        let mut names = Vec::new();
        for spec in reg.specs() {
            assert!(
                !spec.description.is_empty(),
                "{} needs a description",
                spec.name
            );
            assert!(spec.parameters.is_object());
            assert!(!names.contains(&spec.name), "duplicate tool {}", spec.name);
            names.push(spec.name);
        }
        // every spec validates its own happy-path example shape
        for spec in reg.specs() {
            assert!(
                validate_args(&spec, &json!({})).is_ok()
                    || spec.parameters.get("required").is_some()
            );
        }
    }

    #[test]
    fn provider_format_matches_openai_function_shape() {
        let reg = ToolRegistry::standard();
        for def in reg.provider_definitions() {
            assert_eq!(def["type"], "function");
            assert!(def["function"]["name"].is_string());
            assert!(def["function"]["parameters"].is_object());
        }
    }
}
