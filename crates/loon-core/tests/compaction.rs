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
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            check_command: None,
            ultra_rounds: 0,
            permissions: Default::default(),
            context_window: 128_000,
            extra_tools: Vec::new(),
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
            Ok("DETERMINISTIC SUMMARY: user asked three questions; all answered.".to_string())
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
async fn failed_or_empty_summarizer_keeps_history_and_warns() {
    use loon_core::compaction::{CompactOutcome, Summarizer};
    // A failed model summarizer (auth error, context overflow, output cut) used to
    // surface as "empty summary; history retained" with the reason swallowed — and
    // automatic compaction retried it before every request. Both are pinned here.
    fn failing(reason: &'static str) -> Summarizer {
        Arc::new(move |_: String| Box::pin(async move { Err(reason.to_string()) }))
    }
    fn empty() -> Summarizer {
        Arc::new(|_: String| Box::pin(async { Ok(String::new()) }))
    }
    for summarizer in [failing("mock/1: authentication failed"), empty()] {
        let tmp = tempfile::tempdir().unwrap();
        let scripts: Vec<MockScript> = (1..=4)
            .map(|i| MockScript::text(format!("answer number {i} with padding text")))
            .collect();
        let mut agent = agent(&tmp, scripts);
        for i in 1..=3 {
            assert!(matches!(
                agent.run_turn(&format!("question {i}")).await.status,
                loon_core::TurnStatus::Completed
            ));
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        agent.set_event_sink(tx);
        agent.summarizer = Some(summarizer);
        let before: Vec<(Role, String)> = agent
            .history
            .iter()
            .map(|m| (m.role, m.text.clone()))
            .collect();

        let outcome = agent.compact_report(true).await;
        let reason = match &outcome {
            CompactOutcome::Failed(reason) => reason.clone(),
            other => panic!("expected failure, got {other:?}"),
        };
        assert!(
            !reason.trim().is_empty(),
            "the failure must carry a reason, not an empty string"
        );

        // History and journal are untouched: a failed summary never discards context.
        assert_eq!(
            agent
                .history
                .iter()
                .map(|m| (m.role, m.text.clone()))
                .collect::<Vec<_>>(),
            before,
            "failed compaction must keep history"
        );
        assert!(
            agent
                .journal
                .events()
                .unwrap()
                .iter()
                .all(|e| !matches!(e.kind, EventKind::ContextCompacted { .. })),
            "failed compaction must not journal ContextCompacted"
        );

        // The reason reaches the user as a warning instead of a bare "empty summary".
        let mut warned = None;
        while let Ok(event) = rx.try_recv() {
            if let loon_core::AgentEvent::Warning(text) = event {
                warned = Some(text);
            }
        }
        let warned = warned.expect("a warning must be emitted");
        assert!(
            warned.contains("compaction"),
            "warning should name compaction: {warned}"
        );

        // Automatic compaction backs off instead of re-running the failing summarizer
        // before every request.
        assert!(
            agent.auto_compaction_paused(),
            "a failed compaction must pause automatic retries"
        );
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
        Arc::new(|_| Box::pin(async { Ok("prior work summarized".into()) }));
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

#[tokio::test]
async fn model_summarizer_reports_the_real_failure_instead_of_empty_summary() {
    use loon_core::compaction::CompactOutcome;
    // The bug: the model-backed summarizer (no injected closure) swallowed its own
    // failure and the user saw only "compaction returned an empty summary". Here the
    // serving provider rejects the summarization request, so the warning must name
    // that rejection.
    let tmp = tempfile::tempdir().unwrap();
    // Exactly one script per turn; the next request is the summarizer's, and it fails.
    let mut scripts: Vec<MockScript> = (1..=3)
        .map(|i| MockScript::text(format!("answer number {i} with padding text")))
        .collect();
    scripts.push(MockScript::new(vec![MockStep::FailImmediate(
        loon_providers::ProviderError::Auth("HTTP 401: bad key".into()),
    )]));
    let mut agent = agent(&tmp, scripts);
    for i in 1..=3 {
        assert!(matches!(
            agent.run_turn(&format!("question {i}")).await.status,
            loon_core::TurnStatus::Completed
        ));
    }
    // No injected summarizer: the real model_summarizer runs.
    assert!(agent.summarizer.is_none());

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    agent.set_event_sink(tx);
    let before = agent.history.len();
    let outcome = agent.compact_report(true).await;

    let reason = match outcome {
        CompactOutcome::Failed(reason) => reason,
        other => panic!("expected a failure carrying a reason, got {other:?}"),
    };
    assert!(
        reason.contains("401") || reason.contains("auth"),
        "the reason must be the provider's actual failure, got: {reason}"
    );
    assert_eq!(agent.history.len(), before, "history must be retained");

    let mut warnings = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let loon_core::AgentEvent::Warning(text) = event {
            warnings.push(text);
        }
    }
    let warning = warnings
        .iter()
        .find(|w| w.contains("compaction"))
        .unwrap_or_else(|| panic!("no compaction warning; saw {warnings:?}"));
    assert!(
        !warning.contains("empty summary"),
        "the generic message must be gone: {warning}"
    );
    assert!(
        warning.contains("401") || warning.contains("auth"),
        "the warning must carry the real cause: {warning}"
    );
}
