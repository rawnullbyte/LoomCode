//! Project-aware checks with explicit, inspectable command selection.
use crate::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
#[derive(Clone, Copy)]
pub enum Check {
    Tests,
    Build,
    Lint,
}
pub struct CheckTool(pub Check);
impl CheckTool {
    pub fn command(&self, root: &std::path::Path, args: &Value) -> Result<String, ToolError> {
        let kind = match self.0 {
            Check::Tests => "test",
            Check::Build => "build",
            Check::Lint => "lint",
        };
        let mut argv: Vec<String> = if root.join("Cargo.toml").is_file() {
            let mut values = vec![
                "cargo".into(),
                if kind == "lint" { "clippy" } else { kind }.into(),
            ];
            if let Some(package) = args["package"].as_str() {
                values.extend(["-p".into(), package.into()]);
            }
            if kind == "test" {
                if let Some(filter) = args["filter"].as_str() {
                    values.push(filter.into());
                }
            }
            values
        } else if root.join("package.json").is_file() {
            let manifest: Value =
                serde_json::from_slice(&crate::inspect::read_bytes(&root.join("package.json"))?)
                    .map_err(|e| ToolError::InvalidArgs(format!("package.json: {e}")))?;
            if manifest["scripts"][kind].as_str().is_none() {
                return Err(ToolError::InvalidArgs(format!("package.json has no {kind} script; inspect project_info and use bash with the correct command")));
            }
            let manager = if root.join("pnpm-lock.yaml").is_file() {
                "pnpm"
            } else if root.join("yarn.lock").is_file() {
                "yarn"
            } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
                "bun"
            } else {
                "npm"
            };
            vec![manager.into(), "run".into(), kind.into()]
        } else if root.join("go.mod").is_file() {
            vec![
                "go".into(),
                if kind == "lint" { "vet" } else { kind }.into(),
                "./...".into(),
            ]
        } else if root.join("pyproject.toml").is_file()
            || root.join("pytest.ini").is_file()
            || root.join("requirements.txt").is_file()
        {
            match self.0 {
                Check::Tests => {
                    let mut values = vec!["python3".into(), "-m".into(), "pytest".into()];
                    if let Some(filter) = args["filter"].as_str() {
                        values.extend(["-k".into(), filter.into()]);
                    }
                    values
                }
                Check::Build => vec!["python3".into(), "-m".into(), "build".into()],
                Check::Lint => vec![
                    "python3".into(),
                    "-m".into(),
                    "ruff".into(),
                    "check".into(),
                    ".".into(),
                ],
            }
        } else {
            return Err(ToolError::InvalidArgs("project type not detected; inspect project_info and use bash with the repository's documented check command".into()));
        };
        if let Some(extra) = args["args"].as_array() {
            if extra.len() > 20 {
                return Err(ToolError::InvalidArgs(
                    "at most 20 additional arguments".into(),
                ));
            }
            for value in extra {
                argv.push(
                    value
                        .as_str()
                        .ok_or_else(|| ToolError::InvalidArgs("args must contain strings".into()))?
                        .into(),
                );
            }
        }
        Ok(argv
            .iter()
            .map(|s| crate::git_tools::quote(s))
            .collect::<Vec<_>>()
            .join(" "))
    }
}
#[async_trait]
impl Tool for CheckTool {
    fn spec(&self) -> ToolSpec {
        let(name,description)=match self.0 {Check::Tests=>("run_tests","Run tests using detected Cargo, package.json test script, Go or pytest configuration. Uses repository scripts; missing tools/errors are reported. Prefer the repo's documented commands when they differ."),Check::Build=>("run_build","Build detected Rust, JS/TS, Go or Python projects using Cargo, package scripts, go build or python -m build. Reports the actual command and output."),Check::Lint=>("run_lint","Run detected Cargo clippy, package lint script, go vet or Python ruff checks without auto-fixing by default. Reports missing dependencies instead of installing them.")};
        ToolSpec {
            name: name.into(),
            description: description.into(),
            effects: Effects::Mutating,
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"package":{"type":"string"},"filter":{"type":"string"},"args":{"type":"array","maxItems":20,"items":{"type":"string"}},"timeout_secs":{"type":"integer","minimum":1,"maximum":600}},"additionalProperties":false}),
        }
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = args["path"].as_str().unwrap_or(".");
        let root = ctx.ws.resolve(path)?;
        let command = self.command(&root, &args)?;
        let mut out=crate::exec::BashTool.run(json!({"command":command,"cwd":root,"timeout_secs":args["timeout_secs"].as_u64().unwrap_or(120).clamp(1,600)}),ctx).await?;
        out.text = format!("$ {command}\n{}", out.text);
        Ok(out)
    }
}
