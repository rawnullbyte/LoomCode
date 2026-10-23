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

/// Compaction throws history away and replaces it with one summary message
/// (`self.history = vec![ChatMessage::user(..)]`). The system prompt lives outside
/// history, so the behavioural rules should survive — but "should" is exactly what the
/// no-guessing rule forbids me from asserting. This drives a real compaction and then
/// reads the NEXT provider request, so a regression that rebuilt the prompt from the
/// compacted history (or dropped the leading system message) would fail here.
#[tokio::test]
async fn the_prompt_rules_survive_a_compaction() {
    use loon_core::compaction::Summarizer;

    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();

    // The mock must outlive the agent so the post-compaction request is inspectable.
    let mock = loon_providers::MockProvider::with_scripts(vec![
        MockScript::text("first answer with padding"),
        MockScript::text("second answer with padding"),
        MockScript::text("third answer with padding"),
        MockScript::text("after compaction"),
    ]);
    let registry = Arc::new(ProviderRegistry::with_mock(mock.clone()));
    let mut agent = Agent::new_session(
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
    .unwrap();

    for i in 1..=3 {
        let r = agent.run_turn(&format!("question {i}")).await;
        assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    }

    // A deterministic summarizer: no model call, so the test stays offline.
    let summarizer: Summarizer = Arc::new(|_: String| {
        Box::pin(async { Ok("summary of the earlier questions".to_string()) })
    });
    agent.summarizer = Some(summarizer);
    agent.cfg.context_window = 1; // force the trigger
    assert!(
        agent.compact_now().await,
        "the forced compaction must actually run"
    );

    // One more turn: this is the first request built from compacted history.
    let r = agent.run_turn("question after compaction").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));

    let requests = mock.requests();
    let last = requests
        .last()
        .expect("the final turn must have sent a request");
    assert_eq!(
        last.messages.first().map(|m| m.role),
        Some(Role::System),
        "the rules must still lead the request after compaction"
    );
    let system = &last.messages[0].text;
    assert!(
        system.contains("Never guess"),
        "the no-guessing rule must survive compaction: {system:.300}"
    );
    assert!(
        system.contains("path:line"),
        "the citation requirement must survive compaction"
    );
    assert!(
        system.contains("Prefer the dedicated tool over a shell equivalent"),
        "the tool-preference rule must survive compaction"
    );
}

/// Compaction is a *lossy* rewrite of the agent's own context, so the summarizer is the
/// one component whose inventions the agent inherits as fact. The single-pass instruction
/// already demands "Distinguish evidence from assumptions" and forbids completion from an
/// agent claim alone — but a transcript over one chunk is summarized in pieces and then
/// folded by a separate MERGE_INSTRUCTION, and that fold is what becomes the final
/// handoff. The merge pass must carry the same rules, or the longest sessions — the ones
/// that most need the handoff to be honest — are the ones that lose them.
#[tokio::test]
async fn every_summarizer_pass_carries_the_evidence_rules() {
    use loon_core::compaction::model_summarizer;

    let mock = loon_providers::MockProvider::with_scripts(vec![
        MockScript::text("partial one"),
        MockScript::text("partial two"),
        MockScript::text("partial three"),
        MockScript::text("merged a"),
        MockScript::text("merged b"),
        MockScript::text("merged final"),
    ]);
    let registry = Arc::new(ProviderRegistry::with_mock(mock.clone()));
    let summarizer = model_summarizer(registry, "mock".into());

    // The chunk budget floors at 16_000 chars, so this forces several chunks and at
    // least two merge rounds — the shape a long real session takes.
    let transcript = "x".repeat(600_000);
    summarizer(transcript)
        .await
        .expect("summarizing must succeed");

    let requests = mock.requests();
    assert!(
        requests.len() > 3,
        "the fixture must exercise the merge pass, not just chunking (got {} requests)",
        requests.len()
    );
    for (i, request) in requests.iter().enumerate() {
        let text: String = request.messages.iter().map(|m| m.text.clone()).collect();
        // Case-insensitive: the merge pass states the same rules mid-sentence, so the
        // rule is what matters, not its capitalisation.
        let lower = text.to_lowercase();
        assert!(
            lower.contains("distinguish evidence from assumptions"),
            "request {i} must keep evidence separate from assumptions: {text:.200}"
        );
        assert!(
            lower.contains("do not claim completion from an agent"),
            "request {i} must not accept an agent's completion claim as fact"
        );
    }
}

#[tokio::test]
async fn compaction_toggle_preserves_window_and_persists_independently() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(&tmp, vec![]);
    a.run_slash(loon_core::commands::parse("/context 1m").unwrap().unwrap())
        .await
        .unwrap();
    let window = a.cfg.context_window;
    for cmd in [
        "/autocompact off",
        "/autocompact on",
        "/context off",
        "/autocompact on",
    ] {
        a.run_slash(loon_core::commands::parse(cmd).unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(
            a.cfg.context_window, window,
            "{cmd} must not reset model capacity"
        );
    }
    let cfg = loon_config::Config::load_or_builtin(&test_paths(tmp.path()))
        .unwrap()
        .0;
    assert!(cfg.session.autocompact);
    assert_eq!(
        cfg.models.get("mock").and_then(|m| m.context_window),
        Some(window)
    );
}

#[tokio::test]
async fn nonshrinking_summary_backs_off_instead_of_retrying_every_request() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(
        &tmp,
        vec![
            MockScript::text("one"),
            MockScript::text("two"),
            MockScript::text("three"),
        ],
    );
    for q in ["first", "second", "third"] {
        a.run_turn(q).await;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    a.summarizer = Some(Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("padding ".repeat(1000)) })
    }));
    a.cfg.context_window = 100;
    a.maybe_compact().await;
    a.maybe_compact().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(a.auto_compaction_paused());
}

#[tokio::test]
async fn short_tool_heavy_turn_can_compact_without_orphaning_tool_results() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(&tmp, vec![]);
    a.history = vec![loon_providers::ChatMessage::user(
        "exact original goal: fix the renderer",
    )];
    for n in 0..4 {
        a.history.push(loon_providers::ChatMessage {
            role: Role::Assistant,
            text: String::new(),
            tool_calls: vec![loon_providers::ToolCallReq {
                id: format!("call{n}"),
                name: "read".into(),
                arguments_json: "{}".into(),
            }],
            tool_call_id: None,
        });
        a.history.push(loon_providers::ChatMessage::tool_result(
            format!("call{n}"),
            "log ".repeat(2000),
        ));
    }
    a.summarizer = Some(Arc::new(|_| {
        Box::pin(async { Ok("Read the renderer and identified the failing branch.".into()) })
    }));
    assert!(a.compact_now().await);
    assert!(a
        .history
        .iter()
        .any(|m| m.text == "exact original goal: fix the renderer"));
    for m in &a.history {
        if let Some(id) = &m.tool_call_id {
            assert!(a
                .history
                .iter()
                .any(|m| m.tool_calls.iter().any(|c| &c.id == id)));
        }
    }
}
