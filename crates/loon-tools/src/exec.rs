//! Shell execution tool: one-shot command with filtered environment, separate
//! stdout/stderr, sandbox-checked cwd, and process-group teardown on timeout.
//!
//! M1 limitation (documented, not hidden): output capture is pipe-based, so stdout and
//! stderr are separated exactly (unlike a PTY); persistent named PTY jobs are a later
//! milestone. Cancellation kills the child's process group, not just the shell.

use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// Environment variables child processes inherit. Everything else is filtered.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "SHELL", "LANG", "LC_ALL", "TERM", "TMPDIR",
];

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command (sh -c) in the workspace. stdout/stderr are captured separately; the exit code is reported. A timeout kills the whole process group. Commands run with a minimal environment (PATH, HOME, TERM, ...).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "minLength": 1},
                    "cwd": {"type": "string", "description": "Subdirectory of the workspace to run in."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            effects: Effects::Mutating,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = args["command"].as_str().unwrap_or_default().to_string();
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(c) => ctx.ws.resolve(c)?,
            None => ctx.ws.root().to_path_buf(),
        };
        if !cwd.is_dir() {
            return Err(ToolError::Other(format!(
                "cwd {} is not a directory",
                cwd.display()
            )));
        }
        let timeout = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .map(Duration::from_secs)
            .unwrap_or(ctx.default_timeout);
        if timeout > Duration::from_secs(600) {
            return Err(ToolError::InvalidArgs("timeout capped at 600s".into()));
        }

        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(&command).current_dir(&cwd).env_clear();
        for (k, v) in &ctx.child_env {
            cmd.env(k, v);
        }
        for k in ENV_ALLOWLIST {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        #[cfg(unix)]
        {
            // Own process group so timeout/cancel can take down the whole tree.
            cmd.process_group(0);
        }

        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|e| {
            ToolError::Other(format!(
                "failed to start /bin/sh: {e} (verify your system shell is available)"
            ))
        })?;
        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");

        let read_out = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        });
        let read_err = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });

        let mut cancel = ctx.cancel.clone();
        let status = tokio::select! {
            s = child.wait() => Some(s.map_err(ToolError::Io)?),
            _ = tokio::time::sleep(timeout) => None,
            _ = cancel_wait(&mut cancel) => None, // cancelled: treated like timeout below
        };

        let (timed_out, status) = match status {
            Some(s) => (false, Some(s)),
            None => {
                kill_process_group(child.id());
                // Reap so pipes close and readers finish.
                let _ = child.wait().await;
                (true, None)
            }
        };
        let out = read_out.await.unwrap_or_default();
        let err = read_err.await.unwrap_or_default();
        let duration = started.elapsed();

        let stdout_s = String::from_utf8_lossy(&out).to_string();
        let stderr_s = String::from_utf8_lossy(&err).to_string();
        let (bounded_out, full_a, trunc_a) = ctx.bound(stdout_s);
        let (bounded_err, full_b, trunc_b) = ctx.bound(stderr_s);

        let mut text = String::new();
        if timed_out {
            text.push_str(&format!(
                "[TIMEOUT after {:?}: process group killed]\n",
                timeout
            ));
        }
        if !bounded_out.is_empty() {
            text.push_str("--- stdout ---\n");
            text.push_str(&bounded_out);
            text.push('\n');
        }
        if !bounded_err.is_empty() {
            text.push_str("--- stderr ---\n");
            text.push_str(&bounded_err);
            text.push('\n');
        }
        let exit = status.as_ref().map(|s| s.code().unwrap_or(-1));
        text.push_str(&format!(
            "[exit {}: {}]",
            exit.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            match &status {
                Some(s) if s.code().is_none() => "terminated by signal",
                _ => "completed",
            }
        ));
        text.push_str(&format!(" ({:.2}s)\n", duration.as_secs_f64()));

        let mut full = None;
        if full_a.is_some() || full_b.is_some() {
            let mut combined = Vec::new();
            if let Some(a) = full_a {
                combined.extend_from_slice(b"--- stdout (full) ---\n");
                combined.extend_from_slice(&a);
            }
            if let Some(b) = full_b {
                combined.extend_from_slice(b"--- stderr (full) ---\n");
                combined.extend_from_slice(&b);
            }
            full = Some(combined);
        }

        Ok(ToolOutput {
            ok: !timed_out && status.map(|s| s.success()).unwrap_or(false),
            text,
            full,
            truncated: trunc_a || trunc_b,
            structured: json!({
                "exit_code": exit,
                "timed_out": timed_out,
                "duration_ms": duration.as_millis() as u64,
                "cwd": cwd.display().to_string(),
            }),
        })
    }
}

async fn cancel_wait(rx: &mut tokio::sync::watch::Receiver<bool>) {
    // Resolves as soon as cancellation is requested.
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            // sender dropped: never cancelled
            std::future::pending::<()>().await;
        }
    }
}

fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        use std::process::Command;
        // Negative pid targets the whole group (child was started with process_group(0)).
        let _ = Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status();
        // Fall back to the child itself in case the group kill failed.
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    #[cfg(not(unix))]
    let _ = pid;
}
