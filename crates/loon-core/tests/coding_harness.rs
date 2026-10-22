use loon_config::{LoonPaths, SessionCfg};
use loon_core::{Agent, AgentConfig, AgentEvent};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
use serde_json::json;
use std::sync::Arc;
fn paths(root: &std::path::Path) -> LoonPaths {
    LoonPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        runtime_dir: root.join("runtime"),
    }
}
fn agent(root: &std::path::Path, scripts: Vec<MockScript>) -> (LoonPaths, Agent) {
    let paths = paths(root);
    paths.ensure().unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        scripts,
    )));
    let cfg = AgentConfig::from(&SessionCfg::default());
    let tools = loon_core::subagent::default_tools(&paths, &cwd, &cfg, registry.clone(), None);
    let a = Agent::new_session(&paths, cfg, registry, tools, cwd, true).unwrap();
    (paths, a)
}
#[tokio::test]
async fn durable_todos_notes_and_task_evidence_survive_resume_without_workspace_approval() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(
        d.path(),
        vec![
            MockScript::new(vec![MockStep::tool_call(
                "todo",
                "todo_write",
                json!({"todos":[{"content":"Inspect requirement","status":"completed"},{"content":"Implement and verify","status":"in_progress"}]}),
            )]),
            MockScript::new(vec![MockStep::tool_call(
                "note",
                "note",
                json!({"action":"add","text":"src/parser.rs: Unicode error reproduced; regression command cargo test parser"}),
            )]),
            MockScript::new(vec![MockStep::tool_call(
                "task",
                "task",
                json!({"action":"create","subject":"Verify parser","description":"Acceptance: Unicode preserved; regression command cargo test parser"}),
            )]),
            MockScript::text("working state recorded"),
        ],
    );
    a.set_permission_mode(loon_config::PermissionMode::Ask);
    let result = a.run_turn("fix and verify parser").await;
    assert_eq!(result.status, loon_core::TurnStatus::Completed);
    let state = a.worklog.read().unwrap();
    assert_eq!(state.plan.len(), 2);
    assert_eq!(state.notes.len(), 1);
    assert_eq!(state.tasks.len(), 1);
    assert!(state.render().contains("Acceptance: Unicode preserved"));
    assert!(a
        .history
        .iter()
        .filter(|m| m.role == loon_providers::Role::Tool)
        .all(|m| !m.text.contains("PERMISSION")));
    let a = Agent::resume(
        &paths,
        a.session_id(),
        a.cfg.clone(),
        a.registry.clone(),
        loon_tools::ToolRegistry::standard(),
    )
    .unwrap();
    let out = a
        .tools
        .get("todo_read")
        .unwrap()
        .run(json!({}), &a.ctx)
        .await
        .unwrap();
    assert!(out.text.contains("src/parser.rs"));
    assert!(out.text.contains("in_progress"));
    assert!(out.text.contains("Acceptance: Unicode preserved"));
}
#[tokio::test]
async fn bundled_skills_and_coding_tools_are_available_without_files_or_plugins() {
    let d = tempfile::tempdir().unwrap();
    let (_, a) = agent(d.path(), vec![]);
    assert!(a.tools.len() >= 40, "{} tools", a.tools.len());
    assert_eq!(a.skills.iter().filter(|s| s.scope == "bundled").count(), 26);
    for name in [
        "rust",
        "python",
        "typescript",
        "go",
        "frontend",
        "backend",
        "migrations",
        "docs-research",
        "test-repair",
        "release",
    ] {
        let out = a
            .tools
            .get("load_skill")
            .unwrap()
            .run(json!({"name":name}), &a.ctx)
            .await
            .unwrap();
        assert!(out.text.contains("todo_write"));
        assert!(out.text.contains("Task:"));
        assert!(!out.text.starts_with("---"));
    }
    for tool in [
        "TodoWrite",
        "todo_read",
        "context7_search",
        "context7_resolve_library",
        "context7_query_docs",
        "git_diff",
        "json_edit",
        "notebook_edit",
        "run_tests",
        "project_info",
    ] {
        assert!(a.tools.get(tool).is_some(), "missing {tool}");
    }
    std::fs::create_dir_all(a.ctx.ws.root().join(".loon/skills")).unwrap();
    std::fs::write(
        a.ctx.ws.root().join(".loon/skills/rust.md"),
        "workspace Rust conventions",
    )
    .unwrap();
    let out = a
        .tools
        .get("load_skill")
        .unwrap()
        .run(json!({"name":"rust"}), &a.ctx)
        .await
        .unwrap();
    assert!(out.text.contains("workspace Rust conventions"));
}
#[tokio::test]
async fn request_usage_is_total_output_and_finishes_before_tool_execution() {
    let d = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(
        d.path(),
        vec![
            MockScript::new(vec![
                MockStep::Usage(loon_events::model::Usage {
                    input_tokens: Some(900),
                    output_tokens: Some(120),
                    reasoning_tokens: Some(80),
                    cached_input_tokens: None,
                }),
                MockStep::tool_call("r", "tree", json!({"depth":1})),
            ]),
            MockScript::new(vec![
                MockStep::Text("done".into()),
                MockStep::Usage(loon_events::model::Usage {
                    input_tokens: Some(950),
                    output_tokens: Some(7),
                    reasoning_tokens: Some(0),
                    cached_input_tokens: None,
                }),
            ]),
        ],
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    a.set_event_sink(tx);
    a.run_turn("inspect files").await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RequestStarted))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::RequestFinished))
            .count(),
        2
    );
    let totals: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::RequestUsage { output_tokens } = e {
                Some(*output_tokens)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(totals, vec![120, 7]);
    let finish = events
        .iter()
        .position(|e| matches!(e, AgentEvent::RequestFinished))
        .unwrap();
    let tool = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolStarted { .. }))
        .unwrap();
    assert!(finish < tool);
}
#[tokio::test]
async fn context7_configuration_reports_presence_without_exposing_the_secret() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(d.path(), vec![]);
    let result = a
        .run_slash(loon_core::commands::parse("/context7").unwrap().unwrap())
        .await
        .unwrap();
    assert!(result.contains("CONTEXT7_API_KEY"));
    a.config_apply("context7_key_env", "LOOM_DOCS_TOKEN")
        .unwrap();
    assert_eq!(
        loon_config::Config::load_or_builtin(&paths)
            .unwrap()
            .0
            .context7
            .api_key_env,
        "LOOM_DOCS_TOKEN"
    );
    assert_eq!(a.setting_value("context7_key_env"), "LOOM_DOCS_TOKEN");
}

/// `/sessions search` prints one hit per line. Titles stored before the one-line
/// rule can contain newlines, so the listing must flatten them rather than let a
/// single title forge extra hits.
#[tokio::test]
async fn sessions_search_flattens_a_stored_multiline_title() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(d.path(), vec![MockScript::text("done")]);
    a.run_turn("find the parser bug").await;

    // A legacy-style title, written straight into the journal.
    let store = loon_core::open_store(&paths).unwrap();
    let journal = loon_events::Journal::open_default(&store, &paths.sessions_dir(), a.session_id())
        .unwrap()
        .0;
    journal.rename_session("legacy\nsecond line").unwrap();

    let out = a
        .run_slash(
            loon_core::commands::parse("/sessions search parser")
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !out.contains("second line\n"),
        "a stored newline must not become its own line, got: {out:?}"
    );
    assert!(
        out.contains("legacy second line"),
        "the title should appear flattened, got: {out:?}"
    );
}

/// `/find` prints one hit per line, so a multi-line message must be flattened
/// into its hit rather than continuing onto the following lines.
#[tokio::test]
async fn search_flattens_a_multiline_message() {
    let d = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(d.path(), vec![MockScript::text("done")]);
    a.run_turn("first line\nsecond line parser").await;

    let out = a
        .run_slash(loon_core::commands::parse("/find parser").unwrap().unwrap())
        .await
        .unwrap();
    assert!(
        !out.contains("second line parser\n"),
        "a multi-line message must not break the hit list, got: {out:?}"
    );
    assert!(
        out.contains("first line second line parser"),
        "the hit should be one flattened line, got: {out:?}"
    );
}
