//! MCP client against a real spawned fixture server (a small Python process speaking
//! newline-delimited JSON-RPC). Covers the handshake, tools/list, tools/call text
//! content, error propagation, and connect_all failures for a missing command.

use loon_config::McpServerConfig;
use loon_core::mcp::{connect_all, McpClient};
use serde_json::json;
use std::collections::BTreeMap;

/// A minimal MCP server: answers initialize, tools/list, tools/call with canned
/// responses. Requires python3; the test skips honestly when it is absent.
fn fixture_script(echo_prefix: &str) -> String {
    r#"import json, sys
prefix = "ECHO_PREFIX_PLACEHOLDER"
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if "id" not in req:
        continue  # notification
    method = req.get("method")
    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": req["id"], "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "0.1"}}}
    elif method == "tools/list":
        resp = {"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [
            {"name": "echo", "description": "Echo the input",
              "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}}
    elif method == "tools/call":
        text = (req.get("params", {}).get("arguments", {}).get("text", ""))
        if text == "fail":
            resp = {"jsonrpc": "2.0", "id": req["id"], "result": {
                "isError": True, "content": [{"type": "text", "text": "boom"}]}}
        else:
            resp = {"jsonrpc": "2.0", "id": req["id"], "result": {
                "content": [{"type": "text", "text": prefix + text}]}}
    else:
        resp = {"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "no such method"}}
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#
    .replace("ECHO_PREFIX_PLACEHOLDER", echo_prefix)
}

fn server_cfg(script_body: &str) -> McpServerConfig {
    McpServerConfig {
        command: "python3".into(),
        args: vec!["-I".into(), "-c".into(), script_body.to_string()],
        env: Default::default(),
        timeout_secs: 10,
    }
}

async fn python3_available() -> bool {
    tokio::process::Command::new("python3")
        .arg("-c")
        .arg("print(1)")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn handshake_list_and_call_round_trip() {
    if !python3_available().await {
        eprintln!("skipping: python3 unavailable in this environment");
        return;
    }
    let cfg = server_cfg(&fixture_script("echo: "));
    let client = McpClient::spawn("fixture", &cfg)
        .await
        .expect("handshake must complete");
    assert_eq!(client.protocol_version(), "2024-11-05");
    assert_eq!(
        client.server_identity().map(|(n, _)| n.as_str()),
        Some("fixture")
    );

    let tools = client.list_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].server, "fixture");

    let out = client
        .call_tool("echo", json!({"text": "hello"}))
        .await
        .expect("tools/call");
    assert_eq!(out, "echo: hello");
    client.shutdown().await;
}

#[tokio::test]
async fn tool_errors_surface_not_swallowed() {
    if !python3_available().await {
        eprintln!("skipping: python3 unavailable in this environment");
        return;
    }
    let cfg = server_cfg(&fixture_script(""));
    let client = McpClient::spawn("fixture", &cfg).await.unwrap();
    let err = client.call_tool("echo", json!({"text": "fail"})).await;
    assert!(err.is_err(), "isError=true must surface as an error");
    let msg = err.unwrap_err();
    assert!(msg.contains("reported an error"), "{msg}");
    client.shutdown().await;
}

#[tokio::test]
async fn connect_all_reports_dead_servers_instead_of_dropping_them() {
    let mut servers: BTreeMap<String, McpServerConfig> = BTreeMap::new();
    servers.insert(
        "missing".into(),
        McpServerConfig {
            command: "/nonexistent/loon-mcp-server".into(),
            args: vec![],
            env: Default::default(),
            timeout_secs: 5,
        },
    );
    let (tools, errors) = connect_all(&servers).await;
    assert!(tools.is_empty());
    assert_eq!(errors.len(), 1, "a dead server is an error, not silence");
    assert!(errors[0].contains("missing"), "{}", errors[0]);
}
