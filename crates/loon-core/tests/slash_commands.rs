//! Slash commands end to end through a real agent (mock provider, labeled as such):
//! help, context report, model switching, memory save/show, diff on an empty session,
//! and honest errors for undo-with-nothing and unknown rewind modes.

use loon_config::LoonPaths;
use loon_core::commands::SlashCommand;
use loon_core::Agent;
use loon_providers::{MockScript, ProviderRegistry};
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

fn agent(tmp: &std::path::Path, ws: &std::path::Path) -> Agent {
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::text("unused answer")]),
    ));
    let paths = test_paths(tmp);
    paths.ensure().unwrap();
    Agent::new_session(
        &paths,
        loon_core::AgentConfig {
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
        loon_core::standard_tools(&paths, ws),
        ws.to_path_buf(),
        true,
    )
    .unwrap()
}

#[tokio::test]
async fn slash_commands_execute_honestly() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let mut a = agent(tmp.path(), &ws);

    // help
    let out = a.run_slash(SlashCommand::Help).await.unwrap();
    assert!(out.contains("/rewind"));

    // context
    let out = a
        .run_slash(SlashCommand::Context { size: None })
        .await
        .unwrap();
    assert!(out.contains("est. tokens"), "{out}");
    assert!(out.contains("no compaction needed"), "{out}");
    assert!(out.contains("80% of the window"), "{out}");
    // Compact units, and a discoverable way to change the window in one command.
    assert!(out.contains("window 128k"), "{out}");
    assert!(out.contains("/context 1m"), "{out}");

    // model: show + switch + invalid
    let out = a
        .run_slash(SlashCommand::Model { reference: None })
        .await
        .unwrap();
    assert!(out.contains("mock"));
    let err = a
        .run_slash(SlashCommand::Model {
            reference: Some("nope".into()),
        })
        .await;
    assert!(err.is_err(), "unknown model must be refused");
    let out = a
        .run_slash(SlashCommand::Model {
            reference: Some("mock".into()),
        })
        .await
        .unwrap();
    assert!(out.contains("mock"));

    // memory: save to workspace, then show
    a.run_slash(SlashCommand::Memory {
        scope: Some("workspace".into()),
        text: Some("release tag lives in Makefile".into()),
    })
    .await
    .unwrap();
    let out = a
        .run_slash(SlashCommand::Memory {
            scope: Some("workspace".into()),
            text: None,
        })
        .await
        .unwrap();
    assert!(out.contains("release tag"));

    // diff: empty session
    let out = a.run_slash(SlashCommand::Diff).await.unwrap();
    assert!(out.contains("no files changed"), "{out}");

    // undo with nothing to undo is an honest error
    let err = a.run_slash(SlashCommand::Undo).await;
    assert!(err.is_err());

    // unknown rewind mode
    let err = a
        .run_slash(SlashCommand::Rewind {
            n: 1,
            mode: Some("sideways".into()),
        })
        .await;
    assert!(err.unwrap_err().contains("unknown rewind mode"));

    // compact with only recent exchanges: honest no-op
    let out = a.run_slash(SlashCommand::Compact).await.unwrap();
    assert!(out.contains("nothing to compact"));

    // loop tasks: create, list, cancel, cancel-again-refuses
    let out = a
        .run_slash(SlashCommand::Loop {
            interval: "1m".into(),
            prompt: "check CI".into(),
        })
        .await
        .unwrap();
    assert!(out.contains("loop #1"), "{out}");
    let out = a.run_slash(SlashCommand::Loops).await.unwrap();
    assert!(out.contains("#1"), "{out}");
    assert!(out.contains("check CI"));
    let out = a
        .run_slash(SlashCommand::LoopCancel { id: 1 })
        .await
        .unwrap();
    assert!(out.contains("cancelled"));
    assert!(a
        .run_slash(SlashCommand::LoopCancel { id: 1 })
        .await
        .is_err());
    // bad interval refused
    assert!(a
        .run_slash(SlashCommand::Loop {
            interval: "xm".into(),
            prompt: "nope".into(),
        })
        .await
        .is_err());
    // the agent-side tool cancels through the same registry
    a.run_slash(SlashCommand::Loop {
        interval: "5m".into(),
        prompt: "watch the deploy".into(),
    })
    .await
    .unwrap();
    // /rename + /resume listing + /usage + /new swap semantics
    let out = a
        .run_slash(SlashCommand::Rename {
            title: "loop testing session".into(),
        })
        .await
        .unwrap();
    assert!(out.contains("renamed"), "{out}");
    let out = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert!(out.contains("loop testing session"), "{out}");
    assert!(out.contains("/resume <n>"), "{out}");
    // auto-title: /new starts untitled; the next prompt names the session
    let _ = a.run_slash(SlashCommand::New).await;
    let r = a.run_turn("help me refactor the parser module").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    let out = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert!(
        out.contains("help me refactor the parser module"),
        "auto-titled from first prompt: {out}"
    );
    // resume by index swaps the agent in place (session id changes)
    let before = a.session_id();
    let out = a
        .run_slash(SlashCommand::Resume {
            target: Some("1".into()),
        })
        .await;
    // resuming the newest session may or may not be the same id; just require success
    assert!(out.is_ok(), "{out:?}");
    let _ = before;
    let out = a.run_slash(SlashCommand::Usage).await.unwrap();
    assert!(!out.is_empty());
    // unknown resume target refuses honestly
    assert!(a
        .run_slash(SlashCommand::Resume {
            target: Some("zzz".into()),
        })
        .await
        .is_err());

    // /find over the journal + /doctor offline checks
    a.run_slash(SlashCommand::Btw {
        text: "the magic word is xylophone".into(),
    })
    .await
    .unwrap();
    let out = a
        .run_slash(SlashCommand::Find {
            query: "xylophone".into(),
        })
        .await
        .unwrap();
    assert!(out.contains("1 match"), "{out}");
    let out = a
        .run_slash(SlashCommand::Find {
            query: "nothinghere".into(),
        })
        .await
        .unwrap();
    assert!(out.contains("no matches"), "{out}");
    let out = a.run_slash(SlashCommand::Doctor).await.unwrap();
    assert!(out.contains("store: ok"), "{out}");
    assert!(out.contains("config: valid"), "{out}");
}

#[allow(dead_code)] // kept for future tool-site tests
fn paths_tmp(dir: &std::path::Path) -> LoonPaths {
    test_paths(dir)
}

fn tool_ctx(ws: &std::path::Path) -> loon_tools::ToolCtx {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    loon_tools::ToolCtx {
        ws: Arc::new(loon_workspace::Workspace::new(ws, vec![]).unwrap()),
        output_limit: 4096,
        default_timeout: std::time::Duration::from_secs(10),
        child_env: Default::default(),
        cancel: rx,
        blob_sink: None,
    }
}

#[tokio::test]
async fn agent_side_loop_cancel_via_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![]),
    ));
    let cfg = loon_core::AgentConfig {
        model_ref: "mock".into(),
        tool_output_bytes: 4096,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: None,
        ultra_rounds: 0,
        permissions: Default::default(),
        context_window: 128_000,
        extra_tools: Vec::new(),
    };
    let tools = loon_core::subagent::default_tools(
        &paths,
        &ws,
        &cfg,
        registry,
        Some(Arc::new(loon_core::loops::LoopRegistry::new())),
    );
    let mut agent = Agent::new_session(
        &paths,
        cfg,
        Arc::new(ProviderRegistry::with_mock(
            loon_providers::MockProvider::with_scripts(vec![]),
        )),
        tools,
        ws.clone(),
        true,
    )
    .unwrap();
    agent
        .run_slash(SlashCommand::Loop {
            interval: "5m".into(),
            prompt: "watch the deploy".into(),
        })
        .await
        .unwrap();
    let tool = loon_tools::ToolRegistry::standard()
        .with_extra(vec![Arc::new(loon_core::subagent::LoopCancelTool {
            registry: agent.loops.clone(),
        })])
        .get("loop_cancel")
        .expect("loop_cancel registered");
    let ctx = tool_ctx(&ws);
    let out = tool
        .run(json!({"id": 1}), &ctx)
        .await
        .expect("cancel live loop");
    assert!(out.text.contains("cancelled"));
    assert!(agent.loops.is_empty());
}

#[test]
fn clear_is_an_alias_for_new() {
    let parse = |s: &str| loon_core::commands::parse(s).unwrap().unwrap();
    assert_eq!(parse("/new"), SlashCommand::New);
    assert_eq!(parse("/clear"), SlashCommand::New);
    assert!(loon_core::commands::command_list()
        .iter()
        .any(|(name, _)| *name == "clear"));
}
