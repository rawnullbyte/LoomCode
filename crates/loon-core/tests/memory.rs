//! Memory end to end: the `memory` tool persists facts through the real agent loop
//! (mock provider, labeled as such), the next session's system prompt carries them,
//! and ask-mode gates memory writes like any other mutation.

use loon_config::{LoonPaths, PermissionMode, Permissions};
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
async fn model_can_save_memory_and_next_session_sees_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(&home);
    paths.ensure().unwrap();

    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::new(vec![
            MockStep::tool_call(
                "call_1",
                "memory",
                json!({"fact": "the team runs tests with cargo nextest", "scope": "workspace"}),
            ),
        ])]),
    ));
    let mut agent = Agent::new_session(
        &paths,
        AgentConfig {
            model_ref: "mock".into(),
            max_steps: 8,
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            permissions: Default::default(),
            context_window: 128_000,
            extra_tools: Vec::new(),
            check_command: None,
        },
        registry,
        loon_core::standard_tools(&paths, &ws),
        ws.clone(),
        true,
    )
    .unwrap();
    let r = agent.run_turn("remember how we run tests").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));

    let mem = loon_core::memory::MemoryPaths::resolve(&paths.data_dir, &ws);
    let text = loon_core::memory::read_scope(&mem, loon_core::memory::Scope::Workspace);
    assert!(
        text.contains("cargo nextest"),
        "fact persisted, got: {text}"
    );

    // A fresh session's system prompt carries the fact.
    let prompt = loon_core::prompt::system_prompt_with_memory(&ws, &mem);
    assert!(prompt.contains("cargo nextest"));
}

#[tokio::test]
async fn ask_mode_requires_approval_for_memory_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(&home);
    paths.ensure().unwrap();

    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::new(vec![
            MockStep::tool_call("call_1", "memory", json!({"fact": "should not be saved"})),
        ])]),
    ));
    let mut agent = Agent::new_session(
        &paths,
        AgentConfig {
            model_ref: "mock".into(),
            max_steps: 8,
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            permissions: Permissions {
                mode: PermissionMode::Ask,
                ..Permissions::default()
            },
            context_window: 128_000,
            extra_tools: Vec::new(),
            check_command: None,
        },
        registry,
        loon_core::standard_tools(&paths, &ws),
        ws.clone(),
        true,
    )
    .unwrap();
    // Headless ask mode: no hook ⇒ not approved ⇒ not saved.
    let r = agent.run_turn("remember this").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    let mem = loon_core::memory::MemoryPaths::resolve(&paths.data_dir, &ws);
    assert!(
        !mem.workspace.exists(),
        "unapproved memory write must not persist"
    );

    // With an approving hook it does persist.
    let registry2 = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::new(vec![
            MockStep::tool_call("call_1", "memory", json!({"fact": "approved fact saved"})),
        ])]),
    ));
    let mut agent2 = Agent::new_session(
        &paths,
        AgentConfig {
            model_ref: "mock".into(),
            max_steps: 8,
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            permissions: Permissions {
                mode: PermissionMode::Ask,
                ..Permissions::default()
            },
            context_window: 128_000,
            extra_tools: Vec::new(),
            check_command: None,
        },
        registry2,
        loon_core::standard_tools(&paths, &ws),
        ws.clone(),
        true,
    )
    .unwrap();
    agent2.approval_hook = Some(Arc::new(|_req| Box::pin(async { true })));
    let r2 = agent2.run_turn("remember this").await;
    assert!(matches!(r2.status, loon_core::TurnStatus::Completed));
    let text = loon_core::memory::read_scope(&mem, loon_core::memory::Scope::Workspace);
    assert!(text.contains("approved fact saved"));
}

#[tokio::test]
async fn memory_tool_reports_invalid_args_and_unknown_scope_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(&tmp.path().join("home"));
    paths.ensure().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let reg = loon_core::standard_tools(&paths, &ws);
    let tool = reg.get("memory").expect("memory tool registered");
    let ctx = tool_ctx(&ws);
    // missing fact
    assert!(tool.run(json!({}), &ctx).await.is_err());
    // empty fact
    assert!(tool.run(json!({"fact": "   "}), &ctx).await.is_err());
    // unknown scope falls back to workspace
    let out = tool
        .run(
            json!({"fact": "default scope fact", "scope": "bogus"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.ok);
    let mem = loon_core::memory::MemoryPaths::resolve(&paths.data_dir, &ws);
    let text = loon_core::memory::read_scope(&mem, loon_core::memory::Scope::Workspace);
    assert!(text.contains("default scope fact"));
}

fn tool_ctx(ws: &std::path::Path) -> loon_tools::ToolCtx {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    loon_tools::ToolCtx {
        ws: Arc::new(loon_workspace::Workspace::new(ws, vec![]).unwrap()),
        output_limit: 4096,
        default_timeout: std::time::Duration::from_secs(10),
        child_env: Default::default(),
        cancel: rx,
        blob_sink: None,
    }
}
