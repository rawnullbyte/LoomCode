//! Persistent SSH connections: OpenSSH connection multiplexing, so repeated commands to
//! the same host reuse one authenticated TCP session instead of reconnecting (and
//! re-handshaking, and re-typing the key passphrase) every call.
//!
//! How: each named connection gets a ControlMaster (`ssh -M -N -S <socket>`). Every
//! command is then `ssh -S <socket> …`, which rides the existing connection. `close`
//! tears the master down. Authentication is delegated to OpenSSH — Loon never handles,
//! stores or logs keys or passwords.
//!
//! Honest limits: needs OpenSSH on the host; if a master cannot be established (no
//! agent, key prompt required) the tool reports the failure and does not pretend a
//! connection exists.

use crate::tool::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 8;

pub struct SshTool {
    connections: Mutex<BTreeMap<String, Connection>>,
}

impl Default for SshTool {
    fn default() -> Self {
        SshTool {
            connections: Mutex::new(BTreeMap::new()),
        }
    }
}

struct Connection {
    /// `user@host[:port]` as given.
    target: String,
    /// Control socket for this master.
    socket: PathBuf,
    /// Master process; kept so it can be terminated on close/drop.
    master: tokio::process::Child,
    created: Instant,
    commands: u64,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = std::process::Command::new("ssh")
            .arg("-S")
            .arg(&self.socket)
            .arg("-O")
            .arg("exit")
            .arg(&self.target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[async_trait]
impl Tool for SshTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ssh".into(),
            description: "Run commands on a remote host over ONE reused OpenSSH connection (ControlMaster multiplexing): connect once, then every run rides the same authenticated session instead of reconnecting. Actions: connect (name, host, optional user/port/identity), run (name, command, timeout_secs), close (name or all), list, plus tunnel-style local_forward is not supported. Authentication is OpenSSH's own (agent/keys/config); Loon never sees secrets, and a failure to establish the master is reported instead of faked. Requires ssh on PATH.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["connect", "run", "close", "list"]},
                    "name": {"type": "string", "minLength": 1, "description": "Connection name; defaults to the host."},
                    "host": {"type": "string", "minLength": 1, "description": "Hostname or IP (or a Host alias from ~/.ssh/config)."},
                    "user": {"type": "string"},
                    "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                    "identity": {"type": "string", "description": "Path to a private key file (optional)."},
                    "command": {"type": "string", "minLength": 1},
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
        match action {
            "connect" => self.connect(&args, ctx).await,
            "run" => self.run_on(&args, ctx).await,
            "close" => self.close(&args),
            "list" => Ok(self.list()),
            other => Err(ToolError::InvalidArgs(format!(
                "action must be connect/run/close/list, got `{other}`"
            ))),
        }
    }
}

impl SshTool {
    async fn connect(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let host = args["host"]
            .as_str()
            .filter(|h| !h.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("connect requires host".into()))?;
        // A host beginning with `-` is not a hostname; ssh would read it as an option.
        // `--` already stops that at the argv level, but refusing it here names the real
        // problem instead of letting ssh report a confusing usage error.
        if host.starts_with('-') {
            return Err(ToolError::InvalidArgs(format!(
                "host `{host}` must not begin with `-` (that is an ssh option, not a hostname)"
            )));
        }
        // The user field is passed as `-l USER`, so a leading `-` would be read as the
        // argument of `-l` in a way we did not intend; reject it for the same reason.
        if args["user"].as_str().is_some_and(|u| u.starts_with('-')) {
            return Err(ToolError::InvalidArgs(
                "user must not begin with `-`".into(),
            ));
        }
        let target = match (args["user"].as_str(), args["port"].as_u64()) {
            (Some(user), Some(port)) => format!("{user}@{host}:{port}"),
            (Some(user), None) => format!("{user}@{host}"),
            (None, Some(port)) => format!("{host}:{port}"),
            (None, None) => host.to_string(),
        };
        let name = args["name"]
            .as_str()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(host)
            .to_string();
        {
            let connections = self.connections.lock().unwrap();
            if let Some(existing) = connections.get(&name) {
                return Ok(ToolOutput {
                    ok: true,
                    text: format!(
                        "connection `{name}` already open to {} ({} command(s), {}s)",
                        existing.target,
                        existing.commands,
                        existing.created.elapsed().as_secs()
                    ),
                    full: None,
                    truncated: false,
                    structured: json!({"name": name, "reused": true, "target": existing.target}),
                });
            }
            if connections.len() >= MAX_CONNECTIONS {
                return Err(ToolError::Other(format!(
                    "connection limit reached ({MAX_CONNECTIONS}); close one first"
                )));
            }
        }

        // A control socket must be short enough for sun_path (~104 bytes); put it in the
        // session-scoped cache dir the host already owns.
        let socket = short_socket_path(&name);
        if let Some(parent) = socket.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&socket);

        let mut cmd = tokio::process::Command::new("ssh");
        cmd.arg("-M") // master mode
            .arg("-N") // no command: just hold the connection
            .arg("-o")
            .arg("BatchMode=yes") // never hang on a password prompt
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("ServerAliveInterval=30")
            .arg("-o")
            .arg("ControlPersist=600")
            .arg("-S")
            .arg(&socket);
        if let Some(port) = args["port"].as_u64() {
            cmd.arg("-p").arg(port.to_string());
        }
        if let Some(identity) = args["identity"].as_str() {
            cmd.arg("-i").arg(identity);
        }
        if let Some(user) = args["user"].as_str() {
            cmd.arg("-l").arg(user);
        }
        // `--` ends option parsing. Without it a host beginning with `-` is read as an
        // ssh *option*: `--host "-oProxyCommand=..."` ran an arbitrary command before any
        // permission decision, since ssh executes ProxyCommand locally while connecting.
        cmd.arg("--")
            .arg(host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in ctx.child_env.iter() {
            cmd.env(k, v);
        }

        let mut master = cmd.spawn().map_err(|e| {
            ToolError::Other(format!("ssh is not runnable: {e} (install OpenSSH client)"))
        })?;

        // The master reports readiness by creating the socket. Wait briefly, and surface
        // ssh's own stderr when it fails (unknown host, auth, refused).
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stderr = String::new();
        loop {
            if socket.exists() {
                break;
            }
            if let Some(status) = master.try_wait().ok().flatten() {
                if let Some(mut pipe) = master.stderr.take() {
                    stderr.push_str(&String::from_utf8_lossy(&drain(&mut pipe).await));
                }
                return Ok(ToolOutput {
                    ok: false,
                    text: format!(
                        "could not establish an SSH master to {target} (exit {:?}):\n{}",
                        status.code(),
                        stderr.trim()
                    ),
                    full: None,
                    truncated: false,
                    structured: json!({"name": name, "target": target, "connected": false}),
                });
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !socket.exists() {
            let _ = master.start_kill();
            return Ok(ToolOutput {
                ok: false,
                text: format!(
                    "timed out establishing an SSH master to {target}; \
                     check that the host is reachable and key-based auth works"
                ),
                full: None,
                truncated: false,
                structured: json!({"name": name, "target": target, "connected": false}),
            });
        }

        let pid = master.id();
        self.connections.lock().unwrap().insert(
            name.clone(),
            Connection {
                target: target.clone(),
                socket: socket.clone(),
                master,
                created: Instant::now(),
                commands: 0,
            },
        );
        Ok(ToolOutput {
            ok: true,
            text: format!(
                "connected `{name}` to {target} (master pid {:?}); later runs reuse this session",
                pid
            ),
            full: None,
            truncated: false,
            structured: json!({"name": name, "target": target, "pid": pid, "connected": true}),
        })
    }

    async fn run_on(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = args["command"]
            .as_str()
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("run requires command".into()))?
            .to_string();
        let name = {
            let connections = self.connections.lock().unwrap();
            match args["name"].as_str().filter(|n| !n.trim().is_empty()) {
                Some(n) => n.to_string(),
                // One connection open: use it, so the model need not remember the name.
                None if connections.len() == 1 => connections.keys().next().cloned().unwrap(),
                None => {
                    return Err(ToolError::InvalidArgs(
                        "run requires name (multiple connections are open)".into(),
                    ))
                }
            }
        };
        let timeout = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .map(Duration::from_secs)
            .unwrap_or(ctx.default_timeout)
            .min(Duration::from_secs(600));

        let (target, socket) = {
            let connections = self.connections.lock().unwrap();
            let conn = connections.get(&name).ok_or_else(|| {
                ToolError::Other(format!("no connection `{name}`; call action=connect first"))
            })?;
            (conn.target.clone(), conn.socket.clone())
        };
        if !socket.exists() {
            self.connections.lock().unwrap().remove(&name);
            return Err(ToolError::Other(format!(
                "connection `{name}` is gone (master exited); connect again"
            )));
        }

        let mut cmd = tokio::process::Command::new("ssh");
        cmd.arg("-S")
            .arg(&socket)
            .arg("-o")
            .arg("BatchMode=yes")
            // `--` before the target: a stored host that begins with `-` must never be
            // re-interpreted as an ssh option (`-oProxyCommand=...` runs locally).
            .arg("--")
            .arg(&target)
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in ctx.child_env.iter() {
            cmd.env(k, v);
        }
        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|e| {
            ToolError::Other(format!("ssh is not runnable: {e} (install OpenSSH client)"))
        })?;
        let mut out_pipe = child.stdout.take().expect("stdout");
        let mut err_pipe = child.stderr.take().expect("stderr");
        let read_out = tokio::spawn(async move { drain(&mut out_pipe).await });
        let read_err = tokio::spawn(async move { drain(&mut err_pipe).await });
        let mut cancel = ctx.cancel.clone();
        let status = tokio::select! {
            s = child.wait() => Some(s.map_err(ToolError::Io)?),
            _ = tokio::time::sleep(timeout) => None,
            _ = crate::exec::cancel_wait(&mut cancel) => None,
        };
        let timed_out = status.is_none();
        if timed_out {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        let out = tokio::time::timeout(Duration::from_secs(2), read_out)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let err = tokio::time::timeout(Duration::from_secs(2), read_err)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let duration = started.elapsed();
        if let Some(conn) = self.connections.lock().unwrap().get_mut(&name) {
            conn.commands += 1;
        }

        let exit = status.as_ref().and_then(|s| s.code());
        let mut text = String::new();
        if timed_out {
            text.push_str(&format!(
                "[TIMEOUT after {timeout:?}: ssh command killed]\n"
            ));
        }
        let stdout = String::from_utf8_lossy(&out).to_string();
        let stderr = String::from_utf8_lossy(&err).to_string();
        if !stdout.is_empty() {
            text.push_str(&stdout);
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        if !stderr.is_empty() {
            text.push_str("--- stderr ---\n");
            text.push_str(&stderr);
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        text.push_str(&format!(
            "[exit {}: {target} over reused connection `{name}` ({:.2}s)]",
            exit.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            duration.as_secs_f64()
        ));
        let (bounded, full, truncated) = ctx.bound(text);
        Ok(ToolOutput {
            ok: !timed_out && exit == Some(0),
            text: bounded,
            full,
            truncated,
            structured: json!({
                "name": name,
                "target": target,
                "exit_code": exit,
                "timed_out": timed_out,
                "reused": true,
            }),
        })
    }

    fn close(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let name = args["name"].as_str().unwrap_or("all");
        let mut connections = self.connections.lock().unwrap();
        if name == "all" {
            let names: Vec<String> = connections.keys().cloned().collect();
            connections.clear();
            return Ok(ToolOutput::ok(if names.is_empty() {
                "no open connections".to_string()
            } else {
                format!("closed connections: {}", names.join(", "))
            }));
        }
        match connections.remove(name) {
            Some(_) => Ok(ToolOutput::ok(format!("closed connection `{name}`"))),
            None => Err(ToolError::Other(format!("no connection `{name}`"))),
        }
    }

    fn list(&self) -> ToolOutput {
        let connections = self.connections.lock().unwrap();
        if connections.is_empty() {
            return ToolOutput::ok("no open connections; connect to a host first");
        }
        let rows: Vec<String> = connections
            .iter()
            .map(|(name, c)| {
                format!(
                    "{name}: {} (pid {:?}, {} command(s), {}s old)",
                    c.target,
                    c.master.id(),
                    c.commands,
                    c.created.elapsed().as_secs()
                )
            })
            .collect();
        ToolOutput::ok(rows.join("\n"))
    }
}

async fn drain(pipe: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf).await;
    buf
}

/// Unix socket paths are limited to ~104 bytes, so the name is hashed rather than used
/// verbatim (a long hostname would otherwise make `ssh -S` fail).
fn short_socket_path(name: &str) -> PathBuf {
    let digest = loon_workspace::sha256_hex(name.as_bytes());
    let base = std::env::temp_dir();
    base.join(format!("loon-ssh-{}.sock", &digest[..16]))
}
