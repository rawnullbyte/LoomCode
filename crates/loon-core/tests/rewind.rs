//! A08/A09: rewind boundaries — undo/redo round-trips, conflict preservation, workspace
//! rewind leaving earlier + unrelated edits intact, conversation rewind truncating
//! exactly, resume honoring the Rewound mark.

use loon_config::LoonPaths;
use loon_core::rewind::{self, RewindMode};
use loon_core::{Agent, AgentConfig};
use loon_events::model::EventKind;
use loon_events::Journal;
use loon_providers::{MockScript, MockStep, ProviderRegistry};
use loon_tools::ToolRegistry;
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

fn cfg() -> AgentConfig {
    AgentConfig {
        model_ref: "mock".into(),
        tool_output_bytes: 4096,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: None,
        ultra_rounds: 0,
        permissions: loon_config::Permissions::default(),
        context_window: 128_000,
        extra_tools: Vec::new(),
    }
}

fn ws_of(agent: &Agent) -> Arc<loon_workspace::Workspace> {
    agent.ctx.ws.clone()
}

#[tokio::test]
async fn undo_restores_exact_content_and_redo_reapplies() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("proj");
    std::fs::create_dir_all(&ws_root).unwrap();
    std::fs::write(ws_root.join("a.txt"), "version one\n").unwrap();

    let registry = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::new(vec![MockStep::tool_call(
            "c1",
            "write",
            json!({"path": "a.txt", "content": "version two\n"}),
        )]),
        MockScript::text("done"),
    ]));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        Arc::new(registry),
        ToolRegistry::standard(),
        ws_root.clone(),
        true,
    )
    .unwrap();
    let sid = agent.session_id();
    agent.run_turn("make it two").await;

    let store = loon_core::open_store(&paths).unwrap();
    let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    assert_eq!(
        std::fs::read_to_string(ws_root.join("a.txt")).unwrap(),
        "version two\n"
    );

    // undo
    let r = rewind::undo(&journal, &ws_of(&agent), &events).unwrap();
    assert_eq!(r.files_restored, 1, "{r}");
    assert_eq!(
        std::fs::read_to_string(ws_root.join("a.txt")).unwrap(),
        "version one\n"
    );

    // redo
    let (_, events2) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    let r = rewind::redo(&journal, &ws_of(&agent), &events2).unwrap();
    assert_eq!(r.files_restored, 1);
    assert_eq!(
        std::fs::read_to_string(ws_root.join("a.txt")).unwrap(),
        "version two\n"
    );

    // undo again after redo
    let (_, events3) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    rewind::undo(&journal, &ws_of(&agent), &events3).unwrap();
    assert_eq!(
        std::fs::read_to_string(ws_root.join("a.txt")).unwrap(),
        "version one\n"
    );
}

#[tokio::test]
async fn undo_conflict_preserves_user_work_and_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("proj");
    std::fs::create_dir_all(&ws_root).unwrap();
    std::fs::write(ws_root.join("a.txt"), "original\n").unwrap();

    let registry = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::new(vec![MockStep::tool_call(
            "c1",
            "write",
            json!({"path": "a.txt", "content": "by agent\n"}),
        )]),
        MockScript::text("done"),
    ]));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        Arc::new(registry),
        ToolRegistry::standard(),
        ws_root.clone(),
        true,
    )
    .unwrap();
    let sid = agent.session_id();
    agent.run_turn("go").await;
    assert_eq!(
        std::fs::read_to_string(ws_root.join("a.txt")).unwrap(),
        "by agent\n"
    );

    // user edits concurrently — undo must NOT clobber this
    std::fs::write(ws_root.join("a.txt"), "user's own edit\n").unwrap();

    let store = loon_core::open_store(&paths).unwrap();
    let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    let r = rewind::undo(&journal, &ws_of(&agent), &events).unwrap();
    assert_eq!(r.files_restored, 0);
    assert_eq!(r.conflicts.len(), 1);
    assert!(r.conflicts[0].contains("changed by someone else"));
    assert_eq!(
        std::fs::read_to_string(ws_root.join("a.txt")).unwrap(),
        "user's own edit\n",
        "user work preserved"
    );
}

#[tokio::test]
async fn workspace_rewind_restores_only_post_boundary_files() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("proj");
    std::fs::create_dir_all(&ws_root).unwrap();
    std::fs::write(ws_root.join("keep.txt"), "kept\n").unwrap();

    // turn 1 edits keep.txt; turn 2 creates new.txt
    let registry = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::new(vec![MockStep::tool_call(
            "c1",
            "edit",
            json!({"path": "keep.txt", "old_string": "kept", "new_string": "kept and changed"}),
        )]),
        MockScript::text("first turn done"),
        MockScript::new(vec![MockStep::tool_call(
            "c2",
            "write",
            json!({"path": "new.txt", "content": "created later\n"}),
        )]),
        MockScript::text("second turn done"),
    ]));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        Arc::new(registry),
        ToolRegistry::standard(),
        ws_root.clone(),
        true,
    )
    .unwrap();
    let sid = agent.session_id();
    agent.run_turn("turn one").await;
    agent.run_turn("turn two").await;
    assert!(ws_root.join("new.txt").exists());

    // rewind 1 request back (latest request = turn two's), workspace only
    let store = loon_core::open_store(&paths).unwrap();
    let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    let mut history = Vec::new();
    let out = rewind::rewind_n(
        &journal,
        &ws_of(&agent),
        &events,
        &mut history,
        1,
        RewindMode::WorkspaceOnly,
    )
    .unwrap();
    let r = out.restore.expect("workspace restore ran");
    assert_eq!(r.files_restored, 1, "only new.txt removed: {r}");
    assert!(
        !ws_root.join("new.txt").exists(),
        "post-boundary file removed"
    );
    assert_eq!(
        std::fs::read_to_string(ws_root.join("keep.txt")).unwrap(),
        "kept and changed\n",
        "pre-boundary edit untouched"
    );

    // Rewind past the remaining boundaries to the FIRST request (the edit of turn one;
    // each scripted response is its own request, so the edit sits 4 boundaries back).
    let (_, events2) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    let out = rewind::rewind_n(
        &journal,
        &ws_of(&agent),
        &events2,
        &mut history,
        4,
        RewindMode::ConversationAndWorkspace,
    )
    .unwrap();
    let rep = out.restore.expect("workspace restore ran");
    assert_eq!(rep.conflicts, Vec::<String>::new(), "report: {rep}");
    assert_eq!(rep.files_restored, 2, "report: {rep}");
    assert_eq!(
        std::fs::read_to_string(ws_root.join("keep.txt")).unwrap(),
        "kept\n"
    );
    assert_eq!(
        out.history_len_after, 0,
        "history truncated to before the first turn"
    );
}

#[tokio::test]
async fn resume_honors_rewound_mark() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("proj");
    std::fs::create_dir_all(&ws_root).unwrap();
    let registry = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::text("answer one"),
        MockScript::text("answer two"),
    ]));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        Arc::new(registry),
        ToolRegistry::standard(),
        ws_root.clone(),
        true,
    )
    .unwrap();
    let sid = agent.session_id();
    agent.run_turn("q1").await;
    agent.run_turn("q2").await;

    let store = loon_core::open_store(&paths).unwrap();
    let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    let mut history = Vec::new();
    rewind::rewind_n(
        &journal,
        &ws_of(&agent),
        &events,
        &mut history,
        1,
        RewindMode::ConversationOnly,
    )
    .unwrap();

    // Resume the same session: history must reflect the rewind (only q1 remains).
    let registry2 = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::text("resumed."),
    ]));
    let resumed = Agent::resume(
        &paths,
        sid,
        cfg(),
        Arc::new(registry2),
        ToolRegistry::standard(),
    )
    .unwrap();
    assert_eq!(
        resumed.history.len(),
        2,
        "user q1 + assistant answer one: {:?}",
        resumed
            .history
            .iter()
            .map(|m| (&m.role, &m.text))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn fork_and_checkpoint_are_journaled_and_restorable() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("proj");
    std::fs::create_dir_all(&ws_root).unwrap();
    std::fs::write(ws_root.join("f.txt"), "base\n").unwrap();

    let registry = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::new(vec![MockStep::tool_call(
            "c1",
            "write",
            json!({"path": "f.txt", "content": "changed\n"}),
        )]),
        MockScript::text("done"),
    ]));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        Arc::new(registry),
        ToolRegistry::standard(),
        ws_root.clone(),
        true,
    )
    .unwrap();
    let sid = agent.session_id();
    rewind::checkpoint(&agent.journal, &[], "before").unwrap();
    agent.run_turn("go").await;
    assert_eq!(
        std::fs::read_to_string(ws_root.join("f.txt")).unwrap(),
        "changed\n"
    );

    let store = loon_core::open_store(&paths).unwrap();
    let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    rewind::fork(&journal, &events, "experiment").unwrap();
    let r = rewind::restore_checkpoint(&journal, &ws_of(&agent), &events, "before").unwrap();
    assert_eq!(r.files_restored, 1);
    assert_eq!(
        std::fs::read_to_string(ws_root.join("f.txt")).unwrap(),
        "base\n"
    );

    // checkpoint + fork marks are in the journal
    let (_, events2) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    assert!(events2
        .iter()
        .any(|e| matches!(&e.kind, EventKind::CheckpointCreated { name, .. } if name == "before")));
    assert!(events2
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ForkCreated { name, .. } if name == "experiment")));
}

#[test]
fn nothing_to_undo_is_a_clean_error() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tempfile::tempdir().unwrap();
    let store = loon_core::open_store(&test_paths(d.path())).unwrap();
    let id = loon_events::model::EvId::now_v7();
    let j = Journal::create_default(
        &store,
        &loon_events::journal::SessionMeta::fresh(id, "t", 0, "/", true),
    )
    .unwrap();
    let ws = loon_workspace::Workspace::new(tmp.path(), vec![]).unwrap();
    let err = rewind::undo(&j, &ws, &[]).unwrap_err();
    assert!(matches!(err, rewind::RewindError::NothingToUndo));
}

/// The picker needs more than "rewound 1 boundary": it needs the prompt that opened
/// each turn and what that turn changed.
#[tokio::test]
async fn rewind_entries_describe_each_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("proj");
    std::fs::create_dir_all(&ws_root).unwrap();
    std::fs::write(ws_root.join("a.txt"), "one\ntwo\n").unwrap();

    let registry = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::new(vec![MockStep::tool_call(
            "c1",
            "write",
            json!({"path": "a.txt", "content": "one\ntwo\nthree\n"}),
        )]),
        MockScript::text("done"),
    ]));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        Arc::new(registry),
        ToolRegistry::standard(),
        ws_root.clone(),
        true,
    )
    .unwrap();
    let sid = agent.session_id();
    agent.run_turn("add a third line").await;

    let store = loon_core::open_store(&paths).unwrap();
    let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    let entries = rewind::rewind_entries(&journal, &events);
    // The picker has one entry for the user message, including every tool round.
    assert_eq!(entries.len(), 1, "one user message");
    let e = &entries[0];
    assert_eq!(e.n, 2);
    assert_eq!(e.prompt, "add a third line");
    assert_eq!(e.files_changed, 1, "a.txt was mutated");
    assert!(e.added >= 1, "the new line is counted: +{}", e.added);
}

/// `RewindEntry::prompt` is documented as "flattened to one line" because it is
/// rendered as a single menu row. It used to carry the raw message, so a
/// multi-line prompt split the picker entry across rows.
#[tokio::test]
async fn rewind_entries_flatten_a_multiline_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_root = tmp.path().join("proj");
    std::fs::create_dir_all(&ws_root).unwrap();

    let registry = ProviderRegistry::with_mock(loon_providers::MockProvider::with_scripts(vec![
        MockScript::text("done"),
    ]));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        Arc::new(registry),
        ToolRegistry::standard(),
        ws_root.clone(),
        true,
    )
    .unwrap();
    let sid = agent.session_id();
    agent.run_turn("first line\nsecond line").await;

    let store = loon_core::open_store(&paths).unwrap();
    let (journal, events) = Journal::open_default(&store, &paths.sessions_dir(), sid).unwrap();
    let entries = rewind::rewind_entries(&journal, &events);
    let e = entries.last().expect("one entry");
    assert!(
        !e.prompt.contains('\n'),
        "the picker row is one line, so the prompt must be flattened, got: {:?}",
        e.prompt
    );
    assert!(
        e.prompt.contains("first line second line"),
        "the flattened prompt keeps its words, got: {:?}",
        e.prompt
    );
}
