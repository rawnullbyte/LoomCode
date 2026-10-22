//! Fixed, read-only Git operations. No shell command supplied by the model.
use crate::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
#[derive(Clone, Copy)]
pub enum Kind {
    Status,
    Diff,
    Log,
    Show,
    Blame,
    Branches,
}
pub struct GitTool(pub Kind);
pub(crate) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[async_trait]
impl Tool for GitTool {
    fn spec(&self) -> ToolSpec {
        let(name,description)=match self.0 {
   Kind::Status=>("git_status","Inspect branch and staged/unstaged/untracked changes without mutating the Git index."),
   Kind::Diff=>("git_diff","Inspect unstaged or staged patches, optionally against a revision and scoped path. External diff/textconv are disabled."),
   Kind::Log=>("git_log","Read recent commits, optionally scoped to a path/revision. No network request or repository mutation."),
   Kind::Show=>("git_show","Read a commit or a file at a revision without checking out or changing the working tree."),
   Kind::Blame=>("git_blame","Read bounded line blame for one tracked file at a revision. Does not alter the working tree."),
   Kind::Branches=>("git_branches","List local/remote branch references and current branch without fetching or changing branches."),
  };
        let mut props = json!({"cwd":{"type":"string"}});
        let mut required = Vec::new();
        match self.0 {
            Kind::Diff => {
                props["path"] = json!({"type":"string"});
                props["revision"] = json!({"type":"string"});
                props["staged"] = json!({"type":"boolean"});
                props["context"] = json!({"type":"integer","minimum":0,"maximum":20});
            }
            Kind::Log => {
                props["path"] = json!({"type":"string"});
                props["revision"] = json!({"type":"string"});
                props["limit"] = json!({"type":"integer","minimum":1,"maximum":100});
            }
            Kind::Show => {
                props["path"] = json!({"type":"string"});
                props["revision"] = json!({"type":"string"});
            }
            Kind::Blame => {
                props["path"] = json!({"type":"string"});
                props["revision"] = json!({"type":"string"});
                props["start"] = json!({"type":"integer","minimum":1});
                props["end"] = json!({"type":"integer","minimum":1});
                required.push("path");
            }
            _ => {}
        }
        ToolSpec {
            name: name.into(),
            description: description.into(),
            effects: Effects::ReadOnly,
            parameters: json!({"type":"object","properties":props,"required":required,"additionalProperties":false}),
        }
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let revision = args["revision"].as_str().unwrap_or("HEAD");
        if revision.starts_with('-') || revision.is_empty() || revision.contains('\0') {
            return Err(ToolError::InvalidArgs("invalid revision".into()));
        }
        let mut argv: Vec<String> = [
            "git",
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "color.ui=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let path = args["path"].as_str();
        match self.0 {
            Kind::Status => argv.extend(
                ["status", "--short", "--branch", "--untracked-files=normal"].map(String::from),
            ),
            Kind::Diff => {
                argv.extend(["diff", "--no-ext-diff", "--no-textconv"].map(String::from));
                argv.push(format!(
                    "--unified={}",
                    args["context"].as_u64().unwrap_or(3).min(20)
                ));
                if args["staged"].as_bool().unwrap_or(false) {
                    argv.push("--cached".into());
                }
                if args.get("revision").is_some() {
                    argv.push(revision.into());
                }
                argv.push("--".into());
                if let Some(path) = path {
                    argv.push(path.into());
                }
            }
            Kind::Log => {
                argv.extend(["log", "--format=%h %ad %s", "--date=short"].map(String::from));
                argv.push(format!(
                    "-n{}",
                    args["limit"].as_u64().unwrap_or(20).clamp(1, 100)
                ));
                argv.push(revision.into());
                argv.push("--".into());
                if let Some(path) = path {
                    argv.push(path.into());
                }
            }
            Kind::Show => {
                argv.extend(
                    ["show", "--no-ext-diff", "--no-textconv", "--format=fuller"].map(String::from),
                );
                argv.push(if let Some(path) = path {
                    format!("{revision}:{path}")
                } else {
                    revision.into()
                });
                argv.push("--".into());
            }
            Kind::Blame => {
                let path = path.ok_or_else(|| ToolError::InvalidArgs("path required".into()))?;
                let start = args["start"].as_u64().unwrap_or(1).max(1);
                let end = args["end"].as_u64();
                if end.is_some_and(|end| end < start || end - start > 1999) {
                    return Err(ToolError::InvalidArgs(
                        "blame range must be ordered and at most 2000 explicit lines".into(),
                    ));
                }
                argv.extend(["blame", "--no-textconv"].map(String::from));
                if args.get("start").is_some() || end.is_some() {
                    argv.push("-L".into());
                    argv.push(format!(
                        "{start},{}",
                        end.map(|n| n.to_string()).unwrap_or_default()
                    ));
                }
                argv.extend([revision.into(), "--".into(), path.into()]);
            }
            Kind::Branches => {
                argv.extend(["branch", "--all", "--no-color", "--verbose"].map(String::from))
            }
        }
        let command = format!(
            "GIT_OPTIONAL_LOCKS=0 {}",
            argv.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
        );
        crate::exec::BashTool.run(json!({"command":command,"cwd":args["cwd"].as_str().unwrap_or("."),"timeout_secs":30}),ctx).await
    }
}
