//! Regressions for the interactive harness changes; providers are deterministic.
use loon_config::{Config, LoonPaths};
use loon_core::{Agent, AgentConfig, AgentEvent, TurnStatus};
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
    let cfg = AgentConfig::from(&loon_config::SessionCfg::default());
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        scripts,
    )));
    let a = Agent::new_session(
        &paths,
        cfg,
        registry,
        loon_tools::ToolRegistry::standard(),
        cwd,
        true,
    )
    .unwrap();
    (paths, a)
}
async fn command(a: &mut Agent, text: &str) -> String {
    a.run_slash(loon_core::commands::parse(text).unwrap().unwrap())
        .await
        .unwrap()
}
#[tokio::test]
async fn rewind_restores_multiline_prompt_and_new_branch_survives_resume() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(
        d.path(),
        vec![
            MockScript::text("first answer"),
            MockScript::text("discarded answer"),
            MockScript::text("new answer"),
        ],
    );
    a.run_turn("first").await;
    a.run_turn("second\nexact original draft 🦀").await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    a.set_event_sink(tx);
    command(&mut a, "/rewind 1 conversation").await;
    let mut restored = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ConversationRewound { blocks, prompt } = event {
            assert_eq!(prompt, "second\nexact original draft 🦀");
            assert!(!blocks.iter().any(|(_, s)| s.contains("discarded")));
            assert_eq!(blocks.len(), 2);
            restored = true;
        }
    }
    assert!(restored);
    a.run_turn("edited second").await;
    let resumed = Agent::resume(
        &paths,
        a.session_id(),
        a.cfg.clone(),
        a.registry.clone(),
        loon_tools::ToolRegistry::standard(),
    )
    .unwrap();
    let text = resumed
        .history
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("first answer"));
    assert!(text.contains("edited second"));
    assert!(text.contains("new answer"));
    assert!(!text.contains("discarded"));
    assert!(!text.contains("original draft"));
}
#[tokio::test]
async fn rewind_picker_groups_tool_rounds_into_one_user_message() {
    let d = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(
        d.path(),
        vec![
            MockScript::new(vec![MockStep::tool_call("r", "tree", json!({"depth":1}))]),
            MockScript::text("done"),
            MockScript::text("next"),
        ],
    );
    a.run_turn("inspect the project").await;
    a.run_turn("next prompt").await;
    let events = a.journal.events().unwrap();
    assert_eq!(loon_core::rewind::request_boundaries(&events).len(), 3);
    let entries = loon_core::rewind::rewind_entries(&a.journal, &events);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].prompt, "next prompt");
    assert_eq!(entries[1].prompt, "inspect the project");
    assert_eq!(entries[1].n, 3);
}
#[tokio::test]
async fn numeric_auto_compact_persists_and_clamps_to_capacity() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(d.path(), vec![]);
    assert!(command(&mut a, "/autocompact 50k").await.contains("50000"));
    assert_eq!(a.compaction_trigger_tokens(), 50_000);
    assert_eq!(
        Config::load_or_builtin(&paths)
            .unwrap()
            .0
            .session
            .autocompact_tokens,
        Some(50_000)
    );
    assert!(a
        .run_slash(
            loon_core::commands::parse("/autocompact 3")
                .unwrap()
                .unwrap()
        )
        .await
        .is_err());
    a.cfg.context_window = 16_000;
    assert_eq!(a.compaction_trigger_tokens(), 12_800);
}

#[tokio::test]
async fn context_size_accepts_shorthand_and_percent_tracks_the_window() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(d.path(), vec![]);

    // One command sets the window from shorthand: `1m` is 1,000,000 tokens.
    let out = command(&mut a, "/context 1m").await;
    assert_eq!(a.cfg.context_window, 1_000_000, "{out}");
    assert!(out.contains("1M"), "the report uses compact units: {out}");
    assert!(!out.contains("1000k"), "never the old 1000k form: {out}");
    assert_eq!(
        Config::load_or_builtin(&paths)
            .unwrap()
            .0
            .models
            .get("mock")
            .and_then(|m| m.context_window),
        Some(1_000_000),
        "persisted against the active model definition"
    );

    // `200k` works the same way, and the trigger is 80% of it by default.
    command(&mut a, "/context 200k").await;
    assert_eq!(a.cfg.context_window, 200_000);
    assert_eq!(a.compaction_trigger_tokens(), 160_000);

    // A percentage tracks the window instead of pinning an absolute count.
    let out = command(&mut a, "/autocompact 90%").await;
    assert!(out.contains("90%"), "{out}");
    assert_eq!(a.autocompact_percent, Some(90));
    assert_eq!(a.compaction_trigger_tokens(), 180_000);
    command(&mut a, "/context 1m").await;
    assert_eq!(
        a.compaction_trigger_tokens(),
        900_000,
        "90% follows the new window"
    );

    // `auto` returns to the default fraction, and `off` disables compaction.
    command(&mut a, "/context auto").await;
    assert_eq!(a.autocompact_percent, None);
    assert_eq!(a.compaction_trigger_tokens(), 800_000);
    command(&mut a, "/context off").await;
    assert_eq!(a.cfg.context_window, 0);
    assert!(!a.should_compact());

    // Junk and out-of-range values are refused with usage, not silently accepted.
    for bad in [
        "/context 99x",
        "/context 20m",
        "/context 100%",
        "/context 12",
    ] {
        assert!(
            a.run_slash(loon_core::commands::parse(bad).unwrap().unwrap())
                .await
                .is_err(),
            "{bad} must be refused"
        );
    }
}

#[tokio::test]
async fn queued_settings_apply_to_the_next_request_while_busy() {
    let d = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(d.path(), vec![]);
    // The live queue is lock-free, so a settings command can land while a turn runs.
    let tx = a.live_command_sender();
    tx.send(loon_core::commands::SlashCommand::Effort {
        level: "ultra".into(),
    })
    .await
    .unwrap();
    tx.send(loon_core::commands::SlashCommand::Context {
        size: Some("1m".into()),
    })
    .await
    .unwrap();
    // Drained at the request boundary; the same path a running turn takes.
    a.apply_pending_live_commands();
    assert_eq!(a.effort.as_deref(), Some("ultra"));
    assert_eq!(a.cfg.context_window, 1_000_000);
}
#[tokio::test]
async fn pre_tool_hook_blocks_write_and_stop_hook_forces_continuation() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(
        d.path(),
        vec![
            MockScript::new(vec![MockStep::tool_call(
                "w",
                "write",
                json!({"path":"never.txt","content":"bad"}),
            )]),
            MockScript::text("handled"),
        ],
    );
    let mut cfg = Config::builtin();
    cfg.hooks.insert("PreToolUse".into(),vec![loon_config::schema::Hook{matcher:Some("write".into()),command:Some("printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"use permitted alternative\"}}'".into()),url:None,timeout_secs:2}]);
    cfg.save(&paths).unwrap();
    let result = a.run_turn("write a file").await;
    assert_eq!(result.status, TurnStatus::Completed);
    assert!(!a.ctx.ws.root().join("never.txt").exists());
    assert!(
        a.history
            .iter()
            .any(|m| m.text.contains("permitted alternative")),
        "{:#?}",
        a.history
    );
    let (stop_paths, mut a) = agent(
        &d.path().join("stop"),
        vec![MockScript::text("premature"), MockScript::text("verified")],
    );
    let mut cfg = Config::builtin();
    cfg.hooks.insert("Stop".into(),vec![loon_config::schema::Hook{matcher:None,command:Some("if test ! -f .hook-reviewed; then touch .hook-reviewed; printf '%s' '{\"decision\":\"block\",\"reason\":\"verify the goal\"}'; fi".into()),url:None,timeout_secs:2}]);
    cfg.save(&stop_paths).unwrap();
    let result = a.run_turn("finish").await;
    assert_eq!(result.steps, 2);
    assert!(result.assistant_text.contains("verified"));
}
#[tokio::test]
async fn command_host_and_ask_rules_enforce_precedence() {
    use loon_config::{PermissionRule, Permissions};
    use loon_core::permissions::{Decision, PermissionEngine};
    let rule = |tool: &str, command: Option<&str>, host: Option<&str>| PermissionRule {
        tool: tool.into(),
        path_prefix: None,
        command_prefix: command.map(str::to_string),
        host: host.map(str::to_string),
    };
    let cfg = Permissions {
        mode: loon_config::PermissionMode::Allowlist,
        allow_rules: vec![
            rule("bash", Some("cargo test"), None),
            rule("fetch", None, Some("example.org")),
        ],
        ask_rules: vec![rule("bash", Some("cargo test secrets"), None)],
        deny_rules: vec![rule("bash", Some("cargo test forbidden"), None)],
        ..Default::default()
    };
    let engine = PermissionEngine::new(&cfg, None);
    let root = std::path::Path::new("/tmp");
    assert!(engine
        .evaluate_call("bash", &json!({"command":"cargo test -p core"}), root, true)
        .is_allowed());
    assert_eq!(
        engine.evaluate_call(
            "bash",
            &json!({"command":"cargo test; rm -rf x"}),
            root,
            true
        ),
        Decision::NotAllowlisted
    );
    assert_eq!(
        engine.evaluate_call("bash", &json!({"command":"cargo test secrets"}), root, true),
        Decision::ApprovalRequired
    );
    assert!(matches!(
        engine.evaluate_call(
            "bash",
            &json!({"command":"cargo test forbidden"}),
            root,
            true
        ),
        Decision::Denied { .. }
    ));
    assert!(engine
        .evaluate_call(
            "fetch",
            &json!({"url":"https://example.org/a"}),
            root,
            false
        )
        .is_allowed());
    assert_eq!(
        engine.evaluate_call(
            "fetch",
            &json!({"url":"https://example.org.attacker/a"}),
            root,
            false
        ),
        Decision::NotAllowlisted
    );
}
#[test]
fn project_settings_merge_without_losing_provider_tables() {
    let d = tempfile::tempdir().unwrap();
    let paths = paths(d.path());
    paths.ensure().unwrap();
    Config::builtin().save(&paths).unwrap();
    let cwd = d.path().join("proj");
    std::fs::create_dir_all(cwd.join(".loon")).unwrap();
    std::fs::write(
        cwd.join(".loon/config.toml"),
        "[session]\neffort='high'\nautocompact_tokens=50000\n[ui]\nanimations=false\n",
    )
    .unwrap();
    std::fs::write(
        cwd.join(".loon/config.local.toml"),
        "[session]\neffort='ultra'\n",
    )
    .unwrap();
    let (cfg, findings) = Config::load_effective(&paths, &cwd).unwrap();
    assert!(!findings.iter().any(|f| f.is_error));
    assert!(cfg.providers.contains_key("mock"));
    assert_eq!(cfg.session.effort.as_deref(), Some("ultra"));
    assert_eq!(cfg.session.autocompact_tokens, Some(50_000));
    assert!(!cfg.ui.animations);
    assert_eq!(
        Config::load_or_builtin(&paths).unwrap().0.session.effort,
        None
    );
}

#[tokio::test]
async fn plan_mode_blocks_mutations_even_when_model_invents_a_hidden_call() {
    let d = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(
        d.path(),
        vec![
            MockScript::new(vec![MockStep::tool_call(
                "w",
                "write",
                json!({"path":"blocked.txt","content":"bad"}),
            )]),
            MockScript::text("proposal"),
            MockScript::new(vec![MockStep::tool_call(
                "w2",
                "write",
                json!({"path":"allowed.txt","content":"ok"}),
            )]),
            MockScript::text("done"),
        ],
    );
    command(&mut a, "/plan on").await;
    a.run_turn("make a plan").await;
    assert!(!a.ctx.ws.root().join("blocked.txt").exists());
    assert!(a
        .history
        .iter()
        .any(|m| m.text.contains("Plan mode blocks")));
    command(&mut a, "/plan off").await;
    a.run_turn("implement").await;
    assert!(a.ctx.ws.root().join("allowed.txt").exists());
}
#[tokio::test]
async fn configured_subagent_enforces_tools_and_turn_budget() {
    let d = tempfile::tempdir().unwrap();
    let paths = paths(d.path());
    paths.ensure().unwrap();
    let cwd = d.path().join("project");
    std::fs::create_dir_all(cwd.join(".loon/agents")).unwrap();
    std::fs::write(cwd.join(".loon/agents/reader.md"),"---\nname: reader\ndescription: Inspect Rust\nmodel: mock\neffort: high\nmaxTurns: 1\ntools: [read, grep, tree]\n---\nRead the supplied files and report evidence.").unwrap();
    let cfg = AgentConfig::from(&loon_config::SessionCfg::default());
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![
            MockScript::new(vec![MockStep::tool_call(
                "d",
                "delegate",
                json!({"agent":"reader","task":"inspect"}),
            )]),
            MockScript::new(vec![MockStep::tool_call(
                "hidden",
                "write",
                json!({"path":"must-not-exist","content":"bad"}),
            )]),
            MockScript::text("child incomplete; tool unavailable"),
        ],
    )));
    let tools = loon_core::subagent::default_tools(&paths, &cwd, &cfg, registry.clone(), None);
    let mut a = Agent::new_session(&paths, cfg, registry, tools, cwd.clone(), true).unwrap();
    a.run_turn("delegate inspection").await;
    assert!(!cwd.join("must-not-exist").exists());
    let result = a
        .history
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("d"))
        .unwrap();
    assert!(result.text.contains("maxTurns"));
    assert!(result.text.contains("failed"));
}
#[tokio::test]
async fn searching_remote_tools_loads_schema_and_hides_bare_denies() {
    use loon_tools::{Effects, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec};
    struct Remote(&'static str);
    #[async_trait::async_trait]
    impl Tool for Remote {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "remote repository search".into(),
                parameters: json!({"type":"object","properties":{}}),
                effects: Effects::ReadOnly,
            }
        }
        async fn run(&self, _: serde_json::Value, _: &ToolCtx) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok("found"))
        }
    }
    let d = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(d.path(), vec![MockScript::text("done")]);
    a.tools = a.tools.clone().with_extra(vec![
        Arc::new(Remote("mcp_fixture_search")),
        Arc::new(Remote("mcp_fixture_denied")),
    ]);
    a.cfg
        .permissions
        .deny_rules
        .push(loon_config::PermissionRule {
            tool: "mcp_fixture_denied".into(),
            path_prefix: None,
            command_prefix: None,
            host: None,
        });
    // Permission refresh also rebuilds the remote search catalog.
    a.set_permission_mode(loon_config::PermissionMode::Bypass);
    let tools_before = a.estimated_context_tokens();
    let search = a.tools.get("tool_search").unwrap();
    let result = search
        .run(json!({"query":"mcp_fixture_search"}), &a.ctx)
        .await
        .unwrap();
    assert!(result.text.contains("mcp_fixture_search"));
    assert!(!result.text.contains("mcp_fixture_denied"));
    assert!(
        a.estimated_context_tokens() > tools_before,
        "loaded schema enters the next request budget"
    );
}

#[tokio::test]
async fn visible_rewind_history_retains_original_messages_after_compaction() {
    let d = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(
        d.path(),
        vec![
            MockScript::text("answer one"),
            MockScript::text("answer two"),
            MockScript::text("answer three"),
            MockScript::text("answer four"),
        ],
    );
    for text in [
        "first long turn ".repeat(100),
        "second turn".into(),
        "third turn".into(),
    ] {
        a.run_turn(&text).await;
    }
    a.summarizer = Some(Arc::new(|_| {
        Box::pin(async { Ok("earlier work summarized".into()) })
    }));
    assert!(a.compact_now().await);
    a.run_turn("fourth turn").await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    a.set_event_sink(tx);
    command(&mut a, "/rewind 1 conversation").await;
    let mut checked = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ConversationRewound { blocks, .. } = event {
            assert_eq!(blocks.iter().filter(|(r, _)| r == "user").count(), 3);
            assert!(blocks.iter().any(|(_, t)| t == "answer one"));
            assert!(!blocks
                .iter()
                .any(|(_, t)| t.contains("Earlier conversation summary")));
            checked = true;
        }
    }
    assert!(checked);
}

/// Esc while the first request is in flight must *forget* the prompt: the user message
/// leaves live history and the journal, so a resume does not replay it, while the text
/// itself is handed back for the input bar.
#[tokio::test]
async fn cancelled_turn_is_forgotten_from_history_and_journal() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(d.path(), vec![]);
    a.run_turn("keep this one").await;
    // Simulate a cancelled turn: the user message is recorded, then forgotten.
    a.history
        .push(loon_providers::ChatMessage::user("drop this one"));
    a.journal
        .append(
            None,
            loon_events::model::EventKind::UserMessage {
                text: "drop this one".into(),
            },
        )
        .unwrap();
    let dropped = a.forget_cancelled_turn();
    assert_eq!(dropped.as_deref(), Some("drop this one"));
    let texts: Vec<&str> = a.history.iter().map(|m| m.text.as_str()).collect();
    assert!(
        !texts.iter().any(|t| t.contains("drop this one")),
        "forgotten from live history: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("keep this one")),
        "earlier turns are untouched: {texts:?}"
    );

    // The journal agrees: a resume replays without the cancelled prompt.
    let resumed = Agent::resume(
        &paths,
        a.session_id(),
        a.cfg.clone(),
        a.registry.clone(),
        loon_tools::ToolRegistry::standard(),
    )
    .unwrap();
    let replay: Vec<&str> = resumed.history.iter().map(|m| m.text.as_str()).collect();
    assert!(
        !replay.iter().any(|t| t.contains("drop this one")),
        "resume must not replay the cancelled prompt: {replay:?}"
    );
}
