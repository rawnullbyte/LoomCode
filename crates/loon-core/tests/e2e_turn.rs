//! Milestone-1 vertical slice: one complete agent turn — read → edit (CAS-protected) →
//! verify via shell → final answer — against the deterministic mock provider, with the
//! journal asserted end to end. This is the "one verified coding task" of M1; it uses a
//! scripted provider, which is stated as such (no live-model claim).

use loon_config::LoonPaths;
use loon_core::{Agent, AgentConfig};
use loon_events::model::EventKind;
use loon_providers::{MockScript, MockStep, ProviderRegistry};
use loon_tools::ToolRegistry;
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

fn agent_cfg(model: &str) -> AgentConfig {
    AgentConfig {
        model_ref: model.into(),
        tool_output_bytes: 4096,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: None,
        ultra_rounds: 0,
        permissions: loon_config::Permissions::default(),
        context_window: 128_000,
        extra_tools: Vec::new(),
    }
}

#[tokio::test]
async fn verified_coding_task_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let initial = "total = 1 + 1\nprint(total)\n";
    std::fs::write(ws.join("calc.txt"), initial).unwrap();

    // The model must pass the hash it read back for CAS protection.
    let (_bytes, hash) = {
        let data = std::fs::read(ws.join("calc.txt")).unwrap();
        (data.clone(), loon_workspace::sha256_hex(&data))
    };

    let mock_scripts = vec![
        // 1) read the file
        MockScript::new(vec![MockStep::tool_call(
            "call_1",
            "read",
            json!({"path": "calc.txt"}),
        )]),
        // 2) apply the fix with CAS hash from the read
        MockScript::new(vec![MockStep::tool_call(
            "call_2",
            "edit",
            json!({"path": "calc.txt", "old_string": "total = 1 + 1", "new_string": "total = 2 + 2", "expect_sha256": hash}),
        )]),
        // 3) verify by running it
        MockScript::new(vec![MockStep::tool_call(
            "call_3",
            "bash",
            json!({"command": "cat calc.txt", "timeout_secs": 15}),
        )]),
        // 4) report completion
        MockScript::text(
            "Fixed the total (1+1 → 2+2) and verified by cat: output shows total = 2 + 2.",
        ),
    ];
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(mock_scripts),
    ));

    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        agent_cfg("mock"),
        registry,
        ToolRegistry::standard(),
        ws.clone(),
        true,
    )
    .unwrap();

    let result = agent.run_turn("Fix the total in calc.txt").await;

    // ── workspace effect ─────────────────────────────────────────────────────────
    let after = std::fs::read_to_string(ws.join("calc.txt")).unwrap();
    assert_eq!(after, "total = 2 + 2\nprint(total)\n", "edit applied");

    // ── turn status ──────────────────────────────────────────────────────────────
    assert!(
        matches!(result.status, loon_core::TurnStatus::Completed),
        "{:?}",
        result.status
    );
    assert!(
        result.assistant_text.contains("verified"),
        "final report present"
    );
    assert_eq!(result.steps, 4, "read, edit, verify, answer");

    // ── journal is complete and ordered ─────────────────────────────────────────
    let store = loon_core::open_store(&paths).unwrap();
    let (_, events) =
        loon_events::Journal::open_default(&store, &paths.sessions_dir(), agent.session_id())
            .unwrap();
    let kinds: Vec<&EventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(kinds
        .iter()
        .any(|k| matches!(k, EventKind::UserMessage { text } if text.contains("Fix the total"))));
    assert_eq!(
        kinds
            .iter()
            .filter(|k| matches!(k, EventKind::ToolCallObserved { name, .. } if name == "read"))
            .count(),
        1
    );
    assert!(kinds.iter().any(
        |k| matches!(k, EventKind::ToolExecutionFinished { name, ok: true, .. } if name == "edit")
    ));
    assert!(kinds.iter().any(
        |k| matches!(k, EventKind::ToolExecutionFinished { name, ok: true, .. } if name == "bash")
    ));
    // every request got a start and a finish record
    assert_eq!(
        kinds
            .iter()
            .filter(|k| matches!(k, EventKind::RequestStarted { .. }))
            .count(),
        kinds
            .iter()
            .filter(|k| matches!(k, EventKind::RequestFinished { .. }))
            .count()
    );
    assert!(kinds
        .iter()
        .any(|k| matches!(k, EventKind::TurnFinished { .. })));
    // bash verification evidence contains the post-edit content
    let bash_out = events.iter().find_map(|e| match &e.kind {
        EventKind::ToolExecutionFinished {
            name,
            excerpt,
            ok: true,
            ..
        } if name == "bash" => Some(excerpt.clone()),
        _ => None,
    });
    assert!(
        bash_out.unwrap().contains("total = 2 + 2"),
        "verification evidence recorded"
    );
}

#[tokio::test]
async fn stale_cas_write_is_refused_and_reported_to_model() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("f.txt"), "hello").unwrap();

    // Hash the file, then a "user" edits it concurrently before the agent acts.
    // The user's edit keeps the anchor text so the tool reaches the CAS check itself.
    let data = std::fs::read(ws.join("f.txt")).unwrap();
    let stale_hash = loon_workspace::sha256_hex(&data);
    std::fs::write(ws.join("f.txt"), "user changed hello to something else").unwrap();

    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![
            MockScript::new(vec![MockStep::tool_call(
                "call_1",
                "edit",
                json!({"path": "f.txt", "old_string": "hello", "new_string": "agent edit", "expect_sha256": stale_hash}),
            )]),
            MockScript::text("I stopped: the file changed under me; please re-check."),
        ]),
    ));

    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        agent_cfg("mock"),
        registry,
        ToolRegistry::standard(),
        ws.clone(),
        true,
    )
    .unwrap();
    let result = agent.run_turn("edit f.txt").await;

    // user work preserved
    assert_eq!(
        std::fs::read_to_string(ws.join("f.txt")).unwrap(),
        "user changed hello to something else"
    );
    assert!(matches!(result.status, loon_core::TurnStatus::Completed));
    let store = loon_core::open_store(&paths).unwrap();
    let (_, events) =
        loon_events::Journal::open_default(&store, &paths.sessions_dir(), agent.session_id())
            .unwrap();
    let refused = events.iter().any(|e| {
        matches!(&e.kind,
            EventKind::ToolExecutionFinished { name, ok: false, excerpt, .. }
                if name == "edit" && excerpt.contains("changed since it was read"))
    });
    assert!(
        refused,
        "CAS refusal must be journaled as a failed tool run"
    );
}

#[tokio::test]
async fn cancellation_stops_between_steps() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::new(vec![
            MockStep::tool_call("call_1", "bash", json!({"command": "sleep 0.5"})),
            MockStep::tool_call("call_2", "bash", json!({"command": "echo should-not-run"})),
            MockStep::Text("never".into()),
        ])]),
    ));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let agent = Arc::new(tokio::sync::Mutex::new(
        Agent::new_session(
            &paths,
            agent_cfg("mock"),
            registry,
            ToolRegistry::standard(),
            ws.clone(),
            true,
        )
        .unwrap(),
    ));

    // Cancel while the turn is running (Esc mid-turn): through the lock-free handle,
    // exactly like the UI does — the lock itself is held by the running turn.
    let cancel_handle = agent.lock().await.cancel_handle();
    let runner = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.lock().await.run_turn("go").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _ = cancel_handle.send(true);
    let result = runner.await.unwrap();
    assert!(matches!(result.status, loon_core::TurnStatus::Cancelled));

    // The leftover flag must not poison the NEXT session's turn (queue-then-cancel
    // flows rely on this).
    let registry2 = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::text("fresh turn runs")]),
    ));
    let mut agent2 = Agent::new_session(
        &paths,
        agent_cfg("mock"),
        registry2,
        ToolRegistry::standard(),
        ws,
        true,
    )
    .unwrap();
    let r2 = agent2.run_turn("next").await;
    assert!(
        matches!(r2.status, loon_core::TurnStatus::Completed),
        "a stale cancel flag must not poison the next turn, got {:?}",
        r2.status
    );
}

#[tokio::test]
async fn tool_argument_validation_rejects_bad_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![
            MockScript::new(vec![MockStep::tool_call(
                "call_bad",
                "read",
                json!({"path": 42}), // wrong type
            )]),
            MockScript::new(vec![MockStep::tool_call(
                "call_unknown",
                "teleport",
                json!({}),
            )]),
            MockScript::text("both calls were rejected; understood."),
        ]),
    ));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        agent_cfg("mock"),
        registry,
        ToolRegistry::standard(),
        ws,
        true,
    )
    .unwrap();
    let result = agent.run_turn("try bad calls").await;
    assert!(matches!(result.status, loon_core::TurnStatus::Completed));
    let store = loon_core::open_store(&paths).unwrap();
    let (_, events) =
        loon_events::Journal::open_default(&store, &paths.sessions_dir(), agent.session_id())
            .unwrap();
    let rejected = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ToolCallRejected { .. }))
        .count();
    assert_eq!(
        rejected,
        2,
        "both invalid calls rejected: {:?}",
        events.iter().map(|e| e.summary()).collect::<Vec<_>>()
    );
}

/// Regression: opening a session and leaving without sending anything must not leave an
/// empty session behind in the store (that is what `/resume` and `loom sessions list`
/// enumerate). The session is materialized by the first real message instead.
#[tokio::test]
async fn opening_a_session_without_a_message_persists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::new().into(),
    ));
    let agent = Agent::new_session(
        &paths,
        agent_cfg("mock"),
        registry,
        ToolRegistry::standard(),
        ws.clone(),
        false,
    )
    .unwrap();
    let id = agent.session_id();

    let store = loon_core::open_store(&paths).unwrap();
    assert!(
        store.list_sessions().unwrap().is_empty(),
        "no session may be listed before the first message"
    );
    assert!(
        store.session_meta(id).unwrap().is_none(),
        "the session row must not exist yet"
    );
    assert!(
        agent.journal.is_pending(),
        "the journal must still be pending (unwritten)"
    );
    assert!(
        agent.journal.events().unwrap().is_empty(),
        "a lazy session has no events"
    );
    drop(agent);

    // A fresh store handle still sees nothing (nothing was written at any point).
    let store2 = loon_core::open_store(&paths).unwrap();
    assert!(store2.list_sessions().unwrap().is_empty());
    assert!(store2.events(id).unwrap().is_empty());
}

/// The first user message is what creates the session, and the journal still begins with
/// exactly one SessionStarted header so replays keep working.
#[tokio::test]
async fn first_message_materializes_the_session_with_a_started_header() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::text("hello")]),
    ));
    let mut agent = Agent::new_session(
        &paths,
        agent_cfg("mock"),
        registry,
        ToolRegistry::standard(),
        ws,
        false,
    )
    .unwrap();
    let id = agent.session_id();
    assert!(agent.journal.is_pending());

    let result = agent.run_turn("first real message").await;
    assert!(matches!(result.status, loon_core::TurnStatus::Completed));
    assert!(
        !agent.journal.is_pending(),
        "the first message must materialize the session"
    );

    let store = loon_core::open_store(&paths).unwrap();
    let listed = store.list_sessions().unwrap();
    assert_eq!(listed.len(), 1, "exactly one session after one message");
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].title, "first real message");

    let events = store.events(id).unwrap();
    let started = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::SessionStarted { .. }))
        .count();
    assert_eq!(started, 1, "exactly one SessionStarted header");
    assert!(
        matches!(events[0].kind, EventKind::SessionStarted { .. }),
        "the header must be the first event, got {:?}",
        events[0].kind
    );
    assert!(
        events.iter().any(
            |e| matches!(&e.kind, EventKind::UserMessage { text } if text == "first real message")
        ),
        "the message must be journaled"
    );
}
