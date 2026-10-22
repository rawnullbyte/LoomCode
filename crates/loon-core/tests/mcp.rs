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
        url: None,
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
            url: None,
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

#[test]
fn ports_and_explicit_paths_normalize_without_treating_commands_as_urls() {
    use loon_core::mcp::normalize_endpoint;
    assert_eq!(
        normalize_endpoint("3000").unwrap().as_deref(),
        Some("http://127.0.0.1:3000/mcp")
    );
    assert_eq!(
        normalize_endpoint("localhost:4000").unwrap().as_deref(),
        Some("http://localhost:4000/mcp")
    );
    assert_eq!(
        normalize_endpoint("https://example.org/custom")
            .unwrap()
            .as_deref(),
        Some("https://example.org/custom")
    );
    assert_eq!(normalize_endpoint("npx").unwrap(), None);
    assert!(normalize_endpoint("0").is_err());
    assert!(normalize_endpoint("65536").is_err());
}
#[tokio::test]
async fn streamable_http_negotiates_session_and_handles_json_sse_and_pagination() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut calls = Vec::new();
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let (header_end, len) = loop {
                let n = socket.read(&mut buffer).await.unwrap();
                if n == 0 {
                    panic!("request closed")
                };
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let len = headers
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    break (end + 4, len);
                }
            };
            while bytes.len() < header_end + len {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
            let request: serde_json::Value =
                serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap();
            let method = request["method"].as_str().unwrap().to_string();
            assert!(headers.contains("application/json, text/event-stream"));
            if method != "initialize" {
                assert!(headers.contains("mcp-session-id: fixture-session"));
                assert!(headers.contains("mcp-protocol-version: 2025-06-18"));
            }
            calls.push(method.clone());
            if request.get("id").is_none() {
                socket
                    .write_all(
                        b"HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await
                    .unwrap();
                continue;
            }
            let result = match method.as_str() {
                "initialize" => {
                    json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
                }
                "tools/list" if request["params"]["cursor"].is_null() => {
                    json!({"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}],"nextCursor":"page2"})
                }
                "tools/list" => {
                    json!({"tools":[{"name":"other","description":"other","inputSchema":{"type":"object"}}]})
                }
                "tools/call" => {
                    json!({"content":[{"type":"text","text":request["params"]["arguments"]["text"]}]})
                }
                _ => panic!("unexpected {method}"),
            };
            let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
            let sse = method == "tools/call";
            let body = if sse {
                format!("data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n\ndata: {response}\n\n")
            } else {
                response.to_string()
            };
            let response=format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: {}\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\n\r\n{body}",if sse {"text/event-stream"} else {"application/json"},body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            if method == "tools/call" {
                return calls;
            }
        }
        panic!("listener ended")
    });
    let client = McpClient::spawn(
        "http",
        &McpServerConfig {
            command: String::new(),
            url: Some(format!("http://{addr}/custom")),
            args: vec![],
            env: Default::default(),
            timeout_secs: 5,
        },
    )
    .await
    .unwrap();
    assert_eq!(client.list_tools().await.unwrap().len(), 2);
    assert_eq!(
        client
            .call_tool("echo", json!({"text":"hello SSE"}))
            .await
            .unwrap(),
        "hello SSE"
    );
    assert_eq!(
        server.await.unwrap(),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/list",
            "tools/call"
        ]
    );
}
