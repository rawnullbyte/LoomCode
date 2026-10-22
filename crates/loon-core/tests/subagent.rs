//! Subagent delegation end to end (mock provider, labeled as such): the parent's
//! single request scripts a `delegate` call; the child's request consumes the next
//! mock script and its final text comes back as the tool result.

use loon_config::LoonPaths;
use loon_core::{Agent, AgentConfig};
use loon_providers::{MockScript, MockStep, ProviderRegistry};
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

#[tokio::test]
async fn delegate_runs_a_child_session_and_returns_its_report() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }").unwrap();

    // Script order (FIFO per request): 1) parent calls delegate,
    // 2) child answers from its own turn, 3) parent final text.
    let scripts = vec![
        MockScript::new(vec![MockStep::tool_call(
            "call_1",
            "delegate",
            json!({"task": "read src/lib.rs and report what answer() returns"}),
        )]),
        MockScript::text("answer() returns 42"),
        MockScript::text("the subagent reports: answer() returns 42"),
    ];
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(scripts),
    ));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let cfg = AgentConfig {
        model_ref: "mock".into(),
        tool_output_bytes: 4096,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: None,
        ultra_rounds: 0,
        permissions: Default::default(),
        context_window: 128_000,
        extra_tools: Vec::new(),
    };
    let tools = loon_core::subagent::default_tools(&paths, &ws, &cfg, registry.clone(), None);
    let mut agent = Agent::new_session(&paths, cfg, registry, tools, ws.clone(), true).unwrap();
    let r = agent.run_turn("find out what answer() returns").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    assert!(
        r.assistant_text.contains("subagent reports"),
        "{}",
        r.assistant_text
    );

    // The child session is journaled as its own session in the same store.
    let store = loon_core::open_store(&paths).unwrap();
    let sessions = store.list_sessions().unwrap();
    assert!(
        sessions.len() >= 2,
        "parent + child sessions journaled, got {}",
        sessions.len()
    );
}

#[tokio::test]
async fn child_session_has_no_delegate_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(Arc::new(
        loon_providers::MockProvider::new(),
    )));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let cfg = AgentConfig::from(&loon_config::SessionCfg::default());
    let tools = loon_core::subagent::default_tools(&paths, &ws, &cfg, registry.clone(), None);
    let agent = Agent::new_session(&paths, cfg, registry, tools, ws.clone(), true).unwrap();
    let names: Vec<String> = agent.tools.specs().iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"delegate".to_string()));
    assert!(names.contains(&"memory".to_string()));
    // The child toolset built inside delegate_tool is standard + memory only:
    let child = loon_tools::ToolRegistry::standard().with_extra(vec![]);
    let child_names: Vec<String> = child.specs().iter().map(|s| s.name.clone()).collect();
    assert!(!child_names.contains(&"delegate".to_string()));
    assert!(!child_names.contains(&"memory".to_string()));
}
