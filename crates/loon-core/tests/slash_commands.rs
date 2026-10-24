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

/// Regression: `/new` (and `/clear`) must not persist a session until the user actually
/// says something, and `/resume` must not be polluted with empty sessions.
#[tokio::test]
async fn new_and_clear_do_not_persist_an_empty_session() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let mut a = agent(tmp.path(), &ws);

    // Fresh session: nothing written.
    let store = loon_core::open_store(&loon_config::LoonPaths {
        config_dir: tmp.path().join("config"),
        data_dir: tmp.path().join("data"),
        state_dir: tmp.path().join("state"),
        cache_dir: tmp.path().join("cache"),
        runtime_dir: tmp.path().join("runtime"),
    })
    .unwrap();
    assert!(store.list_sessions().unwrap().is_empty());

    // /new twice: still nothing to resume.
    for _ in 0..2 {
        a.run_slash(SlashCommand::New).await.unwrap();
    }
    assert!(
        store.list_sessions().unwrap().is_empty(),
        "switching sessions without a message must not create sessions"
    );
    let out = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert_eq!(out.trim(), "no sessions yet", "resume list must stay empty");

    // Sending one message creates exactly one session, titled from the message.
    let r = a.run_turn("only real message").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    let listed = store.list_sessions().unwrap();
    assert_eq!(listed.len(), 1, "exactly one session after one message");
    assert_eq!(listed[0].title, "only real message");

    // /new again, then quit: the one real session is untouched and no empty one appears.
    a.run_slash(SlashCommand::New).await.unwrap();
    assert_eq!(
        store.list_sessions().unwrap().len(),
        1,
        "a new empty session must not be persisted"
    );
}

/// /delete removes a saved session by index or id, refuses the current session, and
/// never resurrects a deleted one.
#[tokio::test]
async fn delete_removes_saved_sessions_but_not_the_current_one() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let mut a = agent(tmp.path(), &ws);

    // Create a real session (a message materializes it), then move on so it is
    // deletable: /new keeps it in the journal and switches to a fresh session.
    let first = a.session_id();
    let r = a.run_turn("first session body").await;
    assert!(matches!(r.status, loon_core::TurnStatus::Completed));
    a.run_slash(SlashCommand::New).await.unwrap();
    assert_ne!(
        a.session_id(),
        first,
        "/new must switch to a new session id"
    );

    // Deleting the session you are in is refused.
    let err = a
        .run_slash(SlashCommand::Delete {
            target: a.session_id().to_string(),
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("cannot delete the session you are in"),
        "{err}"
    );

    // Deleting a saved session by prefix works and removes it from the listing.
    let out = a
        .run_slash(SlashCommand::Delete {
            target: first.to_string()[..8].to_string(),
        })
        .await
        .unwrap();
    assert!(out.contains("deleted session"), "{out}");
    let listing = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert!(
        !listing.contains(&first.to_string()[..8]),
        "deleted session must not be listed: {listing}"
    );

    // Deleting it again reports it as unknown rather than silently succeeding.
    let err = a
        .run_slash(SlashCommand::Delete {
            target: first.to_string()[..8].to_string(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("unknown or ambiguous"), "{err}");
}

/// `/resume ` with a blank argument parsed to `Some("")`, and every id starts with "",
/// so it always matched the session already open — reporting "resumed" while ignoring
/// what the user meant to select. A blank selector is a usage error.
#[tokio::test]
async fn resume_refuses_a_blank_selector_instead_of_matching_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let mut a = agent(tmp.path(), &ws);

    // A real second session exists, so "no sessions yet" cannot mask the bug.
    let other = {
        let mut b = agent(tmp.path(), &ws);
        b.run_turn("second session").await;
        b.session_id()
    };
    a.run_turn("first session").await;
    let mine = a.session_id();

    let out = a
        .run_slash(SlashCommand::Resume {
            target: Some(String::new()),
        })
        .await;
    let err = out.expect_err("a blank selector must be refused");
    assert!(
        err.to_lowercase().contains("empty") || err.contains("usage"),
        "the error must name the problem, got: {err}"
    );
    // It must not have switched to the other session.
    assert_eq!(
        a.session_id(),
        mine,
        "a blank selector must not resume {other}"
    );
}

/// Titles stored before auto-titling flattened whitespace can still contain
/// newlines. The `/resume` listing prints them, so it must flatten rather than
/// let one title forge extra entries.
#[tokio::test]
async fn resume_listing_flattens_a_stored_multiline_title() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let mut a = agent(tmp.path(), &ws);

    // Materialize the session, then write a legacy-style title straight into the
    // journal, bypassing the parser.
    a.run_turn("hello").await;
    let paths = test_paths(tmp.path());
    let store = loon_core::open_store(&paths).unwrap();
    let journal = loon_events::Journal::open_default(&store, &paths.sessions_dir(), a.session_id())
        .unwrap()
        .0;
    journal.rename_session("legacy\nsecond line").unwrap();

    let out = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert!(
        !out.contains("second line\n"),
        "a stored newline must not become its own line, got: {out:?}"
    );
    assert!(
        out.contains("legacy second line"),
        "the title should appear flattened, got: {out:?}"
    );
}

/// `Agent::session_cwd` is what makes `resume` root its tools in the session's own
/// directory. It must report the recorded workspace, and must not invent one for an
/// unknown session or for a session that recorded no directory.
#[tokio::test]
async fn session_cwd_reports_the_recorded_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let mut a = agent(tmp.path(), &ws);
    let id = a.session_id();

    // A pending session has no journal yet, so there is nothing to report.
    assert_eq!(
        Agent::session_cwd(&test_paths(tmp.path()), id),
        None,
        "a session with no journal must not claim a workspace"
    );

    a.run_turn("hello").await;

    assert_eq!(
        Agent::session_cwd(&test_paths(tmp.path()), id),
        Some(ws.clone()),
        "the recorded workspace must come back verbatim"
    );
    assert_eq!(
        Agent::session_cwd(
            &test_paths(tmp.path()),
            "01a164c1-0000-7000-8000-000000000000".parse().unwrap()
        ),
        None,
        "an unknown session must not report a workspace"
    );
}

/// Session ids are UUIDv7, so the leading characters are a timestamp: two sessions
/// started in the same instant share their first 24 hex chars, and any two started
/// within ~65s share the first 8. The `/resume` listing used to print a fixed 8-char
/// slice, so it showed two DIFFERENT sessions under the SAME id — and typing that id
/// back hit the "you are already in this session" shortcut, reporting success while
/// silently staying put. A displayed id must therefore be both unique and usable.
#[tokio::test]
async fn every_listed_session_id_is_unique_and_selects_that_session() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![
            MockScript::text("answer alpha"),
            MockScript::text("answer beta"),
            MockScript::text("answer gamma"),
        ]),
    ));
    let mut a = Agent::new_session(
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
        loon_tools::ToolRegistry::standard(),
        ws,
        true,
    )
    .unwrap();

    // Three sessions created back to back: exactly the shape that collides.
    for (prompt, title) in [
        ("session alpha about parsing", "alpha"),
        ("session beta about lexing", "beta"),
        ("session gamma about parsing", "gamma"),
    ] {
        let _ = a.run_turn(prompt).await;
        assert!(
            a.run_slash(SlashCommand::New).await.is_ok(),
            "each /new must succeed"
        );
        let _ = title;
    }

    let listing = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();

    // Collect the id shown on each row: "  N: <id> · <title> · ...".
    let shown: Vec<String> = listing
        .lines()
        .filter_map(|line| line.split(": ").nth(1))
        .filter_map(|rest| rest.split(" · ").next())
        .map(str::to_string)
        .collect();
    assert_eq!(shown.len(), 3, "three sessions must be listed: {listing}");

    let unique: std::collections::HashSet<&String> = shown.iter().collect();
    assert_eq!(
        unique.len(),
        shown.len(),
        "each listed id must identify exactly one session, got {shown:?}\n{listing}"
    );

    // Every displayed id must actually select the session it labels.
    for id in &shown {
        let out = a
            .run_slash(SlashCommand::Resume {
                target: Some(id.clone()),
            })
            .await
            .unwrap_or_else(|e| panic!("the listed id {id} must be a usable selector: {e}"));
        assert!(
            out.starts_with(&format!("resumed session {id}")),
            "resuming by the displayed id {id} must land on THAT session, got: {out}"
        );
    }
}

/// The complement of the listing fix: a user may type a short prefix by hand. When
/// that prefix also matches a *different* saved session it is ambiguous, and it must
/// not be answered with "resumed the session you are already in" — that reports
/// success while ignoring the session the user asked to switch to.
#[tokio::test]
async fn a_prefix_shared_with_another_session_is_not_taken_as_the_current_one() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![
            MockScript::text("answer alpha"),
            MockScript::text("answer beta"),
        ]),
    ));
    let mut a = Agent::new_session(
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
        loon_tools::ToolRegistry::standard(),
        ws,
        true,
    )
    .unwrap();

    // Two sessions back to back: they share the leading 8 hex chars.
    let _ = a.run_turn("session alpha about parsing").await;
    let _ = a.run_slash(SlashCommand::New).await;
    let _ = a.run_turn("session beta about lexing").await;

    let current = a.session_id().to_string();
    let short: String = current.chars().take(8).collect();

    // That bare prefix matches the other session too, so it cannot identify this one.
    let result = a
        .run_slash(SlashCommand::Resume {
            target: Some(short.clone()),
        })
        .await;
    match result {
        Err(e) => assert!(
            e.contains("ambiguous") || e.contains("unknown"),
            "an ambiguous prefix must be refused by name, got: {e}"
        ),
        Ok(out) => panic!(
            "the bare prefix {short} also matches another session and must not be \
             accepted as the current one, got: {out}"
        ),
    }

    // The full id is unambiguous and must still resume the session we are in.
    let out = a
        .run_slash(SlashCommand::Resume {
            target: Some(current.clone()),
        })
        .await
        .expect("the full id must resume the current session");
    assert!(out.starts_with("resumed session"), "{out}");
}

/// `/export` with no path writes `loon-session-<id>.md` into the cwd and
/// `export_markdown` overwrites without asking. With a short id slice, two sessions
/// started within ~65s produced the SAME default filename, so the second export
/// silently destroyed the first. A filename has no width budget, so it uses the full id.
#[tokio::test]
async fn a_default_export_from_one_session_does_not_overwrite_another() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![
            MockScript::text("alpha answer"),
            MockScript::text("beta answer"),
        ]),
    ));
    let mut a = Agent::new_session(
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
        loon_tools::ToolRegistry::standard(),
        ws,
        true,
    )
    .unwrap();

    let _ = a.run_turn("session alpha about parsing").await;
    let first = a
        .run_slash(SlashCommand::Export { path: None })
        .await
        .unwrap();

    let _ = a.run_slash(SlashCommand::New).await;
    let _ = a.run_turn("session beta about lexing").await;
    let second = a
        .run_slash(SlashCommand::Export { path: None })
        .await
        .unwrap();

    assert_ne!(
        first, second,
        "two sessions must not export to the same default file"
    );
    // The earlier export must still hold the earlier session's content.
    let first_path = first.trim_start_matches("exported to ").trim();
    let kept = std::fs::read_to_string(first_path)
        .unwrap_or_else(|e| panic!("the first export must survive: {e}"));
    assert!(
        kept.contains("alpha"),
        "the first export was overwritten; it now holds: {kept:.200}"
    );

    // The default path is cwd-relative, so both files land beside the test binary's
    // working directory: remove them rather than littering the crate.
    for out in [&first, &second] {
        let _ = std::fs::remove_file(out.trim_start_matches("exported to ").trim());
    }
}

/// A `/new` session that has sent nothing yet lives only in memory, so it is absent
/// from the store listing the display prefix is computed against. It still shares the
/// leading hex chars with saved sessions created moments earlier. Resuming a LISTED
/// session by its displayed id must therefore reach that saved session, not be caught
/// by the "you are already in this session" shortcut on the pending one.
#[tokio::test]
async fn a_listed_id_still_resumes_the_saved_session_while_a_pending_one_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::text("alpha answer")]),
    ));
    let mut a = Agent::new_session(
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
        loon_tools::ToolRegistry::standard(),
        ws,
        true,
    )
    .unwrap();

    // One saved session.
    let _ = a.run_turn("session alpha about parsing").await;
    let saved = a.session_id();

    // A new session that has sent nothing: pending, so not in the listing.
    let _ = a.run_slash(SlashCommand::New).await;
    let pending = a.session_id();
    assert_ne!(pending, saved);

    let listing = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    let shown: Vec<String> = listing
        .lines()
        .skip(1)
        .filter_map(|l| l.split(": ").nth(1))
        .filter_map(|r| r.split(" · ").next())
        .map(str::to_string)
        .collect();
    assert_eq!(
        shown.len(),
        1,
        "only the saved session is listed: {listing}"
    );

    let out = a
        .run_slash(SlashCommand::Resume {
            target: Some(shown[0].clone()),
        })
        .await
        .expect("the listed id must resume the saved session");
    assert!(
        out.contains(&saved.to_string()),
        "must land on the saved session {saved}, got: {out}"
    );
    assert_eq!(
        a.session_id(),
        saved,
        "the displayed id must not be answered with the pending session"
    );
}

/// `/resume` must only offer sessions started in this workspace. Listing the shared
/// store meant a picker opened in one project showed sessions from unrelated
/// directories, and `Agent::resume` adopts the *stored* cwd as the workspace root —
/// so picking one silently moved the agent out of the tree the user was working in.
#[tokio::test]
async fn resume_lists_only_sessions_from_this_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let here = tmp.path().join("proj");
    let elsewhere = tmp.path().join("other-project");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();

    // One real session in another directory, and one here.
    let foreign = {
        let mut b = agent(tmp.path(), &elsewhere);
        b.run_turn("work in the other project").await;
        b.session_id()
    };
    let mut a = agent(tmp.path(), &here);
    a.run_turn("work in this project").await;
    let mine = a.session_id();
    a.run_slash(SlashCommand::New).await.unwrap();
    let current = a.session_id();
    assert_ne!(current, mine, "/new switches to a fresh session");

    let listing = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert!(
        listing.contains("work in this project"),
        "this directory's session must be listed: {listing}"
    );
    assert!(
        !listing.contains("work in the other project") && !listing.contains(&foreign.to_string()),
        "a session from another directory must not be offered: {listing}"
    );
    assert!(
        listing.contains("other directories"),
        "the hidden count must be stated instead of silently dropping rows: {listing}"
    );

    // Selecting the foreign session by id is refused by name, not as a typo, and the
    // agent stays in this workspace.
    let err = a
        .run_slash(SlashCommand::Resume {
            target: Some(foreign.to_string()),
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("another directory") && err.contains(&elsewhere.display().to_string()),
        "the refusal must name the owning directory, got: {err}"
    );
    assert_eq!(
        a.session_id(),
        current,
        "a refused cross-directory resume must not switch sessions"
    );
    assert_eq!(
        a.ctx.ws.root(),
        here.canonicalize().unwrap(),
        "the workspace root must not move to the other project"
    );

    // The session here is still resumable.
    let out = a
        .run_slash(SlashCommand::Resume {
            target: Some(mine.to_string()),
        })
        .await
        .unwrap();
    assert!(out.contains(&mine.to_string()), "{out}");
}

/// An index is only meaningful against the listing the user actually saw, so
/// `/resume 1` must pick this directory's session even when another directory's
/// session is more recent and would sort first in the global list.
#[tokio::test]
async fn resume_numbers_follow_the_scoped_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let here = tmp.path().join("proj");
    let elsewhere = tmp.path().join("other-project");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();

    let mut a = agent(tmp.path(), &here);
    a.run_turn("mine").await;
    let mine = a.session_id();
    a.run_slash(SlashCommand::New).await.unwrap();
    // Created afterwards, so it is the most recent session globally.
    let foreign = {
        let mut b = agent(tmp.path(), &elsewhere);
        b.run_turn("theirs").await;
        b.session_id()
    };

    let out = a
        .run_slash(SlashCommand::Resume {
            target: Some("1".into()),
        })
        .await
        .unwrap();
    assert!(out.contains(&mine.to_string()), "{out}");
    assert_eq!(
        a.session_id(),
        mine,
        "index 1 must be the first row of the scoped listing, not {foreign}"
    );
}

/// With nothing here but sessions elsewhere, the message must say so instead of
/// claiming there are no sessions at all.
#[tokio::test]
async fn resume_says_when_sessions_exist_only_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let here = tmp.path().join("fresh");
    let elsewhere = tmp.path().join("other-project");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();

    {
        let mut b = agent(tmp.path(), &elsewhere);
        b.run_turn("someone else's work").await;
    }
    let mut a = agent(tmp.path(), &here);
    let out = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert_ne!(out.trim(), "no sessions yet", "that would be a lie: {out}");
    assert!(out.contains("other directories"), "{out}");
    assert!(out.contains(&here.display().to_string()), "{out}");
}

/// Sessions whose cwd differs only by a path *suffix* must not match: `/work/app`
/// is not `/work/app-old`. A plain string prefix check would mix them up.
#[tokio::test]
async fn resume_does_not_confuse_a_sibling_directory_with_a_prefix_name() {
    let tmp = tempfile::tempdir().unwrap();
    let here = tmp.path().join("app");
    let sibling = tmp.path().join("app-old");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();

    {
        let mut b = agent(tmp.path(), &sibling);
        b.run_turn("sibling session").await;
    }
    let mut a = agent(tmp.path(), &here);
    let listing = a
        .run_slash(SlashCommand::Resume { target: None })
        .await
        .unwrap();
    assert!(
        !listing.contains("sibling session"),
        "app-old is a different workspace from app: {listing}"
    );
}

/// `/new` and `/resume` build a fresh agent and hand-copy the host-owned fields. They
/// forgot the background hub, which owns the running children: after a switch, a child's
/// report landed in a hub nothing read, and `background_list` could no longer see the job
/// at all. The hub must survive a session switch; `replace_with` is the one place that
/// guarantees it.
#[tokio::test]
async fn switching_sessions_keeps_the_background_hub() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let mut a = agent(tmp.path(), &ws);

    // Identify the hub the running children were registered in.
    let hub = a.background.clone();
    assert!(
        hub.listing().is_empty(),
        "no children yet, but the handle exists"
    );

    // A switch to a fresh session, and back to a resumed one.
    a.run_slash(SlashCommand::New).await.unwrap();
    assert!(
        a.background.same_hub(&hub),
        "/new must carry the hub, not create a fresh one"
    );

    a.run_turn("a real session").await;
    let id = a.session_id();
    a.run_slash(SlashCommand::New).await.unwrap();
    a.run_slash(SlashCommand::Resume {
        target: Some(id.to_string()),
    })
    .await
    .unwrap();
    assert!(
        a.background.same_hub(&hub),
        "/resume must carry the hub, not create a fresh one"
    );
}

/// Drive one real `edit` through a turn so the journal holds a `FileMutated` with a
/// before-blob, then return the sequence number of that mutation.
async fn a_journaled_edit(
    a: &mut Agent,
    ws: &std::path::Path,
    rel: &str,
    original: &str,
    edited: &str,
) -> u64 {
    use loon_providers::{MockScript, MockStep, ProviderRegistry};
    let full = ws.join(rel);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(&full, original).unwrap();
    let hash = loon_workspace::sha256_hex(original.as_bytes());
    let registry = Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::new(vec![
            MockStep::tool_call(
                "call_edit",
                "edit",
                serde_json::json!({"path": rel, "old_string": original, "new_string": edited, "expect_sha256": hash}),
            ),
        ])]),
    ));
    a.registry = registry;
    let r = a.run_turn("edit it").await;
    assert!(
        matches!(r.status, loon_core::TurnStatus::Completed),
        "{:?}",
        r.status
    );
    // The newest FileMutated for this path carries the pre-edit snapshot.
    a.journal
        .events()
        .unwrap()
        .iter()
        .filter(|e| matches!(&e.kind, loon_events::model::EventKind::FileMutated { path, .. } if path.ends_with(rel)))
        .map(|e| e.seq)
        .next_back()
        .expect("the edit must be journaled with a snapshot")
}

/// `/shadow-restore` matched the journaled path by *suffix* but then rebuilt the write
/// target from the text the user typed, so `/shadow-restore N main.rs` (matching
/// `src/main.rs`) wrote `./main.rs` instead — the wrong file, silently. It also joined the
/// raw text onto the root, so an absolute path or `..` escaped the workspace and the write
/// was neither sandboxed nor journaled.
#[tokio::test]
async fn shadow_restore_writes_the_recorded_path_not_the_typed_suffix() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    let mut a = agent(tmp.path(), &ws);
    let original = "fn main() { old(); }\n";
    let seq = a_journaled_edit(
        &mut a,
        &ws,
        "src/main.rs",
        original,
        "fn main() { new(); }\n",
    )
    .await;

    // The user names the short suffix, as the /shadow listing shows.
    let out = a
        .run_slash(SlashCommand::ShadowRestore {
            seq,
            path: "main.rs".into(),
        })
        .await
        .expect("the suffix must resolve to the journaled path");
    assert!(out.contains("src/main.rs"), "{out}");
    assert_eq!(
        std::fs::read_to_string(ws.join("src/main.rs")).unwrap(),
        original,
        "the snapshotted file is the one restored"
    );
    assert!(
        !ws.join("main.rs").exists(),
        "the typed suffix must not create a second file at the root"
    );
}

/// A selector naming a file outside the workspace must not write anything: the lookup is
/// by journaled path, and the write goes through the sandbox, so nothing escapes.
#[tokio::test]
async fn shadow_restore_cannot_write_outside_the_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
    let mut a = agent(tmp.path(), &ws);
    let seq = a_journaled_edit(&mut a, &ws, "src/main.rs", "old\n", "new\n").await;

    let err = a
        .run_slash(SlashCommand::ShadowRestore {
            seq,
            path: "../../outside/secret.txt".into(),
        })
        .await
        .unwrap_err();
    assert!(
        !tmp.path().join("outside/secret.txt").exists(),
        "nothing may be written outside the workspace: {err}"
    );
    // The workspace file is untouched, too.
    assert_eq!(
        std::fs::read_to_string(ws.join("src/main.rs")).unwrap(),
        "new\n"
    );
}
