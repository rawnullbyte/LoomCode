//! Exercise actual driver/agent concurrency and inspect what the provider receives.
use loon_core::{Agent, AgentConfig, AgentEvent, TurnStatus};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
use loon_tui::{UiCommand, UiNotice};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex};

struct Harness {
    _dir: tempfile::TempDir,
    agent: Arc<Mutex<Agent>>,
    provider: Arc<MockProvider>,
    cmd: mpsc::Sender<UiCommand>,
    events: mpsc::Receiver<AgentEvent>,
    notices: mpsc::Receiver<UiNotice>,
    results: mpsc::Receiver<String>,
    driver: tokio::task::JoinHandle<()>,
}
impl Harness {
    fn new(scripts: Vec<MockScript>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let paths = loon_config::LoonPaths {
            config_dir: p.join("c"),
            data_dir: p.join("d"),
            state_dir: p.join("s"),
            cache_dir: p.join("cache"),
            runtime_dir: p.join("r"),
        };
        paths.ensure().unwrap();
        let provider = MockProvider::with_scripts(scripts);
        let mut a = Agent::new_session(
            &paths,
            AgentConfig::from(&Default::default()),
            Arc::new(ProviderRegistry::with_mock(provider.clone())),
            loon_core::standard_tools(&paths, p),
            p.to_path_buf(),
            true,
        )
        .unwrap();
        let (tx, events) = mpsc::channel(256);
        a.set_event_sink(tx.clone());
        let agent = Arc::new(Mutex::new(a));
        let (cmd, rx) = mpsc::channel(32);
        let (out, results) = mpsc::channel(64);
        let (notice, notices) = mpsc::channel(64);
        let driver = tokio::spawn(loon_tui::driver::run(
            agent.clone(),
            rx,
            tx,
            notice,
            out,
            Arc::new(loon_core::loops::LoopRegistry::new()),
        ));
        Self {
            _dir: dir,
            agent,
            provider,
            cmd,
            events,
            notices,
            results,
            driver,
        }
    }
    async fn event(&mut self, predicate: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let e = self.events.recv().await.unwrap();
                if predicate(&e) {
                    return e;
                }
            }
        })
        .await
        .expect("expected driver event")
    }
    async fn finish(&mut self) {
        assert!(matches!(
            self.event(|e| matches!(e, AgentEvent::TurnFinished { .. }))
                .await,
            AgentEvent::TurnFinished {
                status: TurnStatus::Completed
            }
        ));
    }
    async fn close(self) {
        drop(self.cmd);
        tokio::time::timeout(Duration::from_secs(5), self.driver)
            .await
            .unwrap()
            .unwrap();
    }
}
fn slow(steps: Vec<MockStep>) -> MockScript {
    let mut all = vec![MockStep::Delay(Duration::from_millis(150))];
    all.extend(steps);
    MockScript::new(all)
}

#[tokio::test]
async fn followups_enter_the_next_tool_request_once_in_fifo_order() {
    let mut h = Harness::new(vec![
        slow(vec![MockStep::tool_call(
            "read",
            "list_directory",
            serde_json::json!({"path":"."}),
        )]),
        MockScript::text("done"),
    ]);
    h.cmd
        .send(UiCommand::Submit("inspect".into()))
        .await
        .unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    for s in ["also inspect tests", "preserve public APIs"] {
        h.cmd.send(UiCommand::Submit(s.into())).await.unwrap();
    }
    h.finish().await;
    let reqs = h.provider.requests();
    assert_eq!(
        reqs.len(),
        2,
        "followups belong to the next request of the SAME turn"
    );
    let messages: Vec<_> = reqs[1]
        .messages
        .iter()
        .filter(|m| m.role == loon_providers::Role::User)
        .map(|m| m.text.as_str())
        .collect();
    assert!(
        messages.ends_with(&["also inspect tests", "preserve public APIs"]),
        "{messages:?}"
    );
    assert!(!reqs[0]
        .messages
        .iter()
        .any(|m| m.text == "also inspect tests"));
    assert_eq!(
        messages
            .iter()
            .filter(|s| **s == "preserve public APIs")
            .count(),
        1
    );
    assert!(h.agent.lock().await.inbox.is_empty());
    h.close().await;
}
#[tokio::test]
async fn followup_during_final_text_triggers_next_request_without_stop_review() {
    let mut h = Harness::new(vec![
        slow(vec![MockStep::Text("first answer".into())]),
        MockScript::text("revised answer"),
    ]);
    h.cmd
        .send(UiCommand::Submit("original".into()))
        .await
        .unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    h.cmd
        .send(UiCommand::Submit("actually use Rust".into()))
        .await
        .unwrap();
    h.finish().await;
    assert_eq!(h.provider.requests().len(), 2);
    assert!(h.provider.requests()[1]
        .messages
        .iter()
        .any(|m| m.text == "actually use Rust"));
    h.close().await;
}
#[tokio::test]
async fn stop_preserves_edits_repairs_tool_history_and_pauses_pending_messages() {
    let mut h = Harness::new(vec![
        MockScript::new(vec![
            MockStep::tool_call(
                "write",
                "write",
                serde_json::json!({"path":"kept.txt","content":"completed work"}),
            ),
            MockStep::tool_call("wait", "bash", serde_json::json!({"command":"sleep 20"})),
        ]),
        MockScript::text("continued"),
    ]);
    h.cmd
        .send(UiCommand::Submit("implement".into()))
        .await
        .unwrap();
    h.event(|e| matches!(e,AgentEvent::ToolStarted{name,..} if name=="bash"))
        .await;
    h.cmd
        .send(UiCommand::Submit("then review".into()))
        .await
        .unwrap();
    h.cmd.send(UiCommand::Cancel).await.unwrap();
    assert!(matches!(
        h.event(|e| matches!(e, AgentEvent::TurnFinished { .. }))
            .await,
        AgentEvent::TurnFinished {
            status: TurnStatus::Cancelled
        }
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.provider.requests().len(),
        1,
        "stop must not launch queued work"
    );
    assert_eq!(
        std::fs::read_to_string(h._dir.path().join("kept.txt")).unwrap(),
        "completed work"
    );
    {
        let a = h.agent.lock().await;
        assert_eq!(a.inbox.snapshot(), vec!["then review"]);
        assert!(a.history.iter().any(|m| m.text == "implement"));
        let replay = loon_core::replay::replay_history(&a.journal.events().unwrap(), None);
        assert_eq!(
            serde_json::to_value(&a.history).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
        for m in &a.history {
            for c in &m.tool_calls {
                assert!(
                    a.history
                        .iter()
                        .any(|r| r.tool_call_id.as_deref() == Some(&c.id)),
                    "unanswered {}",
                    c.id
                );
            }
        }
    }
    h.cmd
        .send(UiCommand::Submit("continue".into()))
        .await
        .unwrap();
    h.finish().await;
    assert!(h.provider.requests()[1]
        .messages
        .iter()
        .any(|m| m.text.contains("then review") && m.text.contains("continue")));
    h.close().await;
}
#[tokio::test]
async fn effort_changes_apply_during_a_turn_without_waiting_for_agent_lock() {
    let mut h = Harness::new(vec![
        slow(vec![MockStep::tool_call(
            "list",
            "list_directory",
            serde_json::json!({"path":"."}),
        )]),
        MockScript::text("done"),
    ]);
    h.cmd
        .send(UiCommand::Submit("inspect".into()))
        .await
        .unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    h.cmd
        .send(UiCommand::Slash(
            loon_core::commands::SlashCommand::Effort {
                level: "high".into(),
            },
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), h.results.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(result.contains("next request"), "{result}");
    h.finish().await;
    let reqs = h.provider.requests();
    assert!(reqs[1].max_output_tokens > reqs[0].max_output_tokens);
    h.close().await;
}
#[tokio::test]
async fn new_during_a_request_clears_pending_input_and_switches_cancel_handles() {
    let mut h = Harness::new(vec![
        slow(vec![MockStep::Text("old".into())]),
        MockScript::text("new"),
    ]);
    let old = h.agent.lock().await.session_id();
    h.cmd
        .send(UiCommand::Submit("old task".into()))
        .await
        .unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    h.cmd
        .send(UiCommand::Submit("old queued".into()))
        .await
        .unwrap();
    h.cmd
        .send(UiCommand::Slash(loon_core::commands::SlashCommand::New))
        .await
        .unwrap();
    h.event(|e| matches!(e, AgentEvent::SessionLoaded { .. }))
        .await;
    assert_ne!(h.agent.lock().await.session_id(), old);
    h.cmd
        .send(UiCommand::Submit("new task".into()))
        .await
        .unwrap();
    h.finish().await;
    let last = h.provider.requests().pop().unwrap();
    assert!(!last
        .messages
        .iter()
        .any(|m| m.text.contains("old queued") || m.text == "old task"));
    h.close().await;
}
#[tokio::test]
async fn editing_a_pending_prompt_only_returns_text_that_was_actually_removed() {
    let mut h = Harness::new(vec![slow(vec![MockStep::Text("done".into())])]);
    h.cmd.send(UiCommand::Submit("task".into())).await.unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    h.cmd.send(UiCommand::Submit("draft".into())).await.unwrap();
    h.cmd.send(UiCommand::TakeLastQueued).await.unwrap();
    let draft = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(UiNotice::Draft(t)) = h.notices.recv().await {
                return t;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(draft, "draft");
    h.finish().await;
    assert_eq!(
        h.provider.requests().len(),
        1,
        "edited text must not be sent"
    );
    h.close().await;
}

/// Pulling a queued prompt back for editing must be a pure queue operation: the
/// running turn keeps going and the conversation is never rewound. The edited text
/// then reaches the provider as a normal follow-up.
#[tokio::test]
async fn pulling_back_a_queued_prompt_does_not_stop_or_rewind_the_running_turn() {
    let mut h = Harness::new(vec![
        slow(vec![MockStep::tool_call(
            "read",
            "list_directory",
            serde_json::json!({"path":"."}),
        )]),
        MockScript::text("done"),
    ]);
    h.cmd.send(UiCommand::Submit("task".into())).await.unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    h.cmd.send(UiCommand::Submit("draft".into())).await.unwrap();

    // Wait until the queue mirror shows the pending prompt.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(UiNotice::Queue(items)) = h.notices.recv().await {
                if items == vec!["draft".to_string()] {
                    return;
                }
            }
        }
    })
    .await
    .unwrap();

    h.cmd.send(UiCommand::TakeLastQueued).await.unwrap();
    // The running turn must not be cancelled by the edit. Skip anything stale
    // (duplicate mirrors, the draft notice itself); the withdrawn prompt shows up
    // as an empty queue mirror.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match h.notices.recv().await {
                Some(UiNotice::QueuePaused(true)) => {
                    panic!("editing a queued prompt paused the queue");
                }
                Some(UiNotice::Queue(items)) if items.is_empty() => return,
                Some(_) => continue,
                None => panic!("notice channel closed"),
            }
        }
    })
    .await
    .unwrap();

    // Resending the edited text is an ordinary submit and reaches the provider.
    h.cmd
        .send(UiCommand::Submit("draft edited".into()))
        .await
        .unwrap();
    h.finish().await;
    let reqs = h.provider.requests();
    let all: String = reqs
        .iter()
        .flat_map(|r| r.messages.iter().map(|m| m.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("draft edited"),
        "the edited prompt must be sent: {all}"
    );
    assert!(
        !all.contains("\ndraft\n"),
        "the withdrawn prompt must not send: {all}"
    );
    h.close().await;
}

#[tokio::test]
async fn monitor_status_is_available_while_a_provider_request_is_running() {
    let mut h = Harness::new(vec![slow(vec![MockStep::Text("done".into())])]);
    h.cmd.send(UiCommand::Submit("task".into())).await.unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    h.cmd
        .send(UiCommand::Slash(
            loon_core::commands::SlashCommand::Monitors { args: vec![] },
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), h.results.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(result.contains("no monitors"), "{result}");
    h.finish().await;
    h.close().await;
}

#[tokio::test]
async fn shell_and_monitor_commands_use_real_tools_and_finish_the_ui_operation() {
    let mut h = Harness::new(vec![]);
    for command in [
        "/shell start main",
        "/shell run main export LOON_TEST=kept",
        "/shell run main echo $LOON_TEST",
        "/shell close main",
    ] {
        h.cmd
            .send(UiCommand::Slash(
                loon_core::commands::parse(command).unwrap().unwrap(),
            ))
            .await
            .unwrap();
        h.finish().await;
        let result = tokio::time::timeout(Duration::from_secs(2), h.results.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!result.starts_with("Error:"), "{command}: {result}");
        if command.contains("echo") {
            assert!(result.contains("kept"), "{result}");
        }
    }
    h.close().await;
}

#[tokio::test]
async fn live_compaction_toggles_and_menu_settings_preserve_context_capacity() {
    let mut h = Harness::new(vec![
        slow(vec![MockStep::tool_call(
            "list",
            "list_directory",
            serde_json::json!({"path":"."}),
        )]),
        MockScript::text("done"),
    ]);
    h.cmd
        .send(UiCommand::Submit("inspect".into()))
        .await
        .unwrap();
    h.event(|e| matches!(e, AgentEvent::RequestStarted)).await;
    for cmd in [
        "/context 1m",
        "/autocompact off",
        "/autocompact on",
        "/config menu-set effort high",
    ] {
        h.cmd
            .send(UiCommand::Slash(
                loon_core::commands::parse(cmd).unwrap().unwrap(),
            ))
            .await
            .unwrap();
    }
    for _ in 0..4 {
        let out = tokio::time::timeout(Duration::from_secs(2), h.results.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(out.contains("next request"), "{out}");
    }
    h.finish().await;
    let a = h.agent.lock().await;
    assert_eq!(a.cfg.context_window, 1_000_000);
    assert!(a.autocompact);
    assert_eq!(a.effort.as_deref(), Some("high"));
    drop(a);
    h.close().await;
}
