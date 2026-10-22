use loon_core::{Agent, AgentConfig, AgentEvent};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
use loon_tui::UiCommand;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
#[tokio::test]
async fn prompt_immediately_after_finished_is_executed_without_another_keystroke() {
    let temp = tempfile::tempdir().unwrap();
    let p = temp.path();
    let paths = loon_config::LoonPaths {
        config_dir: p.join("c"),
        data_dir: p.join("d"),
        state_dir: p.join("s"),
        cache_dir: p.join("cache"),
        runtime_dir: p.join("r"),
    };
    paths.ensure().unwrap();
    let mut a = Agent::new_session(
        &paths,
        AgentConfig::from(&Default::default()),
        Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![MockScript::text("one"), MockScript::text("two")],
        ))),
        loon_core::standard_tools(&paths, p),
        p.to_path_buf(),
        true,
    )
    .unwrap();
    let (tx, mut events) = mpsc::channel(8);
    a.set_event_sink(tx.clone());
    let a = Arc::new(Mutex::new(a));
    let (cmd, rx) = mpsc::channel(8);
    let (out, mut results) = mpsc::channel(8);
    let (notices, _notice_rx) = mpsc::channel(8);
    let driver = tokio::spawn(loon_tui::driver::run(
        a,
        rx,
        tx,
        notices,
        out,
        Arc::new(loon_core::loops::LoopRegistry::new()),
    ));
    cmd.send(UiCommand::Submit("first".into())).await.unwrap();
    let mut finished = 0;
    let mut text = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while finished < 2 {
            match events.recv().await.unwrap() {
                AgentEvent::TextDelta(t) => text.push_str(&t),
                AgentEvent::TurnFinished { .. } => {
                    finished += 1;
                    if finished == 1 {
                        cmd.send(UiCommand::Submit("second".into())).await.unwrap();
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(text, "onetwo");
    while let Ok(result) = results.try_recv() {
        assert!(!result.contains("queued"), "{result}");
    }
    drop(cmd);
    driver.await.unwrap();
}
#[tokio::test]
async fn resumed_session_can_be_cancelled_with_the_current_cancel_channel() {
    let temp = tempfile::tempdir().unwrap();
    let p = temp.path();
    let paths = loon_config::LoonPaths {
        config_dir: p.join("c"),
        data_dir: p.join("d"),
        state_dir: p.join("s"),
        cache_dir: p.join("cache"),
        runtime_dir: p.join("r"),
    };
    paths.ensure().unwrap();
    let script = MockScript::new(vec![MockStep::tool_call("wait", "bash", serde_for_wait())]);
    let mut a = Agent::new_session(
        &paths,
        AgentConfig::from(&Default::default()),
        Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![script],
        ))),
        loon_core::standard_tools(&paths, p),
        p.to_path_buf(),
        true,
    )
    .unwrap();
    let id = a.session_id();
    let (tx, mut events) = mpsc::channel(16);
    a.set_event_sink(tx.clone());
    let a = Arc::new(Mutex::new(a));
    let (cmd, rx) = mpsc::channel(8);
    let (out, mut results) = mpsc::channel(8);
    let (notices, _notice_rx) = mpsc::channel(8);
    let driver = tokio::spawn(loon_tui::driver::run(
        a,
        rx,
        tx,
        notices,
        out,
        Arc::new(loon_core::loops::LoopRegistry::new()),
    ));
    cmd.send(UiCommand::Slash(
        loon_core::commands::SlashCommand::Resume {
            target: Some(id.to_string()),
        },
    ))
    .await
    .unwrap();
    assert!(results.recv().await.unwrap().contains("resumed"));
    cmd.send(UiCommand::Submit("wait".into())).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            match event {
                AgentEvent::ToolStarted { .. } => cmd.send(UiCommand::Cancel).await.unwrap(),
                AgentEvent::TurnFinished { status } => {
                    assert_eq!(status, loon_core::TurnStatus::Cancelled);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    drop(cmd);
    driver.await.unwrap();
}
fn serde_for_wait() -> serde_json::Value {
    serde_json::json!({"command":"sleep 30","timeout_secs":60})
}

/// Esc while the first request is still streaming: the prompt must come back to the
/// input bar (via UiNotice::RestorePrompt) instead of being lost.
#[tokio::test]
async fn cancelling_an_in_flight_prompt_hands_it_back_for_editing() {
    let temp = tempfile::tempdir().unwrap();
    let p = temp.path();
    let paths = loon_config::LoonPaths {
        config_dir: p.join("c"),
        data_dir: p.join("d"),
        state_dir: p.join("s"),
        cache_dir: p.join("cache"),
        runtime_dir: p.join("r"),
    };
    paths.ensure().unwrap();
    let mut a = Agent::new_session(
        &paths,
        AgentConfig::from(&Default::default()),
        // A slow shell call keeps the turn genuinely in flight so Esc has something
        // real to cancel (the mock would otherwise answer instantly).
        Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![MockScript::new(vec![MockStep::tool_call(
                "slow",
                "bash",
                serde_json::json!({"command": "sleep 5"}),
            )])],
        ))),
        loon_core::standard_tools(&paths, p),
        p.to_path_buf(),
        true,
    )
    .unwrap();
    let (tx, mut events) = mpsc::channel(8);
    a.set_event_sink(tx.clone());
    let a = Arc::new(Mutex::new(a));
    let (cmd, rx) = mpsc::channel(8);
    let (out, _results) = mpsc::channel(8);
    let (notices, mut notice_rx) = mpsc::channel(8);
    let driver = tokio::spawn(loon_tui::driver::run(
        a,
        rx,
        tx,
        notices,
        out,
        Arc::new(loon_core::loops::LoopRegistry::new()),
    ));

    let prompt = "refactor the parser and run the tests";
    cmd.send(UiCommand::Submit(prompt.into())).await.unwrap();
    // Wait until the turn is actually running, then cancel it.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                AgentEvent::TurnStarted | AgentEvent::RequestStarted => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    cmd.send(UiCommand::Cancel).await.unwrap();

    // The exact prompt text is handed back for the input bar.
    let restored = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match notice_rx.recv().await {
                Some(loon_tui::UiNotice::RestorePrompt(text)) => break Some(text),
                Some(_) => continue,
                None => break None,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(restored.as_deref(), Some(prompt));

    drop(cmd);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), driver).await;
}

/// The restored draft lands in the input bar with the cursor at the end, and the
/// orphaned transcript echo is dropped so resending does not duplicate it.
#[test]
fn restored_prompt_fills_the_input_bar_and_drops_the_orphan_echo() {
    use loon_tui::{Block, StatusLine};
    let mut app = loon_tui::App::new(
        true,
        StatusLine {
            model: "mock".into(),
            member: None,
            mode: "bypass".into(),
            context_est: 0,
            busy: true,
            cwd: "/w".into(),
        },
    );
    let prompt = "fix the flaky test\nand rerun it";
    // What a cancelled turn leaves behind: its user echo plus a partial answer and a
    // "cancelled" status line, with an earlier completed turn before it.
    app.push_block(Block::User("earlier turn".into()));
    app.push_block(Block::Assistant("earlier answer".into()));
    app.push_block(Block::User(prompt.into()));
    app.push_block(Block::Assistant("half an ans".into()));
    app.push_block(Block::System("cancelled".into()));
    app.apply_notice(loon_tui::UiNotice::RestorePrompt(prompt.into()));

    assert_eq!(app.input, prompt, "the prompt is back in the input bar");
    assert_eq!(app.cursor, prompt.chars().count());
    assert!(
        !app.blocks
            .iter()
            .any(|b| matches!(b, Block::User(t) if t == prompt)),
        "the cancelled prompt's transcript row is gone"
    );
    assert!(
        !app.blocks
            .iter()
            .any(|b| matches!(b, Block::System(s) if s == "cancelled")),
        "no cancelled marker is left behind"
    );
    assert!(
        !app.blocks
            .iter()
            .any(|b| matches!(b, Block::Assistant(a) if a == "half an ans")),
        "the partial answer is discarded"
    );
    assert!(
        app.blocks
            .iter()
            .any(|b| matches!(b, Block::User(t) if t == "earlier turn")),
        "earlier turns are preserved"
    );
}
