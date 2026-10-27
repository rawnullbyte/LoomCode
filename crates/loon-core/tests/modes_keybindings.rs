//! Execution modes end to end through a real agent, plus `/keybind` persistence.
//!
//! Every assertion is on what the runtime did: the durable work state, the request the model
//! would have received, the file that was or was not written, and the configuration file on
//! disk. Nothing here trusts a status string alone.

use loon_config::LoonPaths;
use loon_core::commands::SlashCommand;
use loon_core::mode::Mode;
use loon_core::{Agent, TurnStatus};
use loon_providers::{MockScript, ProviderRegistry};
use serde_json::json;
use std::sync::Arc;

fn paths(dir: &std::path::Path) -> LoonPaths {
    LoonPaths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
        state_dir: dir.join("state"),
        cache_dir: dir.join("cache"),
        runtime_dir: dir.join("runtime"),
    }
}

fn agent(tmp: &std::path::Path, ws: &std::path::Path, scripts: Vec<MockScript>) -> Agent {
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(scripts),
    ));
    let paths = paths(tmp);
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

/// Every system message in one string, for asserting on the guidance the model receives.
fn system_text(messages: &[loon_providers::ChatMessage]) -> String {
    messages
        .iter()
        .filter(|m| m.role == loon_providers::Role::System)
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn workspace(tmp: &std::path::Path) -> std::path::PathBuf {
    let ws = tmp.join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    ws
}

/// Ask the agent to run a tool call, through the same gate a model-initiated call takes
/// (validation, permissions, and the plan-mode/read-only strategy restriction).
async fn call(agent: &Agent, name: &str, arguments: serde_json::Value) -> String {
    agent.call_tool_for_test(name, arguments).await
}

/// `plan` is enforced by the runtime: the mutating tool is refused and no file appears. This is
/// the property that makes the mode more than a label.
#[tokio::test]
async fn plan_mode_refuses_a_write_through_the_real_tool_path() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("planned")]);
    a.run_slash(SlashCommand::Mode {
        args: vec!["plan".into()],
    })
    .await
    .unwrap();
    assert!(a.worklog.read().unwrap().plan_mode);
    assert_eq!(a.mode(), Mode::Plan);

    let error = call(
        &a,
        "write",
        json!({"path": "created.txt", "content": "hello"}),
    )
    .await;
    assert!(
        error.contains("Plan mode") || error.contains("read-only"),
        "the refusal must name the reason: {error}"
    );
    assert!(
        !ws.join("created.txt").exists(),
        "the refused write must not have touched the filesystem"
    );
}

/// Review and security are read-only modes, and their refusal names the *mode* rather than
/// "plan mode", because the user never asked for plan mode.
#[tokio::test]
async fn review_mode_is_read_only_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("reviewed")]);
    a.run_slash(SlashCommand::Mode {
        args: vec!["review".into()],
    })
    .await
    .unwrap();
    let error = call(&a, "write", json!({"path": "nope.txt", "content": "x"})).await;
    assert!(error.contains("review"), "{error}");
    assert!(error.contains("read-only"), "{error}");
    assert!(!ws.join("nope.txt").exists());
}

/// Leaving the mode restores write access: the restriction is the mode, not a permanent lock.
#[tokio::test]
async fn returning_to_normal_restores_write_access() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("done")]);
    for mode in ["plan", "normal"] {
        a.run_slash(SlashCommand::Mode {
            args: vec![mode.into()],
        })
        .await
        .unwrap();
    }
    assert!(!a.worklog.read().unwrap().plan_mode);
    let out = call(
        &a,
        "write",
        json!({"path": "allowed.txt", "content": "content"}),
    )
    .await;
    assert!(
        out.contains("wrote"),
        "normal mode permits the write: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("allowed.txt")).unwrap(),
        "content"
    );
}

/// `debug` and `research` are strategies, not restrictions: they must keep write access or they
/// would be unusable for their actual purpose.
#[tokio::test]
async fn debug_and_research_keep_write_access() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    for mode in ["debug", "research"] {
        let mut a = agent(tmp.path(), &ws, vec![MockScript::text("ok")]);
        a.run_slash(SlashCommand::Mode {
            args: vec![mode.into()],
        })
        .await
        .unwrap();
        let out = call(
            &a,
            "write",
            json!({"path": format!("{mode}.txt"), "content": "x"}),
        )
        .await;
        assert!(
            out.contains("wrote"),
            "{mode} must allow writes, got: {out}"
        );
    }
}

/// `ultrawork` enables the independent completion supervisor, which is a real runtime switch —
/// and the change is reported so the status bar can show it.
#[tokio::test]
async fn ultrawork_turns_on_the_supervisor() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("done")]);
    let note = a
        .run_slash(SlashCommand::Mode {
            args: vec!["ultrawork".into()],
        })
        .await
        .unwrap();
    assert!(a.worklog.read().unwrap().ultraloop);
    assert!(note.contains("completion review"), "{note}");
    assert!(!a.worklog.read().unwrap().plan_mode, "ultrawork writes");
}

/// A mode is durable: the work state is journaled, so a resumed session restores the strategy.
#[tokio::test]
async fn the_mode_survives_a_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("hi")]);
    a.run_slash(SlashCommand::Mode {
        args: vec!["security".into()],
    })
    .await
    .unwrap();
    let id = a.session_id();
    a.run_turn("say hello").await;
    let paths = a.loon_paths();
    let resumed = Agent::resume(
        &paths,
        id,
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
        a.registry.clone(),
        loon_core::standard_tools(&paths, &ws),
    )
    .expect("session resumes");
    assert_eq!(resumed.mode(), Mode::Security);
    assert!(
        resumed.worklog.read().unwrap().plan_mode,
        "a read-only mode must come back read-only, not writable"
    );
}

/// An unknown mode is a usage error that lists the real options, not a silent no-op.
#[tokio::test]
async fn an_unknown_mode_lists_the_real_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("x")]);
    let error = a
        .run_slash(SlashCommand::Mode {
            args: vec!["turbo".into()],
        })
        .await
        .expect_err("an unknown mode must be refused");
    assert!(error.contains("turbo"), "{error}");
    for mode in Mode::ALL {
        assert!(error.contains(mode.id()), "{error} is missing {mode:?}");
    }
    assert_eq!(a.mode(), Mode::Normal, "a refused mode changes nothing");
}

/// `/mode NAME TASK` starts the task under the new mode in one step.
#[tokio::test]
async fn a_mode_with_a_task_starts_it_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(
        tmp.path(),
        &ws,
        vec![MockScript::text("the analysis is complete")],
    );
    let out = a
        .run_slash(SlashCommand::Mode {
            args: vec!["review".into(), "check the parser".into()],
        })
        .await
        .unwrap();
    assert!(out.contains("the analysis is complete"), "{out}");
    assert_eq!(a.mode(), Mode::Review);
}

/// A leading keyword selects the mode and is stripped from the request the model receives.
#[tokio::test]
async fn a_leading_keyword_routes_and_is_removed_from_the_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    // The supervisor reviews completion, so the script must be a valid verdict as well as prose.
    let mut a = agent(
        tmp.path(),
        &ws,
        vec![
            MockScript::text("fixed the parser"),
            MockScript::text(
                r#"{"status":"completed","reason":"the parser was fixed","evidence":["fixed the parser"]}"#,
            ),
        ],
    );
    let result = a.run_turn("ultracode fix the parser").await;
    assert_eq!(result.status, TurnStatus::Completed, "{:?}", result.status);
    assert_eq!(a.mode(), Mode::UltraCode);
    assert!(
        a.worklog.read().unwrap().ultraloop,
        "ultracode also enables the supervisor"
    );
    assert_eq!(a.effort.as_deref(), Some("ultra"));

    assert_eq!(
        a.user_prompts_for_test(),
        vec!["fix the parser".to_string()],
        "the keyword must be stripped before the request"
    );
}

/// The mode guidance reaches the model: a review turn is told it must not modify files. Without
/// this the mode would only be enforced by the tool gate, and the model would waste calls
/// discovering it.
#[tokio::test]
async fn the_active_mode_injects_its_guidance_into_the_request() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("ok")]);
    a.run_slash(SlashCommand::Mode {
        args: vec!["review".into()],
    })
    .await
    .unwrap();
    let messages = a.messages_for_test();
    let system = system_text(&messages);
    assert!(
        system.contains("Execution mode: review"),
        "the request must state the mode"
    );
    assert!(
        system.contains("do not modify"),
        "a read-only mode must tell the model it cannot write"
    );

    // Normal mode adds nothing, so the prompt does not grow for ordinary work.
    a.run_slash(SlashCommand::Mode {
        args: vec!["normal".into()],
    })
    .await
    .unwrap();
    let messages = a.messages_for_test();
    let system = system_text(&messages);
    assert!(
        !system.contains("Execution mode:"),
        "normal mode must not inject guidance"
    );
}

/// Ordinary prose never changes the mode: routing is confined to a leading keyword.
#[tokio::test]
async fn ordinary_prose_does_not_change_the_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("ok")]);
    a.run_turn("review the normal behaviour of the parser")
        .await;
    assert_eq!(
        a.mode(),
        Mode::Normal,
        "an English word in front of a sentence is a task, not a mode"
    );
}

// ── /keybind persistence ──────────────────────────────────────────────────────

/// A keybinding change is written to the user configuration file, and the persisted map resolves
/// the new chord. The file is the source of truth: a restart reads it back.
#[tokio::test]
async fn keybind_persists_and_reset_restores_the_default() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("x")]);
    let paths = a.loon_paths();

    let out = a
        .run_slash(SlashCommand::Keybind {
            args: vec!["open_palette".into(), "ctrl+k".into()],
        })
        .await
        .unwrap();
    assert!(out.contains("ctrl+k"), "{out}");

    let cfg = loon_config::Config::load_or_builtin(&paths).unwrap().0;
    assert_eq!(
        cfg.keys.bindings.get("open_palette").map(String::as_str),
        Some("ctrl+k")
    );
    let map = loon_config::KeyMap::from_config(&cfg.keys);
    assert_eq!(
        map.chord_for(loon_config::KeyAction::OpenPalette),
        Some(loon_config::Chord::ctrl('k'))
    );

    a.run_slash(SlashCommand::Keybind {
        args: vec!["reset".into(), "open_palette".into()],
    })
    .await
    .unwrap();
    let cfg = loon_config::Config::load_or_builtin(&paths).unwrap().0;
    assert!(
        !cfg.keys.bindings.contains_key("open_palette"),
        "reset must drop the override rather than pin the default"
    );
    let map = loon_config::KeyMap::from_config(&cfg.keys);
    assert_eq!(
        map.chord_for(loon_config::KeyAction::OpenPalette),
        Some(loon_config::Chord::ctrl('p'))
    );
}

/// A reserved chord is refused with the reason and nothing is written.
#[tokio::test]
async fn a_reserved_chord_is_refused_and_not_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("x")]);
    let error = a
        .run_slash(SlashCommand::Keybind {
            args: vec!["help".into(), "ctrl+z".into()],
        })
        .await
        .expect_err("ctrl+z suspends the process");
    assert!(error.contains("suspend"), "{error}");
    let cfg = loon_config::Config::load_or_builtin(&a.loon_paths())
        .unwrap()
        .0;
    assert!(!cfg.keys.bindings.contains_key("help"));
}

/// A profile switch changes the familiar chords and records nothing per-action, so a later
/// profile change is not polluted by stale overrides.
#[tokio::test]
async fn a_profile_switch_replaces_the_familiar_bindings() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("x")]);
    let out = a
        .run_slash(SlashCommand::Keybind {
            args: vec!["profile".into(), "emacs".into()],
        })
        .await
        .unwrap();
    assert!(out.contains("emacs"), "{out}");
    let cfg = loon_config::Config::load_or_builtin(&a.loon_paths())
        .unwrap()
        .0;
    let map = loon_config::KeyMap::from_config(&cfg.keys);
    assert_eq!(
        map.chord_for(loon_config::KeyAction::HistoryPrevious),
        Some(loon_config::Chord::ctrl('p'))
    );
    assert_eq!(
        map.chord_for(loon_config::KeyAction::OpenPalette),
        Some(loon_config::Chord::alt('p')),
        "the palette must move, not silently shadow history"
    );
    assert!(loon_config::keymap::validate(&cfg.keys).is_empty());
}

/// Taking a chord another action owns is allowed but never silent: the reply names the action
/// that lost the key and how to restore it.
#[tokio::test]
async fn stealing_a_chord_reports_who_lost_it() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("x")]);
    let out = a
        .run_slash(SlashCommand::Keybind {
            args: vec!["help".into(), "ctrl+p".into()],
        })
        .await
        .unwrap();
    assert!(out.contains("help"), "{out}");
    assert!(
        out.contains("open_palette"),
        "the reply must name the action that lost the key: {out}"
    );
    assert!(
        out.contains("reset open_palette"),
        "the reply must say how to restore it: {out}"
    );
    let cfg = loon_config::Config::load_or_builtin(&a.loon_paths())
        .unwrap()
        .0;
    let map = loon_config::KeyMap::from_config(&cfg.keys);
    assert_eq!(
        map.chord_for(loon_config::KeyAction::Help),
        Some(loon_config::Chord::ctrl('p'))
    );
    assert_eq!(
        map.chord_for(loon_config::KeyAction::OpenPalette),
        None,
        "the displaced action really loses the chord"
    );
    assert!(loon_config::keymap::validate(&cfg.keys).is_empty());
}

/// `/keybind list` reports the effective binding for every action, not just the changed ones.
#[tokio::test]
async fn keybind_list_shows_every_action() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let mut a = agent(tmp.path(), &ws, vec![MockScript::text("x")]);
    let out = a
        .run_slash(SlashCommand::Keybind {
            args: vec!["list".into()],
        })
        .await
        .unwrap();
    for action in loon_config::KeyAction::ALL {
        assert!(out.contains(action.id()), "missing {}", action.id());
    }
}
