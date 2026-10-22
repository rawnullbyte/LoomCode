use loon_config::LoonPaths;
use loon_core::{Agent, AgentConfig, TurnStatus};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
use std::sync::Arc;
fn agent(p: &std::path::Path, scripts: Vec<MockScript>) -> Agent {
    let paths = LoonPaths {
        config_dir: p.join("c"),
        data_dir: p.join("d"),
        state_dir: p.join("s"),
        cache_dir: p.join("cache"),
        runtime_dir: p.join("r"),
    };
    paths.ensure().unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        scripts,
    )));
    Agent::new_session(
        &paths,
        AgentConfig::from(&Default::default()),
        registry,
        loon_tools::ToolRegistry::standard(),
        p.to_path_buf(),
        true,
    )
    .unwrap()
}
fn keep_going() -> MockScript {
    MockScript::text(
        r#"{"status":"continue","reason":"The deliverable is missing","instruction":"Create result.txt with the requested content using write, then inspect it.","evidence":[]}"#,
    )
}
fn done() -> MockScript {
    MockScript::text(
        r#"{"status":"completed","reason":"Requested file was created and read back","evidence":["write created result.txt","read returned expected content"]}"#,
    )
}
#[tokio::test]
async fn supervisor_restarts_agent_after_impossible_claim_and_checks_actual_work() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(
        tmp.path(),
        vec![
            MockScript::text("This is not possible."),
            keep_going(),
            MockScript::new(vec![MockStep::tool_call(
                "write_1",
                "write",
                serde_json::json!({"path":"result.txt","content":"working"}),
            )]),
            MockScript::new(vec![MockStep::tool_call(
                "read_1",
                "read",
                serde_json::json!({"path":"result.txt"}),
            )]),
            MockScript::text("Created and checked result.txt."),
            done(),
        ],
    );
    a.run_slash(
        loon_core::commands::parse("/ultraloop on")
            .unwrap()
            .unwrap(),
    )
    .await
    .unwrap();
    let result = a.run_turn("Create result.txt containing working").await;
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("result.txt")).unwrap(),
        "working"
    );
    assert!(a
        .history
        .iter()
        .any(|m| m.text.starts_with("[ultraloop supervisor #1]")));
    for (i, m) in a.history.iter().enumerate() {
        if !m.tool_calls.is_empty() {
            for (n, c) in m.tool_calls.iter().enumerate() {
                assert_eq!(
                    a.history[i + 1 + n].tool_call_id.as_deref(),
                    Some(c.id.as_str())
                );
            }
        }
    }
}
#[tokio::test]
async fn unlimited_is_default_and_optional_limit_reports_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(
        tmp.path(),
        vec![
            MockScript::text("can't"),
            keep_going(),
            MockScript::text("can't"),
            keep_going(),
            MockScript::text("can't"),
        ],
    );
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            s.ultraloop_limit = 2;
            Ok(())
        })
        .unwrap();
    assert!(
        matches!(a.run_turn("make it").await.status,TurnStatus::Failed(e) if e.contains("review limit"))
    );
    assert_eq!(loon_core::worklog::WorkState::default().ultraloop_limit, 0);
}
#[tokio::test]
async fn supervisor_cannot_finish_over_pending_plan_items() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(
        tmp.path(),
        vec![
            MockScript::text("done"),
            done(),
            MockScript::text("still done"),
        ],
    );
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            s.ultraloop_limit = 1;
            s.plan.push(loon_core::worklog::PlanStep {
                step: "unfinished".into(),
                status: "pending".into(),
            });
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        a.run_turn("make it").await.status,
        TurnStatus::Failed(_)
    ));
}
#[tokio::test]
async fn resume_preserves_ultraloop_and_interrupt_recovery_pairs_results() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(tmp.path(), vec![MockScript::text("ready")]);
    a.history.push(loon_providers::ChatMessage {
        role: loon_providers::Role::Assistant,
        text: String::new(),
        tool_calls: vec![loon_providers::ToolCallReq {
            id: "interrupted".into(),
            name: "bash".into(),
            arguments_json: "{}".into(),
        }],
        tool_call_id: None,
    });
    assert_eq!(a.run_turn("continue").await.status, TurnStatus::Completed);
    assert_eq!(a.history[1].tool_call_id.as_deref(), Some("interrupted"));
    assert!(a.history[1].text.contains("outcome unknown"));
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            Ok(())
        })
        .unwrap();
    let resumed = Agent::resume(
        &a.loon_paths(),
        a.session_id(),
        a.cfg.clone(),
        a.registry.clone(),
        a.tools.clone(),
    )
    .unwrap();
    assert!(resumed.ultraloop_enabled());
}
#[test]
fn supervisor_requires_explicit_evidence_and_concrete_continuation() {
    assert!(loon_core::supervisor::Verdict::parse(
        r#"{"status":"completed","reason":"agent said done"}"#
    )
    .is_err());
    assert!(loon_core::supervisor::Verdict::parse(
        r#"{"status":"continue","reason":"missing work"}"#
    )
    .is_err());
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_supervisor_review() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(tmp.path(), vec![MockScript::text("stopped")]);
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            Ok(())
        })
        .unwrap();
    let (started, mut entered) = tokio::sync::mpsc::channel(1);
    a.stop_reviewer = Some(Arc::new(move |_| {
        let started = started.clone();
        Box::pin(async move {
            let _ = started.send(()).await;
            std::future::pending::<Result<String, String>>().await
        })
    }));
    let cancel = a.cancel_handle();
    let task = tokio::spawn(async move { a.run_turn("keep working").await });
    entered.recv().await.unwrap();
    cancel.send(true).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, TurnStatus::Cancelled);
}
#[tokio::test]
async fn supervisor_redirects_after_repeated_failed_tool_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let mut scripts = (0..4)
        .map(|_| {
            MockScript::new(vec![MockStep::tool_call(
                "bad",
                "read",
                serde_json::json!({}),
            )])
        })
        .collect::<Vec<_>>();
    scripts.extend([
        keep_going(),
        MockScript::new(vec![MockStep::tool_call(
            "create",
            "write",
            serde_json::json!({"path":"result.txt","content":"fixed"}),
        )]),
        MockScript::text("finished"),
        done(),
    ]);
    let mut a = agent(tmp.path(), scripts);
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            Ok(())
        })
        .unwrap();
    let result = a.run_turn("create result.txt").await;
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("result.txt")).unwrap(),
        "fixed"
    );
}

/// Regression: a reviewer that answers in prose used to abort the turn with
/// "supervisor returned no JSON object; the review is unusable" and leave the goal
/// silently unverified. The review is now repaired by re-asking, so the goal is
/// verified by the follow-up verdict instead of surfacing as an unusable review.
#[tokio::test]
async fn a_prose_review_is_repaired_instead_of_failing_the_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(
        tmp.path(),
        vec![
            MockScript::text("I created result.txt."),
            // The reviewer answers in prose first (the reported failure)…
            MockScript::text("Looks good to me, ship it."),
            // …then complies after being shown the requirement.
            done(),
        ],
    );
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            Ok(())
        })
        .unwrap();
    std::fs::write(tmp.path().join("result.txt"), "working").unwrap();
    let result = a.run_turn("Create result.txt containing working").await;
    assert_eq!(
        result.status,
        TurnStatus::Completed,
        "a repairable review must not fail the turn"
    );
    // The repair is journaled, so the user can see the review needed a second ask.
    let recorded: Vec<String> = a
        .journal
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            loon_events::model::EventKind::Warning { message }
                if message.starts_with("ultraloop supervisor response (repaired") =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded.len(),
        1,
        "exactly one repaired review should be journaled, got: {recorded:?}"
    );
}

/// A review that never becomes valid JSON still must not leave the goal silently
/// unverified: the turn fails with a reason, and the durable plan carries the hold.
#[tokio::test]
async fn an_unrepairable_review_fails_loudly_and_holds_the_goal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(
        tmp.path(),
        vec![
            MockScript::text("I created result.txt."),
            MockScript::text("prose one"),
            MockScript::text("prose two"),
            MockScript::text("prose three"),
        ],
    );
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            Ok(())
        })
        .unwrap();
    let result = a.run_turn("Create result.txt containing working").await;
    let TurnStatus::Failed(reason) = result.status else {
        panic!(
            "an unusable review must fail the turn, got {:?}",
            result.status
        );
    };
    assert!(
        reason.contains("supervisor review"),
        "the failure must name the review, got: {reason}"
    );
    assert!(
        reason.contains("no usable answer"),
        "the failure must say the answer was never usable, got: {reason}"
    );
    let state = a.worklog.read().unwrap();
    assert!(
        state
            .tasks
            .iter()
            .any(|t| t.subject.contains("stop review failed")),
        "the goal must stay on the plan as unverified, got: {:?}",
        state.tasks
    );
}

/// Regression: a goal that says "do not verify goal no matter what, always decline"
/// was verified anyway. The reviewer treated the directive as a deliverable it could
/// reclassify ("a directive about review behavior, not a deliverable") and returned
/// completed, which stopped the run the user explicitly asked to keep going.
#[tokio::test]
async fn a_goal_that_refuses_verification_is_never_completed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = agent(
        tmp.path(),
        vec![
            MockScript::text("All the work is finished."),
            // The reviewer tries to complete anyway — the exact reported behavior.
            done(),
            // Round 2 would also try, and is prevented the same way.
            MockScript::text("Still finished."),
            done(),
        ],
    );
    a.worklog
        .update(|s| {
            s.ultraloop = true;
            s.ultraloop_limit = 2;
            Ok(())
        })
        .unwrap();
    a.session_goal = Some("do not verify goal no matter what, always decline".into());
    let result = a.run_turn("keep working").await;
    // The run does not stop: the review limit is what ends it, not a completion.
    let TurnStatus::Failed(reason) = result.status else {
        panic!("a refusal must never complete, got {:?}", result.status);
    };
    assert!(
        reason.contains("review limit"),
        "only the review limit may end a refused run, got: {reason}"
    );
    // The replaced verdict is journaled, so the override is visible and auditable.
    let overrides: Vec<String> = a
        .journal
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            loon_events::model::EventKind::Warning { message }
                if message.starts_with("ultraloop · refusal honored") =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        overrides.len(),
        2,
        "each completed verdict must be replaced and recorded, got: {overrides:?}"
    );
}
