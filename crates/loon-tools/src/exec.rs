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
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "NVM_DIR",
    "PNPM_HOME",
    "BUN_INSTALL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "SSH_AUTH_SOCK",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
];

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a Bash command in the workspace (falls back to sh when Bash is unavailable). stdout/stderr are captured separately; the exit code is reported. A timeout kills the whole process group. Commands inherit a filtered development environment, including active virtualenv/toolchain and proxy settings. Set shell to sh explicitly for POSIX commands.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "minLength": 1, "description": "The shell command line to run, e.g. `cargo test`."},
                    "cwd": {"type": "string", "description": "Subdirectory of the workspace to run in."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600},
                    "shell": {"type": "string", "enum": ["bash", "sh"]}
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

        if command.trim().is_empty() {
            return Err(ToolError::InvalidArgs("command must not be blank".into()));
        }
        let shell = if args["shell"] == "sh" || !std::path::Path::new("/bin/bash").exists() {
            "/bin/sh"
        } else {
            "/bin/bash"
        };
        let mut cmd = development_command(shell, &command, &cwd, ctx);

        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|e| {
            ToolError::Other(format!(
                "failed to start {shell}: {e} (verify your system shell is available)"
            ))
        })?;
        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");

        // Drain continuously but retain at most 8 MiB per pipe. `yes`, verbose
        // builds and accidental binary output must not exhaust the host's memory.
        let read_out = tokio::spawn(async move { capture(&mut stdout_pipe).await });
        let read_err = tokio::spawn(async move { capture(&mut stderr_pipe).await });
        let mut group = ProcessGroup::new(child.id());

        let mut cancel = ctx.cancel.clone();
        let status = tokio::select! {
            s = child.wait() => Some(s.map_err(ToolError::Io)?),
            _ = tokio::time::sleep(timeout) => None,
            _ = cancel_wait(&mut cancel) => None, // cancelled: treated like timeout below
        };

        let (timed_out, status) = match status {
            Some(s) => (false, Some(s)),
            None => {
                group.terminate();
                // Reap so pipes close and readers finish.
                let _ = child.wait().await;
                (true, None)
            }
        };
        // A background descendant can hold inherited pipes after the shell exits.
        // Reclaim the process group and bound the drain instead of hanging forever.
        group.terminate();
        let out = finish_capture(read_out).await;
        let err = finish_capture(read_err).await;
        let duration = started.elapsed();

        let stdout_s = String::from_utf8_lossy(&out).to_string();
        let stderr_s = String::from_utf8_lossy(&err).to_string();
        let (bounded_out, full_a, trunc_a) = ctx.bound(stdout_s);
        let (bounded_err, full_b, trunc_b) = ctx.bound(stderr_s);

        let mut text = String::new();
        if timed_out {
            if *ctx.cancel.borrow() {
                text.push_str("[CANCELLED: process group killed]\n");
            } else {
                text.push_str(&format!(
                    "[TIMEOUT after {:?}: process group killed]\n",
                    timeout
                ));
            }
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
            combined.extend_from_slice(b"--- stdout (full) ---\n");
            combined.extend_from_slice(full_a.as_deref().unwrap_or(bounded_out.as_bytes()));
            combined.extend_from_slice(b"\n--- stderr (full) ---\n");
            combined.extend_from_slice(full_b.as_deref().unwrap_or(bounded_err.as_bytes()));
            full = Some(combined);
        }

        Ok(ToolOutput {
            ok: !timed_out && status.map(|s| s.success()).unwrap_or(false),
            text,
            full,
            truncated: trunc_a || trunc_b,
            structured: json!({
                "exit_code": exit,
                "timed_out": timed_out && !*ctx.cancel.borrow(),
                "cancelled": *ctx.cancel.borrow(),
                "shell": shell,
                "duration_ms": duration.as_millis() as u64,
                "cwd": cwd.display().to_string(),
            }),
        })
    }
}

pub(crate) async fn cancel_wait(rx: &mut tokio::sync::watch::Receiver<bool>) {
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

// Every launched shell owns a process group. ESRCH during cleanup means it has
// already exited; there is no fallback signal to send to a reaped/reused child PID.
fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
        // SAFETY: a strictly negative, representable PID targets only the process
        // group we created. No pointers or memory are passed to this syscall.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

// RAII also handles a TUI hard stop dropping the running future.
pub struct ProcessGroup(Option<u32>);
impl ProcessGroup {
    pub fn new(pid: Option<u32>) -> Self {
        Self(pid)
    }
    pub fn terminate(&mut self) {
        kill_process_group(self.0.take());
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}
async fn capture(pipe: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    const CAP: usize = 8 * 1024 * 1024;
    let mut saved = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut dropped = false;
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let keep = n.min(CAP.saturating_sub(saved.len()));
                saved.extend_from_slice(&chunk[..keep]);
                dropped |= keep < n;
            }
        }
    }
    if dropped {
        saved.extend_from_slice(b"\n[capture capped at 8 MiB]\n");
    }
    saved
}
async fn finish_capture(mut task: tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
        Ok(r) => r.unwrap_or_default(),
        Err(_) => {
            task.abort();
            b"[output pipe did not close; capture stopped]".to_vec()
        }
    }
}

pub(crate) fn development_command(
    shell: &str,
    command: &str,
    cwd: &std::path::Path,
    ctx: &ToolCtx,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg("-c").arg(command).current_dir(cwd).env_clear();
    for k in ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    // Host-provided values take precedence over the ambient environment.
    for (k, v) in &ctx.child_env {
        cmd.env(k, v);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("PAGER", "cat");
    cmd.env("GIT_PAGER", "cat");
    cmd.kill_on_drop(true);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        // Own process group so timeout/cancel can take down the whole tree.
        cmd.process_group(0);
    }

    cmd
}
