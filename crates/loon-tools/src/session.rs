//! Persistent shell sessions: one long-lived shell process per named session, reused
//! across tool calls so `cd`, exported variables and activated virtualenvs survive.
//!
//! Why an end-marker instead of waiting for the prompt: the shell's stdout is a pipe, so
//! there is no prompt to match. Each command is followed by a sentinel that echoes a
//! per-call token plus the exit status; everything before the sentinel is that call's
//! output. This is exact and needs no terminal emulation.
//!
//! Honest limits (documented, not hidden):
//! - output is pipe-based, so there is no TTY: full-screen programs (`vim`, `top`) and
//!   interactive password prompts do not work — use `bash` or `ssh` for those;
//! - stdout and stderr are merged per call (ordering is preserved, which is usually what
//!   a human reads anyway); the exit status is reported separately;
//! - a command that never finishes hits the per-call timeout; the session is then
//!   terminated rather than reused, because a half-consumed stdout cannot be resynced.

use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;

/// Concurrent session cap: each one is a real shell process.
const MAX_SESSIONS: usize = 8;
/// Per-session output retained between calls (bytes).
const MAX_BUFFER: usize = 512 * 1024;

pub struct ShellSessionTool {
    sessions: AsyncMutex<BTreeMap<String, Session>>,
    counter: AtomicU64,
}

impl Default for ShellSessionTool {
    fn default() -> Self {
        ShellSessionTool {
            sessions: AsyncMutex::new(BTreeMap::new()),
            counter: AtomicU64::new(0),
        }
    }
}

struct Session {
    child: tokio::process::Child,
    group: crate::exec::ProcessGroup,
    in_flight: bool,
    stdin: tokio::process::ChildStdin,
    /// Accumulated stdout+stderr. Shared with the pump task.
    buf: Arc<Mutex<Buffer>>,
    cwd: String,
    shell: String,
    created: std::time::Instant,
    calls: u64,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Dropping the child kills it (`kill_on_drop` was set at spawn), so a polite
        // `exit` write is unnecessary and cannot be awaited here anyway.
        self.group.terminate();
        let _ = self.child.start_kill();
    }
}

#[derive(Default)]
struct Buffer {
    text: String,
    /// True once the pump task saw EOF: the shell is gone.
    closed: bool,
}

impl Buffer {
    fn add(&mut self, bytes: &[u8]) {
        self.text.push_str(&String::from_utf8_lossy(bytes));
        if self.text.len() > MAX_BUFFER {
            let mut cut = self.text.len() - MAX_BUFFER;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
        }
    }
}

#[async_trait]
impl Tool for ShellSessionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell_session".into(),
            description: "Reuse ONE long-lived shell instead of starting a new process per command: `cd`, exported variables and activated virtualenvs persist across calls. Actions: start (name, optional cwd/shell), run (name, command, timeout_secs) returning that call's output and exit status, cwd (print the working directory), close (name or all), list. Pipe-based: no TTY, so full-screen/interactive programs (vim, top, ssh password prompts) do not work — use bash or the ssh tool for those. A command that times out terminates the session rather than leaving it desynchronised.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["start", "run", "cwd", "close", "list"]},
                    "name": {"type": "string", "minLength": 1, "description": "Session name; defaults to `main`."},
                    "command": {"type": "string", "minLength": 1},
                    "cwd": {"type": "string", "description": "start: working directory (workspace-relative)."},
                    "shell": {"type": "string", "enum": ["bash", "sh"]},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            effects: Effects::Mutating,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let action = args["action"].as_str().unwrap_or_default();
        let name = args["name"].as_str().unwrap_or("main").to_string();
        match action {
            "start" => self.start(&name, &args, ctx).await,
            "run" => self.run_in(&name, &args, ctx).await,
            "cwd" => self.cwd(&name).await,
            "close" => self.close(&name).await,
            "list" => Ok(self.list().await),
            other => Err(ToolError::InvalidArgs(format!(
                "action must be start/run/cwd/close/list, got `{other}`"
            ))),
        }
    }
}

impl ShellSessionTool {
    async fn start(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
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
        let mut sessions = self.sessions.lock().await;
        let stale = sessions.get_mut(name).is_some_and(|s| {
            s.in_flight
                || s.buf.lock().unwrap().closed
                || s.child.try_wait().ok().flatten().is_some()
        });
        if stale {
            sessions.remove(name);
        }
        if let Some(existing) = sessions.get(name) {
            // Reuse: starting an existing session reports it instead of leaking a second
            // shell process under the same name.
            return Ok(ToolOutput {
                ok: true,
                text: format!(
                    "session `{name}` already running (pid {:?}, {} calls, cwd {})",
                    existing.child.id(),
                    existing.calls,
                    existing.cwd
                ),
                full: None,
                truncated: false,
                structured: json!({"name": name, "reused": true, "pid": existing.child.id()}),
            });
        }
        if sessions.len() >= MAX_SESSIONS {
            return Err(ToolError::Other(format!(
                "session limit reached ({MAX_SESSIONS}); close one with action=close"
            )));
        }
        let shell = if args["shell"] == "sh" || !std::path::Path::new("/bin/bash").exists() {
            "/bin/sh".to_string()
        } else {
            "/bin/bash".to_string()
        };
        let mut cmd = shell_command(&shell, &cwd, ctx);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Other(format!("failed to start {shell}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Other("shell stdin unavailable".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Other("shell stdout unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Other("shell stderr unavailable".into()))?;
        let buf = Arc::new(Mutex::new(Buffer::default()));
        // One pump for both pipes: ordering between stdout and stderr is preserved as
        // far as the OS delivers it, which is what a reader expects from a terminal.
        let pump_buf = buf.clone();
        tokio::spawn(async move {
            let mut out_chunk = [0u8; 8192];
            let mut err_chunk = [0u8; 8192];
            let mut out_open = true;
            let mut err_open = true;
            while out_open || err_open {
                tokio::select! {
                    r = stdout.read(&mut out_chunk), if out_open => match r {
                        Ok(0) | Err(_) => out_open = false,
                        Ok(n) => pump_buf.lock().unwrap().add(&out_chunk[..n]),
                    },
                    r = stderr.read(&mut err_chunk), if err_open => match r {
                        Ok(0) | Err(_) => err_open = false,
                        Ok(n) => pump_buf.lock().unwrap().add(&err_chunk[..n]),
                    },
                }
            }
            pump_buf.lock().unwrap().closed = true;
        });
        let pid = child.id();
        sessions.insert(
            name.to_string(),
            Session {
                child,
                group: crate::exec::ProcessGroup::new(pid),
                in_flight: false,
                stdin,
                buf,
                cwd: cwd.display().to_string(),
                shell: shell.clone(),
                created: std::time::Instant::now(),
                calls: 0,
            },
        );
        Ok(ToolOutput {
            ok: true,
            text: format!(
                "started session `{name}` (pid {:?}, {shell}, cwd {})",
                pid,
                cwd.display()
            ),
            full: None,
            truncated: false,
            structured: json!({"name": name, "pid": pid, "shell": shell, "cwd": cwd.display().to_string()}),
        })
    }

    async fn run_in(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let command = args["command"]
            .as_str()
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("run requires command".into()))?
            .to_string();
        let timeout = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .map(Duration::from_secs)
            .unwrap_or(ctx.default_timeout)
            .min(Duration::from_secs(600));

        let mut sessions = self.sessions.lock().await;
        let session = sessions.get_mut(name).ok_or_else(|| {
            ToolError::Other(format!(
                "no session `{name}`; call action=start first (or action=list)"
            ))
        })?;
        if session.in_flight || session.buf.lock().unwrap().closed {
            sessions.remove(name);
            return Err(ToolError::Other(format!(
                "session `{name}` exited; start it again"
            )));
        }

        // A token no real output can contain tells us where this call's output ends.
        let token = format!(
            "__LOON_END_{}_{}__",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        // eval one quoted unit: incomplete syntax reports an error instead of swallowing
        // the sentinel. Merge stderr in the shell so it cannot arrive after the marker.
        let quoted = format!("'{}'", command.replace('\'', "'\\''"));
        let script = format!(
            "eval {quoted} 2>&1\n__loon_status=$?\nprintf '\n{token} %s\n' \"$__loon_status\"\n"
        );
        session.in_flight = true;
        let mut command_group = crate::exec::ProcessGroup::new(session.child.id());
        // Discard anything already buffered (a previous call's tail, shell startup noise).
        session.buf.lock().unwrap().text.clear();
        if let Err(e) = session.stdin.write_all(script.as_bytes()).await {
            sessions.remove(name);
            return Err(ToolError::Other(format!(
                "session `{name}` is no longer writable: {e}"
            )));
        }
        if let Err(e) = session.stdin.flush().await {
            sessions.remove(name);
            return Err(ToolError::Other(format!(
                "session `{name}` flush failed: {e}"
            )));
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let mut wait = ctx.cancel.clone();
        let result = loop {
            if let Some(found) = take_until_marker(&session.buf, &token) {
                break Some(found);
            }
            if session.buf.lock().unwrap().closed {
                break None;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break None;
            }
            tokio::select! {
                _ = tokio::time::sleep(deadline - now) => break None,
                _ = crate::exec::cancel_wait(&mut wait) => break None,
                _ = tokio::time::sleep(Duration::from_millis(15)) => {}
            }
        };

        let (output, exit) = match result {
            Some(found) => found,
            None => {
                // Unsynchronised output cannot be safely reused: retire the session and
                // say so, instead of returning another call's bytes later.
                let cancelled = *ctx.cancel.borrow();
                let exited = session.buf.lock().unwrap().closed;
                sessions.remove(name);
                let mut text = if cancelled {
                    format!("[CANCELLED after {:?}: session `{name}` closed]\n", timeout)
                } else if exited {
                    format!("[EXITED: session `{name}` closed before its command marker]\n")
                } else {
                    format!("[TIMEOUT after {:?}: session `{name}` closed]\n", timeout)
                };
                text.push_str("session process group terminated; start the session again\n");
                return Ok(ToolOutput {
                    ok: false,
                    text,
                    full: None,
                    truncated: false,
                    structured: json!({"name": name, "timed_out": !cancelled && !exited, "exited": exited, "cancelled": cancelled, "closed": true}),
                });
            }
        };

        session.in_flight = false;
        command_group.disarm();
        session.calls += 1;
        // `cd` must be reflected, because the shell's cwd is the session's state.
        let cwd = session_cwd(session)
            .await
            .unwrap_or_else(|| session.cwd.clone());
        session.cwd = cwd.clone();
        let duration = session.created.elapsed();
        let (bounded, full, truncated) = ctx.bound(output);
        let ok = exit == 0;
        Ok(ToolOutput {
            ok,
            text: format!(
                "{bounded}\n[exit {exit}: session `{name}`, cwd {cwd}, {:.2}s total session age]",
                duration.as_secs_f64()
            ),
            full,
            truncated,
            structured: json!({
                "name": name,
                "exit_code": exit,
                "cwd": cwd,
                "calls": session.calls,
            }),
        })
    }

    async fn cwd(&self, name: &str) -> Result<ToolOutput, ToolError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(name)
            .ok_or_else(|| ToolError::Other(format!("no session `{name}`")))?;
        let cwd = session_cwd(session).await.ok_or_else(|| {
            ToolError::Other("could not read the session working directory".into())
        })?;
        session.cwd = cwd.clone();
        Ok(ToolOutput::ok(format!("session `{name}` cwd: {cwd}")))
    }

    async fn close(&self, name: &str) -> Result<ToolOutput, ToolError> {
        let mut sessions = self.sessions.lock().await;
        if name == "all" {
            let names: Vec<String> = sessions.keys().cloned().collect();
            sessions.clear();
            return Ok(ToolOutput::ok(if names.is_empty() {
                "no sessions open".to_string()
            } else {
                format!("closed sessions: {}", names.join(", "))
            }));
        }
        match sessions.remove(name) {
            Some(_) => Ok(ToolOutput::ok(format!("closed session `{name}`"))),
            None => Err(ToolError::Other(format!("no session `{name}`"))),
        }
    }

    async fn list(&self) -> ToolOutput {
        let sessions = self.sessions.lock().await;
        if sessions.is_empty() {
            return ToolOutput::ok("no sessions; start one with action=start");
        }
        let rows: Vec<String> = sessions
            .iter()
            .map(|(name, s)| {
                format!(
                    "{name}: pid {:?}, {}, {} call(s), cwd {}, {}s old",
                    s.child.id(),
                    s.shell,
                    s.calls,
                    s.cwd,
                    s.created.elapsed().as_secs()
                )
            })
            .collect();
        ToolOutput::ok(rows.join("\n"))
    }
}

/// Run `pwd` inside the session and read its answer, so `cd` is observable state.
async fn session_cwd(session: &mut Session) -> Option<String> {
    let token = format!("__LOON_PWD_{}__", std::process::id());
    session.buf.lock().unwrap().text.clear();
    session
        .stdin
        .write_all(format!("command pwd\nprintf '{token} 0\n'\n").as_bytes())
        .await
        .ok()?;
    session.stdin.flush().await.ok()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((out, _)) = take_until_marker(&session.buf, &token) {
            let line = out
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            return (!line.is_empty()).then_some(line);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Split the buffer at the sentinel, returning (output-before, exit status) and dropping
/// the consumed prefix so the next call starts clean.
fn take_until_marker(buf: &Arc<Mutex<Buffer>>, token: &str) -> Option<(String, i32)> {
    let mut guard = buf.lock().unwrap();
    let at = guard.text.find(token)?;
    let after = at + token.len();
    let newline = guard.text[after..].find('\n')?;
    let rest = guard.text[after..after + newline].to_string();
    // The status follows the token on the same line: "<token> <status>".
    let status_text = rest.lines().next().unwrap_or("").trim().to_string();
    let exit = status_text.parse::<i32>().unwrap_or(-1);
    let output = guard.text[..at].to_string();
    // Consume through the end of the sentinel line.
    let consumed = match guard.text[after..].find('\n') {
        Some(nl) => after + nl + 1,
        None => guard.text.len(),
    };
    guard.text.drain(..consumed);
    Some((output, exit))
}

fn shell_command(shell: &str, cwd: &std::path::Path, ctx: &ToolCtx) -> tokio::process::Command {
    // An interactive-style shell without a TTY: reads commands from stdin, no prompt.
    let mut cmd = tokio::process::Command::new(shell);
    cmd.current_dir(cwd)
        .env_clear()
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The same filtered development environment one-shot `bash` gets.
    for k in [
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
    ] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    for (k, v) in ctx.child_env.iter() {
        cmd.env(k, v);
    }
    cmd.env("PAGER", "cat").env("GIT_PAGER", "cat");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd
}

#[cfg(test)]
mod marker_tests {
    use super::*;
    #[test]
    fn incomplete_marker_waits_for_status_and_newline() {
        let buf = Arc::new(Mutex::new(Buffer::default()));
        buf.lock().unwrap().add(b"output\nTOKEN");
        assert!(take_until_marker(&buf, "TOKEN").is_none());
        buf.lock().unwrap().add(b" 2");
        assert!(take_until_marker(&buf, "TOKEN").is_none());
        buf.lock().unwrap().add(b"\n");
        assert_eq!(
            take_until_marker(&buf, "TOKEN"),
            Some(("output\n".into(), 2))
        );
    }
}
