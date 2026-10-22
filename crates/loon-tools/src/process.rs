//! Managed background jobs for dev servers and builds; bounded logs and teardown.
use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;

#[derive(Default)]
pub struct ProcessTool {
    jobs: Mutex<BTreeMap<u64, Job>>,
    next: std::sync::atomic::AtomicU64,
}
struct Job {
    command: String,
    log: Arc<Mutex<Log>>,
    status: Arc<Mutex<Option<i32>>>,
    stop: tokio::sync::watch::Sender<bool>,
}
impl Drop for Job {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
#[derive(Default)]
struct Log {
    text: String,
    dropped: usize,
}
impl Log {
    fn add(&mut self, bytes: &[u8]) {
        self.text.push_str(&String::from_utf8_lossy(bytes));
        if self.text.len() > 256 * 1024 {
            let mut cut = self.text.len() - 256 * 1024;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
        }
    }
}
#[async_trait]
impl Tool for ProcessTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "process".into(), description: "Manage a long-running shell job: start a server/build, poll its bounded stdout/stderr log and exit status, stop it, or list jobs. Jobs are owned by this tool registry and cleaned up when it closes. Use bash for short commands. Poll with the returned job_id and cursor to read only new output; this is pipe-based, not an interactive PTY.".into(),
            parameters: json!({"type":"object","properties":{
                "action":{"type":"string","enum":["start","poll","stop","list"]},
                "command":{"type":"string","minLength":1},"cwd":{"type":"string"},
                "job_id":{"type":"integer","minimum":1},"cursor":{"type":"integer","minimum":0}
            },"required":["action"],"additionalProperties":false}), effects:Effects::Mutating }
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let action = args["action"].as_str().unwrap_or("");
        if action == "start" {
            let command = args["command"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| ToolError::InvalidArgs("start requires command".into()))?
                .to_string();
            if self
                .jobs
                .lock()
                .unwrap()
                .values()
                .filter(|j| j.status.lock().unwrap().is_none())
                .count()
                >= 16
            {
                return Err(ToolError::Other("16 active jobs; stop one first".into()));
            }
            let cwd = ctx.ws.resolve(args["cwd"].as_str().unwrap_or("."))?;
            let shell = if std::path::Path::new("/bin/bash").exists() {
                "/bin/bash"
            } else {
                "/bin/sh"
            };
            let mut cmd = crate::exec::development_command(shell, &command, &cwd, ctx);
            let mut child = cmd.spawn()?;
            let out = child.stdout.take().expect("stdout");
            let err = child.stderr.take().expect("stderr");
            let log = Arc::new(Mutex::new(Log::default()));
            let status = Arc::new(Mutex::new(None));
            let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
            let log_out = log.clone();
            let log_err = log.clone();
            let status_run = status.clone();
            let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            self.jobs.lock().unwrap().insert(
                id,
                Job {
                    command,
                    log,
                    status,
                    stop,
                },
            );
            tokio::spawn(async move {
                let mut group = crate::exec::ProcessGroup::new(child.id());
                let out_task = tokio::spawn(pump(out, log_out));
                let err_task = tokio::spawn(pump(err, log_err));
                let code = tokio::select! {
                    result=child.wait()=>result.ok().and_then(|s|s.code()).unwrap_or(-1),
                    _=stop_rx.changed()=>{group.terminate();let _=child.wait().await;-1}
                };
                // Descendants may still own the pipes. Close the whole job tree.
                group.terminate();
                let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while !out_task.is_finished() || !err_task.is_finished() {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                out_task.abort();
                err_task.abort();
                *status_run.lock().unwrap() = Some(code);
            });
            return Ok(ToolOutput {
                ok: true,
                text: format!("started job {id}; use process poll with job_id={id}"),
                full: None,
                truncated: false,
                structured: json!({"job_id":id,"running":true,"cursor":0}),
            });
        }
        let jobs = self.jobs.lock().unwrap();
        if action == "list" {
            return Ok(ToolOutput::ok(
                jobs.iter()
                    .map(|(id, j)| {
                        format!(
                            "{id}: {} — {}",
                            j.status
                                .lock()
                                .unwrap()
                                .map(|c| format!("exit {c}"))
                                .unwrap_or("running".into()),
                            j.command
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }
        let id = args["job_id"]
            .as_u64()
            .ok_or_else(|| ToolError::InvalidArgs("poll/stop requires job_id".into()))?;
        let job = jobs
            .get(&id)
            .ok_or_else(|| ToolError::Other(format!("unknown job {id}")))?;
        if action == "stop" {
            let _ = job.stop.send(true);
            return Ok(ToolOutput::ok(format!("stop requested for job {id}")));
        }
        if action != "poll" {
            return Err(ToolError::InvalidArgs(
                "action must be start/poll/stop/list".into(),
            ));
        }
        let log = job.log.lock().unwrap();
        let code = *job.status.lock().unwrap();
        let cursor = args["cursor"].as_u64().unwrap_or(log.dropped as u64) as usize;
        let mut start = cursor.saturating_sub(log.dropped).min(log.text.len());
        while !log.text.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + ctx.output_limit).min(log.text.len());
        while !log.text.is_char_boundary(end) {
            end -= 1;
        }
        let text = format!(
            "{}\n[job {id}: {}; cursor {}{}]",
            &log.text[start..end],
            code.map(|c| format!("exit {c}"))
                .unwrap_or("running".into()),
            log.dropped + end,
            if cursor < log.dropped {
                "; older logs evicted"
            } else {
                ""
            }
        );
        Ok(ToolOutput {
            ok: code.map(|c| c == 0).unwrap_or(true),
            text,
            full: None,
            truncated: end < log.text.len(),
            structured: json!({"job_id":id,"running":code.is_none(),"exit_code":code,"cursor":log.dropped+end}),
        })
    }
}
async fn pump(mut pipe: impl tokio::io::AsyncRead + Unpin, log: Arc<Mutex<Log>>) {
    let mut buf = [0u8; 4096];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => log.lock().unwrap().add(&buf[..n]),
        }
    }
}
