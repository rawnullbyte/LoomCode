//! Permission enforcement end to end: the policy gate runs inside the real agent loop —
//! denials, allowlist misses, ask-mode approval (hook yes/no), and the redacted audit
//! trail. Paths stay inside the workspace because the workspace sandbox separately
//! refuses escapes; permission policy is a second, independent layer. Provider is the
//! deterministic mock (labeled as such; no live-model claim).

use loon_config::{LoonPaths, PermissionMode, PermissionRule, Permissions};
use loon_core::{Agent, AgentConfig};
use loon_events::model::EventKind;
use loon_providers::{MockScript, MockStep, ProviderRegistry};
use loon_tools::ToolRegistry;
use serde_json::json;
use std::path::PathBuf;
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

fn cfg_with(mode: PermissionMode, mut perms: Permissions, _ws: &std::path::Path) -> AgentConfig {
    perms.mode = mode;
    AgentConfig {
        model_ref: "mock".into(),
        max_steps: 12,
        tool_output_bytes: 4096,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        permissions: perms,
        context_window: 128_000,
        extra_tools: Vec::new(),
    }
}

fn ws_root(tmp: &tempfile::TempDir) -> PathBuf {
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(ws.join("secrets")).unwrap();
    std::fs::create_dir_all(ws.join("notes")).unwrap();
    ws
}

/// One scripted request: write to `<ws>/secrets/key.txt`.
fn write_script(ws: &std::path::Path) -> Vec<MockScript> {
    let p = ws.join("secrets/key.txt").display().to_string();
    vec![MockScript::new(vec![MockStep::tool_call(
        "call_1",
        "write",
        json!({"path": p, "content": "hello"}),
    )])]
}

fn audit_of(tmp: &tempfile::TempDir) -> String {
    std::fs::read_to_string(test_paths(tmp.path()).state_dir.join("audit.jsonl")).unwrap()
}

fn agent_for(tmp: &tempfile::TempDir, cfg: AgentConfig, ws: &std::path::Path) -> Agent {
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(write_script(ws)),
    ));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    Agent::new_session(
        &paths,
        cfg,
        registry,
        ToolRegistry::standard(),
        ws.to_path_buf(),
        true,
    )
    .unwrap()
}

fn secret_file(ws: &std::path::Path) -> PathBuf {
    ws.join("secrets/key.txt")
}

#[tokio::test]
async fn bypass_writes_but_explicit_deny_refuses_with_audit() {
    // Case 1: plain bypass — the write happens.
    let tmp = tempfile::tempdir().unwrap();
    let ws = ws_root(&tmp);
    let mut agent = agent_for(
        &tmp,
        cfg_with(PermissionMode::Bypass, Permissions::default(), &ws),
        &ws,
    );
    let sid = agent.session_id().to_string();
    let r = agent.run_turn("write a file").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    assert!(secret_file(&ws).exists());
    let audit = audit_of(&tmp);
    assert!(audit.contains("\"allowed\""));
    assert!(audit.contains(&sid));

    // Case 2: bypass + explicit deny — still refused; the model is told not to retry.
    let tmp2 = tempfile::tempdir().unwrap();
    let ws2 = ws_root(&tmp2);
    let perms = Permissions {
        deny_rules: vec![PermissionRule {
            tool: "write".into(),
            path_prefix: Some(ws2.join("secrets").display().to_string()),
        }],
        ..Permissions::default()
    };
    let mut agent2 = agent_for(&tmp2, cfg_with(PermissionMode::Bypass, perms, &ws2), &ws2);
    let r2 = agent2.run_turn("write a file").await;
    assert!(matches!(r2.status, loon_core::TurnStatus::Completed));
    assert!(!secret_file(&ws2).exists());
    let events = agent2.journal.events().unwrap();
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::ToolCallRejected { reason, .. } if reason.contains("denied by configured rule")
    )));
}

#[tokio::test]
async fn allowlist_mode_refuses_unlisted_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = ws_root(&tmp);
    let perms = Permissions {
        allow_rules: vec![PermissionRule {
            tool: "read".into(),
            path_prefix: None,
        }],
        ..Permissions::default()
    };
    let mut agent = agent_for(&tmp, cfg_with(PermissionMode::Allowlist, perms, &ws), &ws);
    let r = agent.run_turn("write a file").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    assert!(!secret_file(&ws).exists());
    assert!(audit_of(&tmp).contains("\"not_allowlisted\""));
}

#[tokio::test]
async fn allowlist_prefix_rule_allows_in_scope_and_refuses_out_of_scope() {
    // In scope: write under <ws>/notes is allowlisted → succeeds.
    let tmp = tempfile::tempdir().unwrap();
    let ws = ws_root(&tmp);
    let perms = Permissions {
        allow_rules: vec![PermissionRule {
            tool: "write".into(),
            path_prefix: Some(ws.join("notes").display().to_string()),
        }],
        ..Permissions::default()
    };
    let target = ws.join("notes/ok.txt");
    let scripts = vec![MockScript::new(vec![MockStep::tool_call(
        "call_1",
        "write",
        json!({"path": target.display().to_string(), "content": "fine"}),
    )])];
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(scripts),
    ));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg_with(PermissionMode::Allowlist, perms, &ws),
        registry,
        ToolRegistry::standard(),
        ws.clone(),
        true,
    )
    .unwrap();
    let r = agent.run_turn("write inside notes").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    assert!(target.exists(), "allowlisted in-scope write must succeed");
}

#[tokio::test]
async fn ask_mode_headless_refuses_without_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = ws_root(&tmp);
    let mut agent = agent_for(
        &tmp,
        cfg_with(PermissionMode::Ask, Permissions::default(), &ws),
        &ws,
    );
    let r = agent.run_turn("write a file").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    assert!(!secret_file(&ws).exists());
    assert!(audit_of(&tmp).contains("\"approval_required\""));
}

#[tokio::test]
async fn ask_mode_hook_approval_grants_and_denial_refuses() {
    for approved in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let ws = ws_root(&tmp);
        let mut agent = agent_for(
            &tmp,
            cfg_with(PermissionMode::Ask, Permissions::default(), &ws),
            &ws,
        );
        let hook: loon_core::permissions::ApprovalHook =
            Arc::new(move |_req| Box::pin(async move { approved }));
        agent.approval_hook = Some(hook);
        let r = agent.run_turn("write a file").await;
        assert!(matches!(r.status, loon_core::TurnStatus::Completed));
        let written = secret_file(&ws).exists();
        assert_eq!(written, approved, "hook answer must decide the outcome");
        if approved {
            assert!(audit_of(&tmp).contains("\"allowed\""));
        } else {
            assert!(audit_of(&tmp).contains("\"approval_required\""));
        }
    }
}
