//! MCP client over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Scope, stated plainly: one server process per configured entry, `initialize`
//! handshake, `tools/list`, `tools/call`, text content joined as the tool result.
//! Resources, prompts, sampling, and Streamable-HTTP transport are not implemented —
//! the config schema accepts only stdio servers.
//!
//! Tool results from MCP servers are third-party content: the adapter marks outputs
//! with a source prefix so the model treats them as data, not instructions.

use async_trait::async_trait;
use loon_config::McpServerConfig;
use loon_tools::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct McpClient {
    server: String,
    child: Mutex<Child>,
    tx: Mutex<mpsc::Sender<String>>,
    resp_rx: Mutex<mpsc::Receiver<Value>>,
    next_id: AtomicU64,
    timeout: Duration,
    protocol_version: String,
    server_info: Option<(String, String)>,
}

impl McpClient {
    /// Spawn the server process and complete the initialize handshake.
    pub async fn spawn(server: &str, cfg: &McpServerConfig) -> Result<McpClient, String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("mcp `{server}` spawn: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("mcp `{server}`: no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("mcp `{server}`: no stdout"))?;

        // Reader task: one line = one JSON-RPC message; responses forwarded by id.
        // Notifications and server-initiated requests carry no id and are dropped —
        // recorded nowhere, acted on never.
        let (resp_tx, resp_rx) = mpsc::channel::<Value>(64);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(v) => {
                        if v.get("id").is_some() && resp_tx.send(v).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => continue, // non-JSON line: tolerated, dropped
                }
            }
        });

        let mut tx = mpsc::channel::<String>(16);
        // Writer task owns stdin so requests never interleave mid-line.
        tokio::spawn(async move {
            let mut w = stdin;
            while let Some(line) = tx.1.recv().await {
                if w.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if w.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = w.flush().await;
            }
        });

        let mut client = McpClient {
            server: server.to_string(),
            child: Mutex::new(child),
            tx: Mutex::new(tx.0),
            resp_rx: Mutex::new(resp_rx),
            next_id: AtomicU64::new(1),
            timeout: Duration::from_secs(cfg.timeout_secs),
            protocol_version: String::new(),
            server_info: None,
        };

        // initialize → response → initialized notification
        let result = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "loon", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        client.protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        client.server_info = result.get("serverInfo").map(|si| {
            (
                si.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                si.get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        });
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    pub fn server_name(&self) -> &str {
        &self.server
    }

    pub fn server_identity(&self) -> Option<&(String, String)> {
        self.server_info.as_ref()
    }

    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    async fn send_raw(&self, line: String) -> Result<(), ToolError> {
        self.tx
            .lock()
            .await
            .send(line)
            .await
            .map_err(|_| ToolError::Other(format!("mcp `{}`: writer closed", self.server)))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_raw(msg.to_string())
            .await
            .map_err(|e| e.to_string())?;
        // Correlate by id; mismatched responses are skipped (they belong to other
        // in-flight requests — MCP allows concurrency).
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut rx = self.resp_rx.lock().await;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "mcp `{}`: timeout waiting for {method}",
                    self.server
                ));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(v)) => {
                    if v.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(err) = v.get("error") {
                            return Err(format!("mcp `{}`: {method} error: {err}", self.server));
                        }
                        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // Not ours: dropped.
                }
                Ok(None) => {
                    return Err(format!(
                        "mcp `{}`: server closed its output mid-request",
                        self.server
                    ))
                }
                Err(_) => {
                    return Err(format!(
                        "mcp `{}`: timeout waiting for {method}",
                        self.server
                    ))
                }
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_raw(msg.to_string())
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, String> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for t in tools {
            let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            out.push(McpToolDef {
                server: self.server.clone(),
                name: name.to_string(),
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input_schema: t.get("inputSchema").cloned().unwrap_or(json!({})),
            });
        }
        Ok(out)
    }

    /// Returns the joined text content of the tool result. Structured/resource content
    /// is serialized as JSON with a marker, never silently dropped.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String, String> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(format!(
                "mcp `{}`: tool {name} reported an error: {}",
                self.server, result
            ));
        }
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut parts = Vec::new();
        for c in content {
            match c.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
                other => parts.push(format!(
                    "[unhandled content type {:?}: {}]",
                    other,
                    serde_json::to_string(&c).unwrap_or_default()
                )),
            }
        }
        Ok(parts.join("\n"))
    }

    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

// ─── Tool adapter ───────────────────────────────────────────────────────────────

pub struct McpTool {
    pub client: Arc<McpClient>,
    pub def: McpToolDef,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: format!("mcp_{}_{}", self.def.server, self.def.name),
            description: format!(
                "[MCP {}] {}",
                self.def.server,
                if self.def.description.is_empty() {
                    "(no description provided by server)"
                } else {
                    &self.def.description
                }
            ),
            parameters: self.def.input_schema.clone(),
            effects: Effects::Mutating, // third-party tools are untrusted until proven read-only
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let out = self
            .client
            .call_tool(&self.def.name, args)
            .await
            .map_err(ToolError::Other)?;
        Ok(ToolOutput::ok(format!(
            "[mcp result from {}]\n{}",
            self.def.server, out
        )))
    }
}

/// Connect to every configured server and enumerate their tools. A server that fails
/// to start is reported, never silently skipped.
pub async fn connect_all(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools = Vec::new();
    let mut errors = Vec::new();
    for (name, cfg) in servers {
        match McpClient::spawn(name, cfg).await {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools().await {
                    Ok(defs) => {
                        for def in defs {
                            tools.push(Arc::new(McpTool {
                                client: client.clone(),
                                def,
                            }) as Arc<dyn Tool>);
                        }
                    }
                    Err(e) => errors.push(format!("mcp `{name}`: tools/list: {e}")),
                }
            }
            Err(e) => errors.push(e),
        }
    }
    (tools, errors)
}
