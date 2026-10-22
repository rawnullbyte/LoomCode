//! Ultra Loop: when the agent says it is done but the project's check still fails, the
//! turn must continue rather than end on the model's word.

use loon_config::LoonPaths;
use loon_core::{Agent, AgentConfig, TurnStatus};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
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

fn agent_cfg(model: &str, check: Option<&str>, rounds: u32) -> AgentConfig {
    AgentConfig {
        model_ref: model.into(),
        tool_output_bytes: 4096,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: check.map(str::to_string),
        ultra_rounds: rounds,
        permissions: loon_config::Permissions::default(),
        context_window: 128_000,
        extra_tools: Vec::new(),
    }
}

/// Build an agent whose mock answers `answers` in order, one scripted reply per turn.
fn agent(
    tmp: &std::path::Path,
    ws: &std::path::Path,
    cfg: AgentConfig,
    answers: Vec<&str>,
) -> Agent {
    let paths = test_paths(tmp);
    paths.ensure().unwrap();
    let scripts = answers
        .into_iter()
        .map(MockScript::text)
        .collect::<Vec<_>>();
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        scripts,
    )));
    Agent::new_session(
        &paths,
        cfg,
        registry,
        loon_tools::ToolRegistry::standard(),
        ws.to_path_buf(),
        true,
    )
    .unwrap()
}

#[tokio::test]
async fn a_failing_check_sends_the_agent_back_to_work() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    // The check fails until the file exists, which the "agent" never creates — so the
    // loop must exhaust its rounds and the turn must still finish honestly.
    let cfg = agent_cfg("mock/one", Some("test -f never-created.txt"), 2);
    let mut a = agent(
        tmp.path(),
        &ws,
        cfg,
        vec!["done!", "still done", "done again"],
    );

    let result = a.run_turn("finish the task").await;
    assert!(
        matches!(result.status, TurnStatus::Failed(ref reason) if reason.contains("verification")),
        "{result:?}"
    );
    // Two rounds were spent asking the model to keep going.
    assert_eq!(a.cfg.ultra_rounds, 2);
}

#[tokio::test]
async fn a_passing_check_ends_the_turn_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = agent_cfg("mock/one", Some("true"), 3);
    let mut a = agent(tmp.path(), &ws, cfg, vec!["all done"]);

    let result = a.run_turn("do it").await;
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(result.steps, 1, "no extra round was needed");
}

#[tokio::test]
async fn no_check_command_means_no_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = agent_cfg("mock/one", None, 3);
    let mut a = agent(tmp.path(), &ws, cfg, vec!["done"]);

    let result = a.run_turn("do it").await;
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(result.steps, 1);
}

/// A model that keeps sending the same invalid call must not be allowed to spin: the
/// turn ends with an honest failure instead of looping until the user cancels.
#[tokio::test]
async fn repeated_identical_invalid_calls_stop_the_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    // `read` without `path` — invalid every time.
    let bad = || {
        MockScript::new(vec![MockStep::tool_call(
            "call_1",
            "read",
            serde_json::json!({}),
        )])
    };
    let scripts = (0..8).map(|_| bad()).collect::<Vec<_>>();
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        scripts,
    )));
    let mut a = Agent::new_session(
        &paths,
        agent_cfg("mock/one", None, 0),
        registry,
        loon_tools::ToolRegistry::standard(),
        ws.to_path_buf(),
        true,
    )
    .unwrap();
    let result = a.run_turn("do something").await;
    match result.status {
        TurnStatus::Failed(reason) => assert!(
            reason.contains("repeated the same rejected tool call"),
            "{reason}"
        ),
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(
        result.steps <= 5,
        "the breaker stops the turn early, used {} steps",
        result.steps
    );
}

/// Extra keys must not cost a turn: models routinely add a stray field.
#[tokio::test]
async fn unknown_tool_arguments_are_ignored_not_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "hello\n").unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![
            MockScript::new(vec![MockStep::tool_call(
                "call_1",
                "read",
                serde_json::json!({"path": "a.txt", "reason": "I want to see it", "why": 1}),
            )]),
            MockScript::text("read it"),
        ],
    )));
    let mut a = Agent::new_session(
        &paths,
        agent_cfg("mock/one", None, 0),
        registry,
        loon_tools::ToolRegistry::standard(),
        ws.to_path_buf(),
        true,
    )
    .unwrap();
    let result = a.run_turn("look at a.txt").await;
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.status);
}

#[tokio::test]
async fn automatic_verification_replays_as_a_paired_tool_exchange() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = agent_cfg("mock/one", Some("true"), 1);
    let mut a = agent(tmp.path(), &ws, cfg, vec!["done"]);
    assert_eq!(a.run_turn("verify").await.status, TurnStatus::Completed);
    let resumed = Agent::resume(
        &a.loon_paths(),
        a.session_id(),
        a.cfg.clone(),
        a.registry.clone(),
        a.tools.clone(),
    )
    .unwrap();
    for (i, m) in resumed.history.iter().enumerate() {
        if m.role == loon_providers::Role::Tool {
            let id = m.tool_call_id.as_deref().unwrap();
            assert!(resumed.history[..i]
                .iter()
                .rev()
                .find(|m| m.role != loon_providers::Role::Tool)
                .unwrap()
                .tool_calls
                .iter()
                .any(|c| c.id == id));
        }
    }
}
