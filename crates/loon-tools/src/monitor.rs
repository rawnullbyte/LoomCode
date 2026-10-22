//! Background monitors: watch a long-running process (dev server, test watcher, log tail)
//! and poll its output incrementally, with pattern matching so the agent can wait for a
//! specific line instead of sleeping and re-reading everything.
//!
//! Distinct from `process`: a monitor is built for *observation* — bounded ring log,
//! per-monitor cursors, `wait_for` with a timeout, and a matched-line report. It also
//! tracks the exit status so a crashed server is visible rather than silently idle.

use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

const MAX_MONITORS: usize = 16;
const MAX_LOG: usize = 256 * 1024;

pub struct MonitorTool {
    monitors: Mutex<BTreeMap<u64, Monitor>>,
    next: AtomicU64,
}

impl Default for MonitorTool {
    fn default() -> Self {
        MonitorTool {
            monitors: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
        }
    }
}

struct Monitor {
    label: String,
    command: String,
    log: Arc<Mutex<Log>>,
    status: Arc<Mutex<Option<i32>>>,
    stop: tokio::sync::watch::Sender<bool>,
    started: Instant,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

#[derive(Default)]
struct Log {
    text: String,
    /// Bytes dropped from the front (keeps cursors meaningful after eviction).
    dropped: usize,
}

impl Log {
    fn add(&mut self, bytes: &[u8]) {
        self.text.push_str(&String::from_utf8_lossy(bytes));
        if self.text.len() > MAX_LOG {
            let mut cut = self.text.len() - MAX_LOG;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
        }
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "monitor".into(),
            description: "Watch a long-running process (dev server, test watcher, log tail) without blocking: start a monitor, then poll for new output, wait for a pattern with a timeout, read a bounded log, or stop it. Each poll returns only new output plus a cursor, and the exit status once the process ends, so a crashed server is visible instead of silently idle. Use `process` for fire-and-forget jobs and `bash` for short commands.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["start", "poll", "wait_for", "logs", "stop", "list"]},
                    "command": {"type": "string", "minLength": 1, "description": "start: shell command to run and watch."},
                    "label": {"type": "string", "description": "start: human-readable name (defaults to the command)."},
                    "cwd": {"type": "string"},
                    "monitor_id": {"type": "integer", "minimum": 1},
                    "cursor": {"type": "integer", "minimum": 0, "description": "Byte cursor from a previous poll; omit to read from the start."},
                    "pattern": {"type": "string", "description": "wait_for: regular expression to wait for."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600},
                    "max_bytes": {"type": "integer", "minimum": 1}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            effects: Effects::Mutating,
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let action = args["action"].as_str().unwrap_or_default();
        match action {
            "start" => self.start(&args, ctx).await,
            "poll" => self.read(&args, ctx, false),
            "logs" => self.read(&args, ctx, true),
            "wait_for" => self.wait_for(&args, ctx).await,
            "stop" => self.stop(&args),
            "list" => Ok(self.list()),
            other => Err(ToolError::InvalidArgs(format!(
                "action must be start/poll/wait_for/logs/stop/list, got `{other}`"
            ))),
        }
    }
}

impl MonitorTool {
    async fn start(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = args["command"]
            .as_str()
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("start requires command".into()))?
            .to_string();
        {
            let monitors = self.monitors.lock().unwrap();
            let active = monitors
                .values()
                .filter(|m| m.status.lock().unwrap().is_none())
                .count();
            if active >= MAX_MONITORS {
                return Err(ToolError::Other(format!(
                    "{MAX_MONITORS} active monitors; stop one first"
                )));
            }
        }
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
        let shell = if std::path::Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        let mut cmd = crate::exec::development_command(shell, &command, &cwd, ctx);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Other(format!("failed to start {shell}: {e}")))?;
        let out = child.stdout.take().expect("stdout piped");
        let err = child.stderr.take().expect("stderr piped");
        let log = Arc::new(Mutex::new(Log::default()));
        let status = Arc::new(Mutex::new(None));
        let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let label = args["label"]
            .as_str()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(&command)
            .to_string();

        let log_out = log.clone();
        let log_err = log.clone();
        let status_run = status.clone();
        self.monitors.lock().unwrap().insert(
            id,
            Monitor {
                label: label.clone(),
                command: command.clone(),
                log,
                status,
                stop,
                started: Instant::now(),
            },
        );
        tokio::spawn(async move {
            let mut group = crate::exec::ProcessGroup::new(child.id());
            let out_task = tokio::spawn(pump(out, log_out));
            let err_task = tokio::spawn(pump(err, log_err));
            let code = tokio::select! {
                result = child.wait() => result.ok().and_then(|s| s.code()).unwrap_or(-1),
                _ = stop_rx.changed() => { group.terminate(); let _ = child.wait().await; -1 }
            };
            group.terminate();
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                while !out_task.is_finished() || !err_task.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            out_task.abort();
            err_task.abort();
            *status_run.lock().unwrap() = Some(code);
        });
        Ok(ToolOutput {
            ok: true,
            text: format!("monitor {id} started: {label}"),
            full: None,
            truncated: false,
            structured: json!({"monitor_id": id, "label": label, "running": true, "cursor": 0}),
        })
    }

    /// `poll` returns only new bytes; `logs` returns the retained tail regardless.
    fn read(&self, args: &Value, ctx: &ToolCtx, whole_log: bool) -> Result<ToolOutput, ToolError> {
        let id = monitor_id(args)?;
        let monitors = self.monitors.lock().unwrap();
        let monitor = monitors
            .get(&id)
            .ok_or_else(|| ToolError::Other(format!("unknown monitor {id}")))?;
        let log = monitor.log.lock().unwrap();
        let code = *monitor.status.lock().unwrap();
        let max = args["max_bytes"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(ctx.output_limit);
        let (start, note) = if whole_log {
            let start = log.text.len().saturating_sub(max);
            (start, "")
        } else {
            match args["cursor"].as_u64() {
                // No cursor: this is the first read, so start at the beginning.
                None => (0, ""),
                Some(cursor) => {
                    let evicted = cursor < log.dropped as u64;
                    (
                        (cursor as usize)
                            .saturating_sub(log.dropped)
                            .min(log.text.len()),
                        if evicted {
                            "; older output evicted"
                        } else {
                            ""
                        },
                    )
                }
            }
        };
        let mut start = start;
        while !log.text.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + max).min(log.text.len());
        while !log.text.is_char_boundary(end) {
            end -= 1;
        }
        let body = &log.text[start..end];
        let text = format!(
            "{body}\n[monitor {id} `{}`: {}; cursor {}{note}]",
            monitor.label,
            code.map(|c| format!("exit {c}"))
                .unwrap_or_else(|| "running".into()),
            log.dropped + end
        );
        Ok(ToolOutput {
            ok: code.map(|c| c == 0).unwrap_or(true),
            text,
            full: None,
            truncated: end < log.text.len(),
            structured: json!({
                "monitor_id": id,
                "label": monitor.label,
                "running": code.is_none(),
                "exit_code": code,
                "cursor": log.dropped + end,
                "age_secs": monitor.started.elapsed().as_secs(),
            }),
        })
    }

    /// Block until the pattern appears or the timeout expires. Reports the matching
    /// lines and the new cursor, or an honest timeout (never a fabricated match).
    async fn wait_for(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let id = monitor_id(args)?;
        let pattern = args["pattern"]
            .as_str()
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("wait_for requires pattern".into()))?;
        let re = regex::Regex::new(pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad pattern: {e}")))?;
        let timeout =
            Duration::from_secs(args["timeout_secs"].as_u64().unwrap_or(60).clamp(1, 600));
        let deadline = tokio::time::Instant::now() + timeout;
        let mut wait = ctx.cancel.clone();
        loop {
            {
                let monitors = self.monitors.lock().unwrap();
                let monitor = monitors
                    .get(&id)
                    .ok_or_else(|| ToolError::Other(format!("unknown monitor {id}")))?;
                let log = monitor.log.lock().unwrap();
                let code = *monitor.status.lock().unwrap();
                let cursor = args["cursor"].as_u64().unwrap_or(log.dropped as u64) as usize;
                let from = cursor.saturating_sub(log.dropped).min(log.text.len());
                let mut from = from;
                while !log.text.is_char_boundary(from) {
                    from += 1;
                }
                let window = &log.text[from..];
                if let Some(m) = re.find(window) {
                    let line_start = window[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let line_end = window[m.end()..]
                        .find('\n')
                        .map(|i| m.end() + i)
                        .unwrap_or(window.len());
                    let line = window[line_start..line_end].trim().to_string();
                    return Ok(ToolOutput {
                        ok: true,
                        text: format!(
                            "matched in monitor {id} `{}`: {line}\n[cursor {}]",
                            monitor.label,
                            log.dropped + from + line_end
                        ),
                        full: None,
                        truncated: false,
                        structured: json!({
                            "monitor_id": id,
                            "matched": true,
                            "line": line,
                            "cursor": log.dropped + from + line_end,
                        }),
                    });
                }
                if let Some(c) = code {
                    // The process ended without the pattern: report that, do not wait.
                    return Ok(ToolOutput {
                        ok: false,
                        text: format!(
                            "monitor {id} `{}` exited ({c}) without matching `{pattern}`\n{}",
                            monitor.label,
                            tail(window, 4000)
                        ),
                        full: None,
                        truncated: false,
                        structured: json!({"monitor_id": id, "matched": false, "exit_code": c}),
                    });
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(ToolOutput {
                    ok: false,
                    text: format!(
                        "timed out after {}s waiting for `{pattern}` in monitor {id}; it is still running",
                        timeout.as_secs()
                    ),
                    full: None,
                    truncated: false,
                    structured: json!({"monitor_id": id, "matched": false, "timed_out": true}),
                });
            }
            tokio::select! {
                _ = tokio::time::sleep(deadline - now) => {}
                _ = crate::exec::cancel_wait(&mut wait) => {
                    return Ok(ToolOutput {
                        ok: false,
                        text: format!("cancelled while waiting for `{pattern}`"),
                        full: None,
                        truncated: false,
                        structured: json!({"monitor_id": id, "matched": false, "cancelled": true}),
                    });
                }
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }

    fn stop(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let id = monitor_id(args)?;
        let monitors = self.monitors.lock().unwrap();
        let monitor = monitors
            .get(&id)
            .ok_or_else(|| ToolError::Other(format!("unknown monitor {id}")))?;
        let _ = monitor.stop.send(true);
        Ok(ToolOutput::ok(format!("stop requested for monitor {id}")))
    }

    fn list(&self) -> ToolOutput {
        let monitors = self.monitors.lock().unwrap();
        if monitors.is_empty() {
            return ToolOutput::ok("no monitors; start one with action=start");
        }
        let rows: Vec<String> = monitors
            .iter()
            .map(|(id, m)| {
                format!(
                    "{id}: {} — {} `{}` ({}s)",
                    m.status
                        .lock()
                        .unwrap()
                        .map(|c| format!("exit {c}"))
                        .unwrap_or_else(|| "running".into()),
                    m.label,
                    m.command,
                    m.started.elapsed().as_secs()
                )
            })
            .collect();
        ToolOutput::ok(rows.join("\n"))
    }
}

fn monitor_id(args: &Value) -> Result<u64, ToolError> {
    args["monitor_id"]
        .as_u64()
        .ok_or_else(|| ToolError::InvalidArgs("monitor_id is required".into()))
}

fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[…]{}\n", &text[start..])
}

async fn pump(mut pipe: impl tokio::io::AsyncRead + Unpin, log: Arc<Mutex<Log>>) {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => log.lock().unwrap().add(&buf[..n]),
        }
    }
}
