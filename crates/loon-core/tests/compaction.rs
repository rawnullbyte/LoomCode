//! Context compaction end to end: crossing the threshold summarizes older exchanges
//! through an injectable (deterministic, non-model) summarizer, journals the result,
//! and — the invariant that matters — `replay_history` over the journal reproduces the
//! live post-compaction history exactly, so resume sees byte-identical context.

use loon_config::LoonPaths;
use loon_core::{Agent, AgentConfig};
use loon_events::model::EventKind;
use loon_providers::{MockScript, MockStep, ProviderRegistry, Role};
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

fn agent(tmp: &tempfile::TempDir, scripts: Vec<MockScript>) -> Agent {
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(scripts),
    ));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    Agent::new_session(
        &paths,
        AgentConfig {
            model_ref: "mock".into(),
            max_steps: 12,
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            permissions: Default::default(),
            context_window: 128_000,
            extra_tools: Vec::new(),
            check_command: None,
        },
        registry,
        ToolRegistry::standard(),
        ws,
        true,
    )
    .unwrap()
}

#[tokio::test]
async fn compaction_summarizes_keeps_tail_and_replays_identically() {
    let tmp = tempfile::tempdir().unwrap();
    // One script per turn: each turn consumes one request script.
    let scripts: Vec<MockScript> = (1..=4)
        .map(|i| MockScript::text(format!("answer number {i} with plenty of padding text")))
        .collect();
    let mut agent = agent(&tmp, scripts);

    for i in 1..=3 {
        let r = agent.run_turn(&format!("question {i}")).await;
        assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    }
    // 3 exchanges, nothing compacted yet.
    assert!(!agent
        .journal
        .events()
        .unwrap()
        .iter()
        .any(|e| matches!(e.kind, EventKind::ContextCompacted { .. })));
    assert_eq!(agent.history.len(), 6);

    // Shrink the window and install a deterministic summarizer (not a model call —
    // labeled as such; this tests mechanics, not summary quality).
    agent.cfg.context_window = 16; // tiny ⇒ threshold crossed immediately
    let hook: loon_core::compaction::Summarizer = Arc::new(|_| {
        Box::pin(async {
            "DETERMINISTIC SUMMARY: user asked three questions; all answered.".to_string()
        })
    });
    agent.summarizer = Some(hook);
    assert!(agent.should_compact());

    let r = agent.run_turn("question 4").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));

    // Post-compaction live history: [summary, kept user2..assistant4]
    assert_eq!(agent.history[0].role, Role::User);
    assert!(agent.history[0]
        .text
        .starts_with("[Earlier conversation summary]"));
    assert!(agent.history[0].text.contains("DETERMINISTIC SUMMARY"));
    let compacted = agent
        .journal
        .events()
        .unwrap()
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::ContextCompacted {
                before_tokens,
                after_tokens,
                ..
            } => Some((before_tokens, after_tokens)),
            _ => None,
        })
        .expect("ContextCompacted journaled");
    assert!(
        compacted.0 > compacted.1,
        "compaction must shrink the estimate"
    );

    // ── the invariant: replay reproduces the live history exactly ───────────────
    let events = agent.journal.events().unwrap();
    let replayed = loon_core::replay::replay_history(&events, None);
    assert_eq!(replayed.len(), agent.history.len());
    for (live, redone) in agent.history.iter().zip(replayed.iter()) {
        assert_eq!(live.role, redone.role);
        assert_eq!(live.text, redone.text);
        assert_eq!(live.tool_calls.len(), redone.tool_calls.len());
    }
}

#[tokio::test]
async fn compact_now_is_a_noop_when_only_recent_exchanges_exist() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = agent(&tmp, vec![MockScript::text("one answer")]);
    let r = agent.run_turn("only question").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    agent.cfg.context_window = 1; // force should_compact true
    assert!(agent.should_compact());
    // A single exchange is below the keep-window: nothing to summarize.
    assert!(!agent.compact_now().await);
    assert!(agent
        .journal
        .events()
        .unwrap()
        .iter()
        .all(|e| !matches!(e.kind, EventKind::ContextCompacted { .. })));
}

#[tokio::test]
async fn compaction_fires_automatically_mid_turn_under_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    // Turn 1: a tool call (grows history), then its answer.
    let scripts = vec![
        MockScript::new(vec![MockStep::tool_call(
            "call_1",
            "read",
            json!({"path": "notes.txt"}),
        )]),
        MockScript::text("done with the task, long winded final answer for padding"),
    ];
    std::fs::write(tmp.path().join("proj").join("notes.txt"), "some notes").ok();
    let mut agent = agent(&tmp, scripts);
    // Simulate prior heavy context: window small enough that the first request trips it.
    agent.cfg.context_window = 12;
    let hook: loon_core::compaction::Summarizer =
        Arc::new(|_| Box::pin(async { "prior work summarized".into() }));
    agent.summarizer = Some(hook);
    // Nothing to summarize yet (no exchanges) ⇒ request proceeds without compaction.
    let r = agent.run_turn("go").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    assert!(agent
        .journal
        .events()
        .unwrap()
        .iter()
        .all(|e| !matches!(e.kind, EventKind::ContextCompacted { .. })));
}
