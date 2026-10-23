//! The status bar's context figure is fed by `AgentEvent::ContextUpdated`. It used to
//! be emitted only when the turn ENDED, so a long turn displayed the size it started
//! with. These tests drive a real turn and assert the refresh happens mid-turn.

use loon_config::LoonPaths;
use loon_core::{Agent, AgentConfig, AgentEvent};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
use serde_json::json;
use std::sync::Arc;

fn test_paths(dir: &std::path::Path) -> LoonPaths {
    LoonPaths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
        state_dir: dir.join("state"),
        cache_dir: dir.join("cache"),
        runtime_dir: dir.join("runtime"),
    }
}

fn cfg(model: &str, tool_output_bytes: usize) -> AgentConfig {
    AgentConfig {
        model_ref: model.into(),
        tool_output_bytes,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: None,
        ultra_rounds: 0,
        permissions: Default::default(),
        context_window: 200_000,
        extra_tools: Vec::new(),
    }
}

/// A two-step turn: one scripted tool call, then a final answer.
async fn two_step_agent(
    tmp: &tempfile::TempDir,
    file: &str,
    content: &str,
    tool_output_bytes: usize,
) -> (Agent, tokio::sync::mpsc::Receiver<AgentEvent>) {
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join(file), content).unwrap();
    let scripts = vec![
        MockScript::new(vec![MockStep::tool_call(
            "call_1",
            "read",
            json!({"path": file}),
        )]),
        MockScript::text("done"),
    ];
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        scripts,
    )));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let cfg = cfg("mock", tool_output_bytes);
    let tools = loon_core::subagent::default_tools(&paths, &ws, &cfg, registry.clone(), None);
    let mut agent = Agent::new_session(&paths, cfg, registry, tools, ws, true).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    agent.set_event_sink(tx);
    (agent, rx)
}

/// Collect the relevant event order until the turn finishes.
async fn trace(mut rx: tokio::sync::mpsc::Receiver<AgentEvent>) -> Vec<&'static str> {
    let mut tags = Vec::new();
    while let Some(ev) = rx.recv().await {
        let (tag, done) = match ev {
            AgentEvent::ContextUpdated { .. } => ("context", false),
            AgentEvent::ToolFinished { .. } => ("tool", false),
            AgentEvent::RequestStarted => ("request", false),
            AgentEvent::TurnFinished { .. } => ("finished", true),
            _ => continue,
        };
        tags.push(tag);
        if done {
            break;
        }
    }
    tags
}

#[tokio::test]
async fn the_context_size_is_refreshed_before_the_next_request() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut agent, rx) = two_step_agent(&tmp, "data.txt", &"x".repeat(4000), 4096).await;
    let (result, tags) = tokio::join!(agent.run_turn("read data.txt and summarise"), trace(rx));
    assert!(matches!(result.status, loon_core::TurnStatus::Completed));

    // Two requests run. The second one is assembled AFTER the tool result, so the
    // refresh must already have happened by then — that is what keeps the bar live.
    let second_request = tags
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == "request")
        .nth(1)
        .map(|(i, _)| i)
        .expect("the scripted turn makes two requests");
    let refreshed_before = tags[..second_request].contains(&"context");
    assert!(
        refreshed_before,
        "the context size must be refreshed before the request that follows a tool \
         result; observed order: {tags:?}"
    );
}

#[tokio::test]
async fn the_reported_size_grows_as_tool_output_enters_context() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut agent, mut rx) =
        two_step_agent(&tmp, "big.txt", &"abcdefgh".repeat(2000), 64_000).await;
    let collector = async {
        let mut sizes = Vec::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::ContextUpdated { tokens } => sizes.push(tokens),
                AgentEvent::TurnFinished { .. } => break,
                _ => {}
            }
        }
        sizes
    };
    let (result, sizes) = tokio::join!(agent.run_turn("read big.txt"), collector);
    assert!(matches!(result.status, loon_core::TurnStatus::Completed));
    assert!(
        sizes.len() >= 2,
        "expected an update before and after the tool result, got {sizes:?}"
    );
    assert!(
        *sizes.last().unwrap() > *sizes.first().unwrap(),
        "the figure must grow once the file content is in context: {sizes:?}"
    );
}
