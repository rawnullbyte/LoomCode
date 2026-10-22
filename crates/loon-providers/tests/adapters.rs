//! Adapter acceptance tests (A03, A04): both wire protocols complete a tool cycle over a
//! real HTTP fixture; hostile streams (malformed chunks, missing usage, dropped
//! completion) produce classified failures instead of silent wrong behavior.

mod support;

use loon_providers::chat::ChatCompletionsAdapter;
use loon_providers::http::{Endpoint, HttpTransport};
use loon_providers::responses::ResponsesAdapter;
use loon_providers::{ChatMessage, ChatRequest, FinishReason, ProviderEvent};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

fn endpoint_of(base: &str) -> Endpoint {
    Endpoint::from_parts(base, None, &Default::default(), Duration::from_secs(10))
}

async fn drain(mut rx: mpsc::Receiver<ProviderEvent>) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn chat_completions_completes_tool_cycle() {
    // A03: fragmented streamed tool call, finish_reason=tool_calls, usage in final chunk.
    let frames = vec![
        serde_json::json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"Let me look."}}]}).to_string(),
        // tool call split across three frames
        serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}).to_string(),
        serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": \"src/main.rs\"}"}}]}}]}).to_string(),
        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":4}}}).to_string(),
    ];
    let fixture =
        support::Fixture::start(vec![(200, "text/event-stream", support::sse_body(&frames))]).await;
    let http = Arc::new(HttpTransport::new().unwrap());
    let adapter = ChatCompletionsAdapter::new(http);
    let req = ChatRequest {
        model: "test-model".into(),
        messages: vec![ChatMessage::user("hi")],
        tools: vec![
            serde_json::json!({"type":"function","function":{"name":"read","parameters":{}}}),
        ],
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel(64);
    adapter
        .stream(&endpoint_of(&fixture.base_url), &req, tx)
        .await
        .expect("stream ok");
    let events = drain(rx).await;

    let complete = events.iter().find_map(|e| match e {
        ProviderEvent::ToolCallComplete { call } => Some(call.clone()),
        _ => None,
    });
    let call = complete.expect("tool call completed");
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "read");
    assert_eq!(call.arguments_json, r#"{"path": "src/main.rs"}"#);
}

#[tokio::test]
async fn chat_completions_usage_and_finish() {
    let frames = vec![
        serde_json::json!({"choices":[{"index":0,"delta":{"content":"Hello"}}]}).to_string(),
        serde_json::json!({"choices":[{"index":0,"delta":{"content":" world"},"finish_reason":"stop"}]}).to_string(),
        serde_json::json!({"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}).to_string(),
    ];
    let fixture =
        support::Fixture::start(vec![(200, "text/event-stream", support::sse_body(&frames))]).await;
    let adapter = ChatCompletionsAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .unwrap();
    let events = drain(rx).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Usage(u) if u.input_tokens == Some(5) && u.output_tokens == Some(2))));
    assert!(events.iter().any(|e| matches!(
        e,
        ProviderEvent::Finished {
            finish: FinishReason::Stop
        }
    )));
}

#[tokio::test]
async fn chat_missing_usage_stays_absent_not_zero() {
    let frames = vec![
        serde_json::json!({"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}]})
            .to_string(),
    ];
    let fixture =
        support::Fixture::start(vec![(200, "text/event-stream", support::sse_body(&frames))]).await;
    let adapter = ChatCompletionsAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .unwrap();
    let events = drain(rx).await;
    assert!(
        !events.iter().any(|e| matches!(e, ProviderEvent::Usage(_))),
        "no fabricated usage"
    );
}

#[tokio::test]
async fn chat_malformed_chunk_is_classified() {
    let body = b"data: {not json}\n\ndata: [DONE]\n\n".to_vec();
    let fixture = support::Fixture::start(vec![(200, "text/event-stream", body)]).await;
    let adapter = ChatCompletionsAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    let err = adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .expect_err("malformed chunk must error");
    assert!(
        matches!(err, loon_providers::ProviderError::Malformed(_)),
        "{err}"
    );
    drop(rx);
}

#[tokio::test]
async fn chat_empty_keepalive_frames_are_skipped_not_fatal() {
    // Real gateways interleave empty `data:` heartbeats and whitespace-only frames.
    // These must be skipped, not reported as "chunk is not JSON".
    let body = b"data: \n\ndata:   \n\n: keepalive\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_vec();
    let fixture = support::Fixture::start(vec![(200, "text/event-stream", body)]).await;
    let adapter = ChatCompletionsAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .expect("empty keepalive frames must be skipped");
    let events = drain(rx).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            ProviderEvent::TextDelta { text } if text == "hi"
        )),
        "text after keepalives is delivered: {events:?}"
    );
}

#[tokio::test]
async fn responses_empty_keepalive_frames_are_skipped_not_fatal() {
    let body = b"data: \n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"yo\"}\n\ndata: {\"type\":\"response.completed\"}\n\n".to_vec();
    let fixture = support::Fixture::start(vec![(200, "text/event-stream", body)]).await;
    let adapter = ResponsesAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .expect("empty keepalive frames must be skipped");
    let events = drain(rx).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            ProviderEvent::TextDelta { text } if text == "yo"
        )),
        "text after keepalives is delivered: {events:?}"
    );
}

#[tokio::test]
async fn chat_stream_ended_without_terminator_is_dropped() {
    // No finish_reason, no [DONE] — endpoint died mid-stream.
    let body =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n".to_vec();
    let fixture = support::Fixture::start(vec![(200, "text/event-stream", body)]).await;
    let adapter = ChatCompletionsAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    let err = adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .expect_err("must classify as dropped");
    assert!(
        matches!(err, loon_providers::ProviderError::Dropped(_)),
        "{err}"
    );
    drop(rx);
}

#[tokio::test]
async fn chat_http_error_status_classified() {
    let fixture = support::Fixture::start(vec![(
        401,
        "application/json",
        b"{\"error\":{\"message\":\"bad key\"}}".to_vec(),
    )])
    .await;
    let adapter = ChatCompletionsAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, _rx) = mpsc::channel(64);
    let err = adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .expect_err("401 must classify");
    assert!(
        matches!(err, loon_providers::ProviderError::Auth(_)),
        "{err}"
    );
}

#[tokio::test]
async fn endpoint_ignoring_stream_gets_non_streamed_response() {
    let body = serde_json::json!({
        "choices": [{
            "index": 0,
            "message": {"role":"assistant","content":"plain answer","tool_calls":[
                {"id":"call_9","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}
            ]},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 9}
    })
    .to_string()
    .into_bytes();
    let fixture = support::Fixture::start(vec![(200, "application/json", body)]).await;
    let adapter = ChatCompletionsAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .unwrap();
    let events = drain(rx).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::TextDelta { text } if text == "plain answer")));
    assert!(events.iter().any(|e| matches!(e,
        ProviderEvent::ToolCallComplete { call } if call.name == "bash" && call.arguments_json.contains("ls"))));
}

#[tokio::test]
async fn responses_api_completes_tool_cycle() {
    // A03 (Responses flavor): typed events, arguments.done, usage in response.completed.
    let frames = vec![
        serde_json::json!({"type":"response.output_text.delta","delta":"Checking."}).to_string(),
        serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_ab","name":"edit","arguments":""}}).to_string(),
        serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"a.rs\",\"new_"}).to_string(),
        serde_json::json!({"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"a.rs\",\"new_string\":\"x\"}"}).to_string(),
        serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_ab","name":"edit","arguments":"{\"path\":\"a.rs\",\"new_string\":\"x\"}"}}).to_string(),
        serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":20,"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2}}}}).to_string(),
    ];
    let fixture =
        support::Fixture::start(vec![(200, "text/event-stream", support::sse_body(&frames))]).await;
    let adapter = ResponsesAdapter::new(Arc::new(HttpTransport::new().unwrap()));
    let (tx, rx) = mpsc::channel(64);
    adapter
        .stream(&endpoint_of(&fixture.base_url), &ChatRequest::default(), tx)
        .await
        .unwrap();
    let events = drain(rx).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::TextDelta { text } if text == "Checking.")));
    let call = events
        .iter()
        .find_map(|e| match e {
            ProviderEvent::ToolCallComplete { call } => Some(call.clone()),
            _ => None,
        })
        .expect("function_call completed");
    assert_eq!(call.id, "call_ab");
    assert_eq!(call.name, "edit");
    assert_eq!(call.arguments_json, r#"{"path":"a.rs","new_string":"x"}"#);
    assert!(events
        .iter()
        .any(|e| matches!(e, ProviderEvent::Usage(u) if u.input_tokens == Some(20) && u.reasoning_tokens == Some(2))));
    assert!(events.iter().any(|e| matches!(
        e,
        ProviderEvent::Finished {
            finish: FinishReason::Stop
        }
    )));
}

#[tokio::test]
async fn responses_body_shape_uses_items() {
    // The Responses adapter must speak Responses wire format, not Chat Completions.
    let body = ResponsesAdapter::build_body(&ChatRequest {
        model: "m".into(),
        messages: vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("hello"),
            ChatMessage::tool_result("call_1", "done"),
            ChatMessage::assistant("did it"),
        ],
        tools: vec![
            serde_json::json!({"type":"function","function":{"name":"t","description":"d","parameters":{}}}),
        ],
        ..Default::default()
    });
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
    assert_eq!(body["input"][2]["type"], "function_call_output");
    assert_eq!(body["input"][2]["call_id"], "call_1");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "t");
    assert_eq!(body["store"], false);
    assert!(
        body.get("messages").is_none(),
        "no chat-completions leakage"
    );
}

#[tokio::test]
async fn transport_retains_every_frame_in_a_coalesced_network_chunk() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = (0..30)
        .map(|i| format!("data: {i}\n\n"))
        .collect::<String>();
    let expected = body.len();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let response=format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {expected}\r\nconnection: close\r\n\r\n{body}");
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let http = HttpTransport::new().unwrap();
    let mut stream = http
        .post_sse(
            &endpoint_of(&format!("http://{addr}")),
            "/",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
    let mut frames = Vec::new();
    while let Some(frame) = stream.next_frame(Duration::from_secs(2)).await.unwrap() {
        frames.push(frame.data);
    }
    assert_eq!(frames, (0..30).map(|i| i.to_string()).collect::<Vec<_>>());
}
#[test]
fn responses_serializes_flat_call_and_output_items() {
    let req = ChatRequest {
        messages: vec![
            ChatMessage::user("inspect"),
            ChatMessage {
                role: loon_providers::Role::Assistant,
                text: "Reading now".into(),
                tool_calls: vec![loon_providers::ToolCallReq {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments_json: r#"{"path":"a"}"#.into(),
                }],
                tool_call_id: None,
            },
            ChatMessage::tool_result("c1", "contents"),
            ChatMessage::assistant("done"),
        ],
        ..Default::default()
    };
    let body = ResponsesAdapter::build_body(&req);
    let items = body["input"].as_array().unwrap();
    assert!(items.iter().all(|v| v.is_object()));
    assert_eq!(items[2]["type"], "function_call");
    assert_eq!(items[3]["type"], "function_call_output");
    assert_eq!(items[2]["call_id"], items[3]["call_id"]);
}
