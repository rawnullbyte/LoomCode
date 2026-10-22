use loon_config::{Config, LoonPaths};
use loon_core::{commands::SlashCommand, config_ui::encode, Agent, AgentConfig, AgentEvent};
use loon_providers::{MockProvider, ProviderRegistry};
use serde_json::json;
use std::sync::Arc;
fn agent(root: &std::path::Path) -> (LoonPaths, Agent, tokio::sync::mpsc::Receiver<AgentEvent>) {
    let paths = LoonPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        runtime_dir: root.join("runtime"),
    };
    paths.ensure().unwrap();
    Config::builtin().save(&paths).unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(Arc::new(MockProvider::new())));
    let cfg = AgentConfig::from(&loon_config::SessionCfg::default());
    let tools = loon_core::subagent::default_tools(&paths, &cwd, &cfg, registry.clone(), None);
    let mut a = Agent::new_session(&paths, cfg, registry, tools, cwd, true).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(512);
    a.set_event_sink(tx);
    (paths, a, rx)
}
async fn ui(a: &mut Agent, op: &str, path: &str, value: Option<&str>) -> Result<String, String> {
    let mut args = vec!["ui".into(), op.into(), encode(path)];
    if let Some(v) = value {
        args.push(v.into());
    }
    a.run_slash(SlashCommand::Config { args }).await
}
fn drain(rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    events
}
fn menus(events: &[AgentEvent]) -> Vec<loon_core::catalog::MenuItem> {
    events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::OpenMenu { items, .. } = e {
                Some(items.clone())
            } else {
                None
            }
        })
        .flatten()
        .collect()
}
#[tokio::test]
async fn every_serialized_field_is_discoverable_in_ui_and_named_collections_can_be_added() {
    let d = tempfile::tempdir().unwrap();
    let (_, mut a, mut rx) = agent(d.path());
    for (path, name) in [
        ("/providers", "remote"),
        ("/models", "custom"),
        ("/combos", "daily"),
        ("/mcp/servers", "docs"),
        ("/hooks", "Stop"),
    ] {
        ui(&mut a, "insert", path, Some(name)).await.unwrap();
    }
    for path in [
        "/combos/daily/members",
        "/hooks/Stop",
        "/permissions/allow_rules",
        "/ui/spinner_verbs",
    ] {
        ui(&mut a, "add", path, None).await.unwrap();
    }
    for (path, name) in [
        ("/providers/remote/headers", "X-Team"),
        ("/mcp/servers/docs/env", "CUSTOM_KEY"),
    ] {
        ui(&mut a, "insert", path, Some(name)).await.unwrap();
    }
    drain(&mut rx);
    // Walk the generated menu tree, including optional fields, lists and empty tables.
    let mut queue = vec![String::new()];
    let mut visited = std::collections::BTreeSet::new();
    let mut edits = std::collections::BTreeSet::new();
    while let Some(path) = queue.pop() {
        if !visited.insert(path.clone()) {
            continue;
        }
        ui(&mut a, "open", &path, None).await.unwrap();
        for item in menus(&drain(&mut rx)) {
            if let Some(Ok(SlashCommand::Config { args })) =
                loon_core::commands::parse(&item.command)
            {
                if args.first().is_some_and(|s| s == "ui")
                    && args.get(1).is_some_and(|s| s == "open" || s == "edit")
                {
                    let raw = args.get(2).map(String::as_str).unwrap_or("");
                    let bytes = (0..raw.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).unwrap())
                        .collect::<Vec<_>>();
                    let p = String::from_utf8(bytes).unwrap();
                    if args[1] == "open" {
                        queue.push(p);
                    } else {
                        edits.insert(p);
                    }
                }
            }
        }
    }
    for path in [
        "/context7/api_key",
        "/context7/enabled",
        "/context7/api_key_env",
        "/context7/timeout_secs",
        "/providers/remote/headers/X-Team",
        "/providers/remote/timeout_secs",
        "/providers/remote/mock",
        "/models/custom/max_output_tokens",
        "/models/custom/capabilities/parallel_tool_calls",
        "/combos/daily/members/0/max_retries",
        "/combos/daily/requires/min_context_window",
        "/combos/daily/persistent_rotation",
        "/mcp/servers/docs/env/CUSTOM_KEY",
        "/mcp/servers/docs/timeout_secs",
        "/hooks/Stop/0/matcher",
        "/permissions/allow_rules/0/command_prefix",
        "/appearance/color",
        "/appearance/ascii_only",
        "/session/allow_retry_after_partial_stream",
        "/ui/footer",
        "/ui/tip_text",
        "/ui/spinner_verbs/0",
        "/ui/welcome_banner",
    ] {
        assert!(edits.contains(path), "missing UI field: {path}");
    }
    assert!(!edits.contains("/schema_version"));
    // Removing individual collection values does not expose arbitrary required-field deletion.
    ui(&mut a, "remove", "/mcp/servers/docs/env/CUSTOM_KEY", None)
        .await
        .unwrap();
    assert!(ui(&mut a, "remove", "/session/model", None).await.is_err());
}
#[tokio::test]
async fn context7_is_configurable_without_env_or_restart_and_secrets_do_not_enter_events() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a, mut rx) = agent(d.path());
    ui(
        &mut a,
        "set",
        "/context7/api_key",
        Some("fixture-ui-key-never-print"),
    )
    .await
    .unwrap();
    ui(
        &mut a,
        "set",
        "/context7/api_key_env",
        Some("LOOM_UI_TEST_MISSING_KEY_ENV_27"),
    )
    .await
    .unwrap();
    ui(&mut a, "set", "/context7/timeout_secs", Some("45"))
        .await
        .unwrap();
    ui(&mut a, "set", "/context7/enabled", Some("false"))
        .await
        .unwrap();
    assert!(
        Config::load_or_builtin(&paths)
            .unwrap()
            .0
            .context7
            .api_key
            .is_none(),
        "draft saved too soon"
    );
    ui(&mut a, "save", "/context7", None).await.unwrap();
    let cfg = Config::load_or_builtin(&paths).unwrap().0;
    assert_eq!(
        cfg.context7.api_key.as_deref(),
        Some("fixture-ui-key-never-print")
    );
    assert_eq!(cfg.context7.timeout_secs, 45);
    assert!(!cfg.context7.enabled);
    let err = a
        .tools
        .get("context7_search")
        .unwrap()
        .run(json!({"query":"Rust"}), &a.ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("disabled"));
    ui(&mut a, "edit", "/context7/api_key", None).await.unwrap();
    let events = drain(&mut rx);
    for e in &events {
        assert!(!format!("{e:?}").contains("fixture-ui-key-never-print"));
    }
    let editor = events
        .iter()
        .find_map(|e| {
            if let AgentEvent::OpenEditor(e) = e {
                Some(e)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(editor.kind, "config-secret");
    assert!(editor.values[1].is_empty());
    ui(&mut a, "set", "/context7/api_key", Some(""))
        .await
        .unwrap();
    ui(&mut a, "save", "/context7", None).await.unwrap();
    assert!(Config::load_or_builtin(&paths)
        .unwrap()
        .0
        .context7
        .api_key
        .is_some());
    ui(&mut a, "clear", "/context7/api_key", None)
        .await
        .unwrap();
    ui(&mut a, "set", "/context7/enabled", Some("true"))
        .await
        .unwrap();
    ui(&mut a, "save", "/context7", None).await.unwrap();
    assert!(Config::load_or_builtin(&paths)
        .unwrap()
        .0
        .context7
        .api_key
        .is_none());
    let err = ui(&mut a, "test", "/context7", None).await.unwrap_err();
    assert!(err.contains("/settings"));
    assert!(!err.contains("fixture-ui-key"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(paths.config_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[tokio::test]
async fn invalid_drafts_and_external_changes_are_rejected_and_discard_recovers() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a, _) = agent(d.path());
    ui(&mut a, "set", "/context7/timeout_secs", Some("0"))
        .await
        .unwrap();
    assert!(ui(&mut a, "save", "", None)
        .await
        .unwrap_err()
        .contains("timeout_secs"));
    assert_eq!(
        Config::load_or_builtin(&paths)
            .unwrap()
            .0
            .context7
            .timeout_secs,
        30
    );
    ui(&mut a, "discard", "", None).await.unwrap();
    ui(&mut a, "set", "/ui/tip_text", Some("draft hint"))
        .await
        .unwrap();
    let (mut cfg, _) = Config::load_or_builtin(&paths).unwrap();
    cfg.ui.tip_text = "external hint".into();
    cfg.save(&paths).unwrap();
    assert!(ui(&mut a, "save", "", None)
        .await
        .unwrap_err()
        .contains("outside"));
    assert_eq!(
        Config::load_or_builtin(&paths).unwrap().0.ui.tip_text,
        "external hint"
    );
    ui(&mut a, "discard", "", None).await.unwrap();
    ui(&mut a, "set", "/ui/tip_text", Some("saved hint"))
        .await
        .unwrap();
    ui(&mut a, "save", "/ui", None).await.unwrap();
    assert_eq!(
        Config::load_or_builtin(&paths).unwrap().0.ui.tip_text,
        "saved hint"
    );
}
#[tokio::test]
async fn advanced_settings_apply_to_runtime_and_project_overrides_keep_precedence() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a, mut rx) = agent(d.path());
    let extra = d.path().join("extra");
    std::fs::create_dir(&extra).unwrap();
    ui(&mut a, "add", "/permissions/extra_write_roots", None)
        .await
        .unwrap();
    ui(
        &mut a,
        "set",
        "/permissions/extra_write_roots/0",
        Some(extra.to_str().unwrap()),
    )
    .await
    .unwrap();
    for (path, value) in [
        ("/session/tool_output_bytes", "4096"),
        ("/session/allow_retry_after_partial_stream", "true"),
        ("/permissions/mode", "ask"),
        ("/ui/tip_text", "custom hint"),
        ("/ui/footer", "false"),
        ("/appearance/color", "mono"),
    ] {
        ui(&mut a, "set", path, Some(value)).await.unwrap();
    }
    ui(&mut a, "save", "", None).await.unwrap();
    assert_eq!(a.ctx.output_limit, 4096);
    assert!(a.cfg.allow_retry_after_partial_stream);
    assert_eq!(a.cfg.permissions.mode, loon_config::PermissionMode::Ask);
    a.ctx.ws.check_writable(&extra.join("allowed.txt")).unwrap();
    let events = drain(&mut rx);
    assert!(events.iter().any(
        |e| matches!(e,AgentEvent::UiPreference{key,value} if key=="tip_text"&&value=="custom hint")
    ));
    let project = a.ctx.ws.root().join(".loon");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("config.toml"), "[context7]\ntimeout_secs=70\n").unwrap();
    ui(&mut a, "discard", "", None).await.unwrap();
    ui(&mut a, "set", "/context7/timeout_secs", Some("45"))
        .await
        .unwrap();
    ui(&mut a, "save", "/context7", None).await.unwrap();
    assert_eq!(
        Config::load_or_builtin(&paths)
            .unwrap()
            .0
            .context7
            .timeout_secs,
        45
    );
    assert_eq!(
        Config::load_effective(&paths, a.ctx.ws.root())
            .unwrap()
            .0
            .context7
            .timeout_secs,
        70
    );
    assert!(ui(&mut a, "open", "é", None).await.is_err());
}
#[tokio::test]
async fn project_and_local_scopes_save_sparse_overrides_and_preserve_other_values() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a, _) = agent(d.path());
    ui(&mut a, "scope", "project", None).await.unwrap();
    ui(&mut a, "set", "/context7/timeout_secs", Some("60"))
        .await
        .unwrap();
    ui(&mut a, "save", "/context7", None).await.unwrap();
    let project = a.ctx.ws.root().join(".loon/config.toml");
    let text = std::fs::read_to_string(&project).unwrap();
    assert!(text.contains("timeout_secs = 60"));
    assert!(!text.contains("providers"), "flattened inherited settings");
    assert!(!text.contains("api_key"));
    assert_eq!(
        Config::load_or_builtin(&paths)
            .unwrap()
            .0
            .context7
            .timeout_secs,
        30
    );
    ui(&mut a, "scope", "local", None).await.unwrap();
    ui(&mut a, "set", "/context7/timeout_secs", Some("15"))
        .await
        .unwrap();
    ui(&mut a, "save", "/context7", None).await.unwrap();
    assert_eq!(
        Config::load_effective(&paths, a.ctx.ws.root())
            .unwrap()
            .0
            .context7
            .timeout_secs,
        15
    );
    ui(&mut a, "scope", "project", None).await.unwrap();
    ui(&mut a, "set", "/ui/tip_text", Some("team hint"))
        .await
        .unwrap();
    assert!(ui(&mut a, "scope", "user", None)
        .await
        .unwrap_err()
        .contains("Save or discard"));
    ui(&mut a, "save", "/ui", None).await.unwrap();
    let cfg = Config::load_effective(&paths, a.ctx.ws.root()).unwrap().0;
    assert_eq!(cfg.ui.tip_text, "team hint");
    assert_eq!(cfg.context7.timeout_secs, 15);
    ui(&mut a, "remove", "/providers/mock", None).await.unwrap();
    let err = ui(&mut a, "save", "", None).await.unwrap_err();
    assert!(err.contains("provider") || err.contains("inherited"));
    ui(&mut a, "discard", "", None).await.unwrap();
    // An unrelated nested extension and comments survive a scoped field edit.
    let original = std::fs::read_to_string(&project).unwrap();
    std::fs::write(
        &project,
        format!("# team config\n{original}\n[context7.extension]\ncustom = 'preserve'\n"),
    )
    .unwrap();
    ui(&mut a, "discard", "", None).await.unwrap();
    ui(&mut a, "set", "/context7/timeout_secs", Some("65"))
        .await
        .unwrap();
    ui(&mut a, "save", "/context7", None).await.unwrap();
    let saved = std::fs::read_to_string(project).unwrap();
    assert!(saved.contains("# team config"));
    assert!(saved.contains("custom = 'preserve'"));
}
#[tokio::test]
async fn session_controls_are_editable_in_settings_and_persist_with_the_session() {
    let d = tempfile::tempdir().unwrap();
    let (paths, mut a, _) = agent(d.path());
    for (k, v) in [
        ("ultraloop", "true"),
        ("ultraloop_limit", "12"),
        ("supervisor_model", "mock"),
        ("plan_mode", "true"),
        ("session_goal", "Finish the parser"),
    ] {
        a.config_apply(k, v).unwrap();
    }
    let state = a.worklog.read().unwrap();
    assert!(state.ultraloop && state.plan_mode);
    assert_eq!(state.ultraloop_limit, 12);
    assert_eq!(state.supervisor_model.as_deref(), Some("mock"));
    assert_eq!(a.session_goal.as_deref(), Some("Finish the parser"));
    assert!(a
        .config_apply("supervisor_model", "missing/not-a-model")
        .is_err());
    // Settings must not resurrect an empty session: nothing sent yet, so the store
    // still has no trace of it.
    let store = loon_core::open_store(&paths).unwrap();
    assert!(
        store.list_sessions().unwrap().is_empty(),
        "settings on a message-less session must not create a session row"
    );
    // A real user action afterwards materializes it (/btw journals a user message
    // without a model turn), and only then can a fresh agent resume it and read the
    // persisted settings back.
    a.run_slash(SlashCommand::Btw {
        text: "settings should survive".into(),
    })
    .await
    .unwrap();
    let resumed = Agent::resume(
        &paths,
        a.session_id(),
        a.cfg.clone(),
        a.registry.clone(),
        loon_tools::ToolRegistry::standard(),
    )
    .unwrap();
    assert_eq!(resumed.setting_value("ultraloop"), "true");
    assert_eq!(resumed.setting_value("session_goal"), "Finish the parser");
}
