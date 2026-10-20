//! Slash commands end to end through a real agent (mock provider, labeled as such):
//! help, context report, model switching, memory save/show, diff on an empty session,
//! and honest errors for undo-with-nothing and unknown rewind modes.

use loon_config::LoonPaths;
use loon_core::commands::SlashCommand;
use loon_core::Agent;
use loon_providers::{MockScript, ProviderRegistry};
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
            max_steps: 4,
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            permissions: Default::default(),
            context_window: 128_000,
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
    let out = a.run_slash(SlashCommand::Context).await.unwrap();
    assert!(out.contains("est. tokens"), "{out}");
    assert!(out.contains("no compaction needed"), "{out}");
    assert!(out.contains("80% of window"), "{out}");

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
}

#[tokio::test]
async fn diff_lists_mutations_from_the_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    // Turn script: write a new file.
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![loon_providers::MockScript::new(vec![
            loon_providers::MockStep::tool_call(
                "call_1",
                "write",
                serde_json::json!({"path": "a.txt", "content": "one\\ntwo\\n"}),
            ),
        ])]),
    ));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut a = Agent::new_session(
        &paths,
        loon_core::AgentConfig {
            model_ref: "mock".into(),
            max_steps: 4,
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            permissions: Default::default(),
            context_window: 128_000,
        },
        registry,
        loon_core::standard_tools(&paths, &ws),
        ws.clone(),
        true,
    )
    .unwrap();
    let r = a.run_turn("update a.txt").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    let out = a.run_slash(SlashCommand::Diff).await.unwrap();
    assert!(out.contains("modified"), "{out}");
    assert!(out.contains("a.txt"), "{out}");
    assert!(!out.contains("created"), "{out}");
}
