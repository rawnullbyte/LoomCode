//! Local lifecycle hooks. Commands receive one JSON object on stdin; HTTP hooks
//! receive the same body. Output is bounded and no child inherits the terminal.
use crate::{Agent, AgentEvent};
use loon_config::schema::Hook;
use loon_tools::exec::ProcessGroup;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default, Debug)]
pub struct Outcome {
    pub blocked: bool,
    pub context: String,
}
impl Outcome {
    fn consume(&mut self, output: &str, exit_block: bool) {
        self.blocked |= exit_block;
        if let Ok(value) = serde_json::from_str::<Value>(output) {
            let specific = &value["hookSpecificOutput"];
            self.blocked |= specific["permissionDecision"] == "deny"
                || value["decision"] == "block"
                || value["continue"] == false;
            for item in [
                &specific["additionalContext"],
                &specific["permissionDecisionReason"],
                &value["reason"],
                &value["systemMessage"],
            ] {
                if let Some(text) = item.as_str() {
                    self.context.push_str(text);
                    self.context.push('\n');
                }
            }
        } else if !output.trim().is_empty() {
            self.context.push_str(output.trim());
            self.context.push('\n');
        }
    }
}

async fn invoke(
    hook: &Hook,
    input: Value,
    cwd: &std::path::Path,
) -> Result<(String, bool), String> {
    match (&hook.command, &hook.url) {
        (Some(command), None) => {
            let mut cmd = tokio::process::Command::new("bash");
            cmd.arg("--noprofile")
                .arg("--norc")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(unix)]
            cmd.process_group(0);
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            let mut group = ProcessGroup::new(child.id());
            let mut stdin = child.stdin.take().ok_or("hook stdin unavailable")?;
            let stdout = child.stdout.take().ok_or("hook stdout unavailable")?;
            let stderr = child.stderr.take().ok_or("hook stderr unavailable")?;
            let bytes = serde_json::to_vec(&input).map_err(|e| e.to_string())?;
            let writer = async move {
                let _ = stdin.write_all(&bytes).await;
                drop(stdin);
            };
            let read = |stream: tokio::process::ChildStdout| async move {
                let mut data = Vec::new();
                let _ = stream.take(65_537).read_to_end(&mut data).await;
                data
            };
            let err_read = async move {
                let mut data = Vec::new();
                let _ = stderr.take(65_537).read_to_end(&mut data).await;
                data
            };
            let (_, out, err, status) = tokio::join!(writer, read(stdout), err_read, child.wait());
            group.terminate();
            if out.len() > 65_536 || err.len() > 65_536 {
                return Err("hook output exceeds 64 KiB".into());
            }
            let status = status.map_err(|e| e.to_string())?;
            let out = String::from_utf8_lossy(&out).to_string();
            if status.code() == Some(2) {
                Ok((format!("{out}\n{}", String::from_utf8_lossy(&err)), true))
            } else if status.success() {
                Ok((out, false))
            } else {
                Err(format!(
                    "exit {}: {}",
                    status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&err)
                        .chars()
                        .take(500)
                        .collect::<String>()
                ))
            }
        }
        (None, Some(url)) => {
            let response = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| e.to_string())?
                .post(url)
                .json(&input)
                .send()
                .await
                .map_err(|e| e.to_string())?
                .error_for_status()
                .map_err(|e| e.to_string())?;
            let mut stream = response;
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.chunk().await.map_err(|e| e.to_string())? {
                if bytes.len() + chunk.len() > 65_536 {
                    return Err("hook output exceeds 64 KiB".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok((String::from_utf8_lossy(&bytes).to_string(), false))
        }
        _ => Err("hook requires exactly one command or url".into()),
    }
}

impl Agent {
    /// Hosts call this once when closing a session surface.
    pub async fn finish_session(&self) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.run_hooks("SessionEnd", json!({})),
        )
        .await;
    }
    pub(crate) async fn run_hooks(&self, event: &str, data: Value) -> Outcome {
        let cfg = match loon_config::Config::load_effective(&self.loon_paths(), self.ctx.ws.root())
        {
            Ok((cfg, _)) => cfg,
            Err(e) => {
                self.emit(AgentEvent::Warning(format!("hooks config: {e}")));
                return Outcome {
                    blocked: true,
                    context: format!("Invalid hooks configuration: {e}"),
                };
            }
        };
        let mut result = Outcome::default();
        let mut input = json!({"hook_event_name":event, "session_id":self.session_id.to_string(), "cwd":self.ctx.ws.root(), "data":data});
        // Familiar top-level fields make hooks easy to share across local harnesses.
        if let Some(obj) = data.as_object() {
            for (k, v) in obj {
                input[k] = v.clone();
            }
        }
        for hook in cfg.hooks.get(event).into_iter().flatten() {
            if let Some(matcher) = &hook.matcher {
                let tool = data["tool_name"].as_str().unwrap_or("");
                if !matcher
                    .split('|')
                    .any(|m| m.trim() == "*" || m.trim() == tool)
                {
                    continue;
                }
            }
            let mut cancel = self.ctx.cancel.clone();
            if *cancel.borrow() && event != "SessionEnd" {
                break;
            }
            let timeout = std::time::Duration::from_secs(hook.timeout_secs.clamp(1, 600));
            let response = tokio::select! {
                output = tokio::time::timeout(timeout, invoke(hook, input.clone(), self.ctx.ws.root())) => output.map_err(|_| "hook timed out".to_string()).and_then(|r|r),
                _ = crate::cancel_changed(&mut cancel), if event!="SessionEnd" => Err("hook cancelled".into()),
            };
            match response {
                Ok((output, blocked)) => result.consume(&output, blocked),
                Err(e) => {
                    self.emit(AgentEvent::Warning(format!("{event} hook: {e}")));
                    // A failed pre-operation policy hook cannot accidentally grant access.
                    if matches!(event, "PreToolUse" | "UserPromptSubmit") {
                        result.blocked = true;
                        result.context.push_str(&e);
                    }
                }
            }
        }
        result.context = result.context.chars().take(16_000).collect();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_deny_and_context_are_preserved() {
        let mut result = Outcome::default();
        result.consume(r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"use patch","additionalContext":"conventions"}}"#,false);
        assert!(result.blocked);
        assert!(result.context.contains("use patch"));
        assert!(result.context.contains("conventions"));
        result.consume("stop until verified", true);
        assert!(result.context.contains("verified"));
    }
}
