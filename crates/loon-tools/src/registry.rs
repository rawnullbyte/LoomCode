//! Tool registry: construction, lookup, schema exposure to providers.

use crate::tool::{to_provider_format, Tool, ToolSpec};
use std::sync::Arc;

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
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
            Arc::new(FuzzyEditTool),
            Arc::new(MultiEditTool),
            Arc::new(GlobTool),
            Arc::new(GrepTool),
            Arc::new(TreeTool),
            Arc::new(BashTool),
        ])
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
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
