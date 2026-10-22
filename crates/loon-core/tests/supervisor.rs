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
