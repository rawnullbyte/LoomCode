//! Background subagents: `background_spawn` returns immediately, the parent can message,
//! list and kill the child, and the child's report reaches the parent — through
//! `background_list` for the model and through `AgentEvent::SubagentFinished` for the
//! user. Every test drives the real tools against the mock provider.

use loon_config::LoonPaths;
use loon_core::subagent::{self, BackgroundHub};
use loon_core::{Agent, AgentConfig, AgentEvent};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
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
        tool_output_bytes: 8192,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: None,
        ultra_rounds: 0,
        permissions: Default::default(),
        context_window: 128_000,
        extra_tools: Vec::new(),
    }
}

fn tool_ctx(ws: &std::path::Path) -> loon_tools::ToolCtx {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    loon_tools::ToolCtx {
        ws: Arc::new(loon_workspace::Workspace::new(ws, vec![]).unwrap()),
        output_limit: 8192,
        default_timeout: std::time::Duration::from_secs(30),
        child_env: Default::default(),
        cancel: rx,
        blob_sink: None,
    }
}

/// The entry bound must not turn into a silent drop. When the hub declines to announce
/// every finished report individually, the parent has to be told that outcomes exist
/// which this step does not name — otherwise a burst of children reads exactly like
/// children that finished and produced nothing, which is the ambiguity this whole
/// retention mechanism exists to avoid.
#[tokio::test]
async fn a_report_dropped_by_the_entry_bound_is_announced_as_dropped() {
    use loon_core::subagent::JobStatus;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();
    // The hub announces 32 reports per step; forty small children overflow that.
    for i in 0..40u64 {
        let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![MockScript::text(format!("result {i}"))],
        )));
        let child_tools = subagent::default_tools_with_hub(
            &paths,
            &ws,
            &cfg(),
            child_registry,
            None,
            hub.clone(),
        );
        child_tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&ws))
            .await
            .expect("spawn succeeds");
    }
    for _ in 0..400 {
        if hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let mut agent = Agent::new_session(&paths, cfg(), parent_registry, parent_tools, ws, true)
        .expect("parent session");
    agent.background = hub.clone();
    let _ = agent.run_turn("carry on").await;

    let noted = agent
        .history
        .iter()
        .filter(|m| m.text.contains("were not announced individually"))
        .count();
    assert_eq!(
        noted, 1,
        "the parent must be told once that reports were dropped instead of announced"
    );
    // Nothing may be lost: every child is still listed with its status.
    let listed = hub.listing();
    assert_eq!(listed.len(), 40, "every child must stay listed");
    assert!(
        listed
            .iter()
            .all(|(_, _, s, _, _)| *s == JobStatus::Completed),
        "every child must be recorded as completed"
    );
}

/// A hub wired to a workspace with the mock provider, plus the tools it owns.
struct Fixture {
    /// Kept alive so the temp workspace outlives the tools.
    _tmp: tempfile::TempDir,
    ws: std::path::PathBuf,
    hub: BackgroundHub,
    tools: loon_tools::ToolRegistry,
}

fn fixture(scripts: Vec<MockScript>) -> Fixture {
    fixture_with_mock(scripts).0
}

/// Same as [`fixture`], but also hands back the mock provider so a test can assert what
/// the child was actually SENT (the mock records every request).
fn fixture_with_mock(scripts: Vec<MockScript>) -> (Fixture, Arc<MockProvider>) {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let mock = MockProvider::with_scripts(scripts);
    let registry = Arc::new(ProviderRegistry::with_mock(mock.clone()));
    let hub = BackgroundHub::new();
    let tools = subagent::default_tools_with_hub(&paths, &ws, &cfg(), registry, None, hub.clone());
    (
        Fixture {
            _tmp: tmp,
            ws,
            hub,
            tools,
        },
        mock,
    )
}

impl Fixture {
    fn ctx(&self) -> loon_tools::ToolCtx {
        tool_ctx(&self.ws)
    }

    async fn call(&self, name: &str, args: serde_json::Value) -> String {
        let tool = self.tools.get(name).expect("tool is registered");
        let out = tool
            .run(args, &self.ctx())
            .await
            .unwrap_or_else(|e| panic!("{name} failed: {e}"));
        out.text
    }

    async fn try_call(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<String, loon_tools::ToolError> {
        self.tools
            .get(name)
            .expect("tool is registered")
            .run(args, &self.ctx())
            .await
            .map(|o| o.text)
    }
}

#[tokio::test]
async fn the_parent_gets_a_handle_without_waiting_for_the_child() {
    // The child would answer with this text; the spawn call must return BEFORE that
    // answer exists, carrying only the id.
    let f = fixture(vec![MockScript::text("child report body")]);
    let out = f
        .call("background_spawn", json!({"task": "do the thing"}))
        .await;
    assert!(
        out.contains("background agent #1"),
        "spawn must name the handle: {out}"
    );
    assert!(
        !out.contains("child report body"),
        "spawn must not block until the child answered: {out}"
    );

    // Listing immediately shows it as running (the mock resolves on its own schedule,
    // so either running or completed is legitimate here — but it must be listed).
    let listed = f.call("background_list", json!({})).await;
    assert!(listed.contains("#1"), "the child must be listed: {listed}");
}

#[tokio::test]
async fn a_finished_child_reports_its_status_and_its_work() {
    let f = fixture(vec![MockScript::text("read 3 files; nothing to change")]);
    f.call("background_spawn", json!({"task": "survey the repo"}))
        .await;

    // Wait for the child's turn to land, then read the report the parent collects.
    let mut listed = String::new();
    for _ in 0..200 {
        listed = f.call("background_list", json!({})).await;
        if listed.contains("completed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        listed.contains("completed"),
        "the child must report a status: {listed}"
    );
    assert!(
        listed.contains("read 3 files; nothing to change"),
        "the report must carry the work the child did: {listed}"
    );
}

#[tokio::test]
async fn the_report_reaches_the_parent_as_an_event() {
    let f = fixture(vec![MockScript::text("child says: 42")]);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    f.hub.set_sink(tx);

    f.call("background_spawn", json!({"task": "compute"})).await;

    let finished = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::SubagentFinished {
                id, status, text, ..
            } = ev
            {
                return (id, status, text);
            }
        }
        panic!("the sink closed before the child reported");
    })
    .await
    .expect("a finished child must announce itself on the parent's sink");

    assert_eq!(finished.0, 1, "the event must carry the handle id");
    assert_eq!(finished.1, "completed", "status: {}", finished.1);
    assert!(
        finished.2.contains("child says: 42"),
        "the event must carry the report: {}",
        finished.2
    );
}

#[tokio::test]
async fn starting_a_child_announces_itself_before_it_finishes() {
    // The user must see a spawned agent immediately, not only once it reports.
    let f = fixture(vec![MockScript::text("late report")]);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    f.hub.set_sink(tx);
    f.call("background_spawn", json!({"task": "work"})).await;

    let started = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::SubagentUpdate { id, status, .. } = ev {
                return (id, status);
            }
        }
        panic!("the sink closed without a start notice");
    })
    .await
    .expect("a spawned child must announce that it started");
    assert_eq!(started.0, 1);
    assert_eq!(started.1, "running");
}

#[tokio::test]
async fn a_running_child_can_be_messaged_and_then_reports() {
    // Two-step child: it asks for a tool call, so it is still running when we message
    // it; the steering message is applied at the next step boundary.
    let f = fixture(vec![
        MockScript::new(vec![MockStep::tool_call(
            "call_1",
            "read",
            json!({"path": "seed.txt"}),
        )]),
        MockScript::text("second step done"),
    ]);
    std::fs::write(f.ws.join("seed.txt"), "seed").unwrap();
    f.call("background_spawn", json!({"task": "two step task"}))
        .await;

    // Accept either outcome of the race: queued while running, or an honest refusal
    // once it already finished. What must never happen is a silent success.
    let message = f
        .try_call(
            "background_message",
            json!({"id": 1, "message": "also check the README"}),
        )
        .await;
    match message {
        Ok(note) => assert!(
            note.contains("queued for background agent #1"),
            "a queued message must say so: {note}"
        ),
        Err(e) => assert!(
            e.to_string().contains("already"),
            "the only acceptable refusal is 'already finished': {e}"
        ),
    }

    let mut listed = String::new();
    for _ in 0..200 {
        listed = f.call("background_list", json!({})).await;
        if listed.contains("completed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(listed.contains("completed"), "child must finish: {listed}");
}

#[tokio::test]
async fn killing_a_running_child_stops_it_and_is_reported() {
    // A long child: it keeps asking for tools, so it is definitely still running.
    let f = fixture(vec![MockScript::new(vec![MockStep::tool_call(
        "call_1",
        "read",
        json!({"path": "seed.txt"}),
    )])]);
    std::fs::write(f.ws.join("seed.txt"), "seed").unwrap();
    f.call("background_spawn", json!({"task": "never ending"}))
        .await;

    let killed = f.call("background_kill", json!({"id": 1})).await;
    assert!(killed.contains("killed"), "kill must confirm: {killed}");

    let listed = f.call("background_list", json!({})).await;
    assert!(
        listed.contains("cancelled") || listed.contains("killed"),
        "a killed child must not be reported as running: {listed}"
    );
}

#[tokio::test]
async fn managing_an_unknown_agent_is_an_error_not_a_silent_no_op() {
    let f = fixture(vec![]);
    let msg = f
        .try_call("background_message", json!({"id": 99, "message": "hi"}))
        .await;
    assert!(msg.is_err(), "messaging an unknown id must fail");
    assert!(
        msg.unwrap_err()
            .to_string()
            .contains("no background agent #99"),
        "the error must name the missing handle"
    );
    let kill = f.try_call("background_kill", json!({"id": 99})).await;
    assert!(kill.is_err(), "killing an unknown id must fail");
}

#[tokio::test]
async fn the_parent_turn_absorbs_a_finished_report_into_its_context() {
    // Reports are drained at step boundaries, so a child that finished before the
    // parent's turn starts must be in the very first request's context. The child is
    // driven to completion first: sharing one FIFO mock between a concurrent child and
    // parent would make script assignment racy and the assertion meaningless.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();

    // The child's own script.
    let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("the parser has an off-by-one")],
    )));
    let child_tools =
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), child_registry, None, hub.clone());
    let ctx = tool_ctx(&ws);
    child_tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "investigate the parser"}), &ctx)
        .await
        .expect("spawn succeeds");
    for _ in 0..200 {
        if hub
            .listing()
            .iter()
            .any(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // The parent now runs a turn with a registry of its own.
    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        parent_registry,
        parent_tools,
        ws.clone(),
        true,
    )
    .unwrap();
    agent.background = hub;

    let result = agent.run_turn("carry on with the plan").await;
    assert!(matches!(result.status, loon_core::TurnStatus::Completed));

    let joined = agent
        .history
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n---\n");
    assert!(
        joined.contains("background #1") && joined.contains("the parser has an off-by-one"),
        "the subagent's report must reach the parent's context; history was:\n{joined}"
    );
}

#[tokio::test]
async fn messaging_a_child_is_visible_in_the_transcript() {
    // The steering message comes from the parent model, so the user would otherwise see
    // a running child change course with no explanation. The notice must name the handle.
    let f = fixture(vec![MockScript::new(vec![MockStep::tool_call(
        "call_1",
        "read",
        json!({"path": "seed.txt"}),
    )])]);
    std::fs::write(f.ws.join("seed.txt"), "seed").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    f.hub.set_sink(tx);

    f.call("background_spawn", json!({"task": "long task"}))
        .await;
    f.call(
        "background_message",
        json!({"id": 1, "message": "change of plan"}),
    )
    .await;

    let notice = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::SubagentUpdate { id, note, .. } = ev {
                if note.contains("message") {
                    return (id, note);
                }
            }
        }
        panic!("the sink closed without a message notice");
    })
    .await
    .expect("messaging a running child must be visible to the user");

    assert_eq!(notice.0, 1, "the notice must carry the handle id");
    assert!(
        notice.1.contains("message"),
        "the notice must say a message was delivered: {}",
        notice.1
    );
}

#[tokio::test]
async fn killing_a_child_is_visible_before_it_unwinds() {
    // Cancellation is cooperative and only takes effect at the child's next step
    // boundary, so the user must not have to wait for that to see the kill land.
    let f = fixture(vec![MockScript::new(vec![MockStep::tool_call(
        "call_1",
        "read",
        json!({"path": "seed.txt"}),
    )])]);
    std::fs::write(f.ws.join("seed.txt"), "seed").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    f.hub.set_sink(tx);

    f.call("background_spawn", json!({"task": "never ending"}))
        .await;
    f.call("background_kill", json!({"id": 1})).await;

    let notice = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::SubagentUpdate { id, status, .. } = ev {
                if status == "cancelled" {
                    return id;
                }
            }
        }
        panic!("the sink closed without a cancellation notice");
    })
    .await
    .expect("killing a child must be visible immediately");

    assert_eq!(notice, 1, "the notice must carry the handle id");
}

#[tokio::test]
async fn a_steering_message_actually_reaches_the_childs_next_step() {
    // The older message test only proves the parent QUEUED a message; it would still
    // pass if the hub's steering sender were wired to nothing, leaving the child
    // permanently deaf. This one reads what the child was actually sent: the mock
    // records every request, so the steering text must appear in a LATER request —
    // i.e. it was absorbed at a step boundary and shown to the child's model.
    let (f, mock) = fixture_with_mock(vec![
        // Step 1 sleeps before answering, which gives the parent a deterministic
        // window to message a child that is genuinely still running.
        MockScript::new(vec![
            MockStep::Delay(std::time::Duration::from_millis(300)),
            MockStep::tool_call("call_1", "read", json!({"path": "seed.txt"})),
        ]),
        MockScript::text("steered and finished"),
    ]);
    std::fs::write(f.ws.join("seed.txt"), "seed").unwrap();

    f.call("background_spawn", json!({"task": "long task"}))
        .await;
    f.call(
        "background_message",
        json!({"id": 1, "message": "also check the README"}),
    )
    .await;

    // Wait for the child to finish so every request it made has been recorded.
    let mut listed = String::new();
    for _ in 0..400 {
        listed = f.call("background_list", json!({})).await;
        if listed.contains("completed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(listed.contains("completed"), "child must finish: {listed}");

    let requests = mock.requests();
    assert!(
        requests.len() >= 2,
        "the child must have taken at least two steps, got {}",
        requests.len()
    );
    // The drain sits at the top of the agent loop, before the request is built —
    // so depending on scheduling the steering may land in the first request (it
    // arrived before the child started) or a later one (it arrived mid-turn).
    // Either way it must appear: if the hub's steering sender were wired to
    // nothing, NO request would carry it and the child would be deaf.
    assert!(
        requests.iter().any(|r| {
            r.messages
                .iter()
                .any(|m| m.text.contains("[steering]") && m.text.contains("also check the README"))
        }),
        "the steering message must reach the child's model; the hub's sender would \
         otherwise be wired to nothing (got {} requests)",
        requests.len()
    );
}

#[tokio::test]
async fn a_huge_child_report_is_bounded_before_it_enters_the_parent_context() {
    // `tool_output_bytes` is documented as "tool output above this size goes to a blob,
    // excerpt shown" (config/schema.rs). `delegate` and `background_list` both honour it
    // via `ToolCtx::bound`, but the auto-inject drain path put the child's report into the
    // parent's context verbatim — so one chatty subagent could blow the context window in
    // a single step, and the journal kept a second copy of the whole thing.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();

    // 50 KB, ~6x the 8192-byte limit the fixture configures.
    let huge = "Y".repeat(50_000);
    let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text(huge)],
    )));
    let child_tools =
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), child_registry, None, hub.clone());
    let ctx = tool_ctx(&ws);
    child_tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "write me an essay"}), &ctx)
        .await
        .expect("spawn succeeds");
    for _ in 0..400 {
        if hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        parent_registry,
        parent_tools,
        ws.clone(),
        true,
    )
    .unwrap();
    agent.background = hub;
    let _ = agent.run_turn("carry on").await;

    let injected: Vec<&str> = agent
        .history
        .iter()
        .filter(|m| m.text.contains("[background #"))
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(injected.len(), 1, "the report must still be delivered");
    let note = injected[0];
    assert!(
        note.contains("background #1"),
        "the report must still identify its child: {}",
        &note[..note.len().min(120)]
    );
    assert!(
        note.len() <= cfg().tool_output_bytes + 256,
        "the injected report must be bounded like every other tool output \
         (limit {}, got {} bytes)",
        cfg().tool_output_bytes,
        note.len()
    );
    assert!(
        note.contains("truncated") && note.contains("blob"),
        "a clipped report must say so and point at the full text: {}",
        &note[..note.len().min(300)]
    );
}

#[tokio::test]
async fn a_report_cut_at_a_multibyte_character_stays_valid_utf8() {
    // `bound_report` cuts at a byte limit; if the limit lands inside a multi-byte
    // character the excerpt must retreat to the boundary, not slice mid-character.
    // The report is 8200 CJK characters (24600 bytes against the 8192-byte limit),
    // so the cut is guaranteed to land inside a 3-byte character.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();

    let cjk = "日".repeat(8200);
    let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text(cjk)],
    )));
    let child_tools =
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), child_registry, None, hub.clone());
    let ctx = tool_ctx(&ws);
    child_tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "cjk report"}), &ctx)
        .await
        .expect("spawn succeeds");
    for _ in 0..400 {
        if hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        parent_registry,
        parent_tools,
        ws.clone(),
        true,
    )
    .unwrap();
    agent.background = hub;
    let result = agent.run_turn("carry on").await;
    assert!(matches!(result.status, loon_core::TurnStatus::Completed));

    let injected: Vec<&str> = agent
        .history
        .iter()
        .filter(|m| m.text.contains("[background #"))
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(injected.len(), 1, "the report must still be delivered");
    // Would panic or corrupt if the cut split a character: the note is a String and
    // the assertion below reads the raw bytes back.
    let bytes = injected[0].as_bytes();
    assert!(
        std::str::from_utf8(bytes).is_ok() && bytes.len() <= cfg().tool_output_bytes + 256,
        "cut must sit on a char boundary within the limit (got {} bytes)",
        bytes.len()
    );
}

/// The hub keeps every finished child's report so `background_list` can show it. That
/// retention must be bounded: a long session that spawns many children would otherwise
/// hold every report for the hub's whole lifetime. The bound is on TOTAL retained report
/// bytes, not on the number of jobs — jobs themselves stay listed (their status is the
/// record that the work happened) and only the oldest finished reports are released.
#[tokio::test]
async fn finished_reports_do_not_accumulate_without_bound() {
    let fx = fixture(vec![]);
    // Each child reports ~50 KB; ten of them is ~500 KB, comfortably over the budget.
    let big = "x".repeat(50_000);
    for i in 0..10 {
        let mock = MockProvider::with_scripts(vec![MockScript::text(format!("{big} child {i}"))]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let paths = test_paths(fx._tmp.path());
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
            .await
            .expect("spawn succeeds");
    }

    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let jobs = fx.hub.listing();
    assert_eq!(
        jobs.len(),
        10,
        "every job must stay listed, only its report is released"
    );
    assert!(
        jobs.iter()
            .all(|(_, _, s, _, _)| *s == subagent::JobStatus::Completed),
        "releasing a report must not disturb the job's status"
    );

    let retained: usize = jobs
        .iter()
        .filter_map(|(_, _, _, r, _)| r.as_ref().map(|r| r.len()))
        .sum();
    assert!(
        retained <= 64 * 1024,
        "retained report bytes must stay bounded (got {retained} bytes across {} jobs)",
        jobs.len()
    );

    // The newest result is the one the parent is most likely to ask for, so it must
    // survive intact — releasing must evict oldest-first.
    let newest = jobs.last().expect("at least one job");
    let report = newest
        .3
        .as_ref()
        .expect("the most recent report must still be readable");
    assert!(
        report.contains("child 9"),
        "the newest child's report must not be the one evicted"
    );
}

/// The retention bound is a promise about what the HUB holds, and the hub keeps two
/// copies of every report: `job.report` for `background_list` and `pending` for the
/// parent model's next step boundary. Bounding only the first one under-reports the
/// real retained memory by a factor of the queue length, so a session that spawns
/// many children and takes no step in between still grows without limit.
#[tokio::test]
async fn the_retention_bound_covers_the_undelivered_queue_too() {
    let fx = fixture(vec![]);
    // Ten ~50 KB reports against a 64 KB budget. `finished_reports_do_not_accumulate`
    // pins the `job.report` half; this pins the whole hub.
    let big = "x".repeat(50_000);
    for i in 0..10 {
        let mock = MockProvider::with_scripts(vec![MockScript::text(format!("{big} child {i}"))]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let paths = test_paths(fx._tmp.path());
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
            .await
            .expect("spawn succeeds");
    }

    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // No step boundary was crossed, so every report is still queued for the parent.
    let listed: usize = fx
        .hub
        .listing()
        .iter()
        .filter_map(|(_, _, _, r, _)| r.as_ref().map(|r| r.len()))
        .sum();
    // One drain, held: calling it twice would measure an emptied queue the second time.
    let (delivered, _dropped) = fx.hub.drain_reports();
    let queued: usize = delivered.iter().map(|(_, _, _, t)| t.len()).sum();
    // The budget counts each surviving report once; the hub may hold two copies of it
    // (job.report for `background_list`, and the queue entry for the parent's next step
    // boundary), so peak retention is bounded by twice the budget. What must NOT happen
    // is the queue growing independently: before this was fixed the two copies were
    // bounded separately and the queue was not bounded at all, so ten 50 KB children
    // left 550 088 bytes resident with no step in between.
    let retained = listed + queued;
    assert!(
        retained <= 2 * 64 * 1024,
        "the hub must not retain more than its documented budget across BOTH copies \
         (peak bound 131072, listed {listed} + queued {queued} = {retained} bytes)"
    );
    // The two surfaces must agree: a report released from `job.report` must also be
    // released from the queue, otherwise the queue keeps the memory the budget exists
    // to release.
    assert_eq!(
        listed, queued,
        "every surviving report must be present in both copies or neither \
         (listed {listed}, queued {queued})"
    );
    // And the newest must survive intact in the queue too — it is the report the parent
    // is about to be handed.
    let (_, _, _, last_text) = delivered.last().expect("the newest report is queued");
    assert!(
        last_text.contains("child 9"),
        "the newest report must not be released before delivery"
    );
}

/// The retention bound has to hold for *any* report size, not just the one the other
/// tests happen to use. `trim_reports` counts each surviving report once (so a report is
/// not evicted the instant it arrives) but has to account for both copies when it frees
/// one. Counting once and freeing twice makes the loop exit early, so the amount that
/// actually stays resident can drift above the documented peak.
#[tokio::test]
async fn the_retention_bound_holds_for_every_report_size() {
    // 20 KB is deliberately below half the 64 KB budget: a size where counting each
    // report once and freeing two copies per eviction disagree most. That disagreement
    // makes eviction run past the budget, so a report that WOULD have fitted gets
    // released — the bug is over-release, not under-release.
    for size in [1_000usize, 5_000, 20_000, 21_900, 32_000, 50_000] {
        let fx = fixture(vec![]);
        let big = "x".repeat(size);
        for i in 0..12 {
            let mock =
                MockProvider::with_scripts(vec![MockScript::text(format!("{big} child {i}"))]);
            let registry = Arc::new(ProviderRegistry::with_mock(mock));
            let paths = test_paths(fx._tmp.path());
            let tools = subagent::default_tools_with_hub(
                &paths,
                &fx.ws,
                &cfg(),
                registry,
                None,
                fx.hub.clone(),
            );
            tools
                .get("background_spawn")
                .unwrap()
                .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
                .await
                .expect("spawn succeeds");
        }
        for _ in 0..400 {
            if fx
                .hub
                .listing()
                .iter()
                .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        let listed: usize = fx
            .hub
            .listing()
            .iter()
            .filter_map(|(_, _, _, r, _)| r.as_ref().map(|r| r.len()))
            .sum();
        let queued: usize = fx
            .hub
            .drain_reports()
            .0
            .iter()
            .map(|(_, _, _, t)| t.len())
            .sum();
        let retained = listed + queued;
        assert!(
            retained <= 2 * 64 * 1024,
            "reports of {size} bytes left {retained} bytes resident \
             (listed {listed} + queued {queued}), above the documented peak of 131072"
        );
    }
}

/// Eviction must stop as soon as the surviving reports fit the budget. Counting each
/// report once but subtracting both copies per eviction makes `trim_reports` believe it
/// has freed twice as much as it did, so it keeps releasing reports that would have
/// fitted — the newest-first scan turns into a much earlier cutoff and reports that the
/// budget could hold are lost.
#[tokio::test]
async fn eviction_stops_once_the_survivors_fit_the_budget() {
    let fx = fixture(vec![]);
    // Six 20 KB reports = 120 KB, which is under the 2x peak but over the 64 KB budget,
    // so eviction must run — and must stop with the newest reports intact.
    let big = "x".repeat(20_000);
    for i in 0..6 {
        let mock = MockProvider::with_scripts(vec![MockScript::text(format!("{big} child {i}"))]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let paths = test_paths(fx._tmp.path());
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
            .await
            .expect("spawn succeeds");
    }
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let survivors: Vec<String> = fx
        .hub
        .listing()
        .iter()
        .filter_map(|(_, _, _, r, _)| r.clone())
        .collect();
    let kept = survivors.len();
    let retained: usize = survivors.iter().map(|r| r.len()).sum();
    // The budget is a floor on what must survive: whatever fits must not be released.
    assert!(
        retained >= 64 * 1024 - 20_100,
        "eviction released reports that fitted the budget: only {kept} of 6 survived \
         ({retained} bytes retained, budget 65536)"
    );
}

/// The queue entry for a released report is still injected into the parent's context at
/// the next step boundary. Injecting it as a bare header would read exactly like a child
/// that finished and returned nothing — the same ambiguity `background_list` already
/// avoids — so the injection has to say the text was released.
#[tokio::test]
async fn an_injected_report_says_so_when_its_text_was_released() {
    let fx = fixture(vec![]);
    let big = "x".repeat(50_000);
    let paths = test_paths(fx._tmp.path());
    // Ten ~50 KB reports against a 64 KB budget: the oldest nine are released, so the
    // parent is handed nine emptied queue entries plus one intact report.
    for i in 0..10 {
        let mock = MockProvider::with_scripts(vec![MockScript::text(format!("{big} child {i}"))]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
            .await
            .expect("spawn succeeds");
    }
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &fx.ws,
        &cfg(),
        parent_registry.clone(),
        None,
        fx.hub.clone(),
    );
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        parent_registry,
        parent_tools,
        fx.ws.clone(),
        true,
    )
    .unwrap();
    agent.background = fx.hub.clone();
    let _ = agent.run_turn("carry on").await;

    let injected: Vec<&str> = agent
        .history
        .iter()
        .filter(|m| m.text.contains("[background #"))
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(
        injected.len(),
        10,
        "every finished child must still be announced to the parent"
    );
    // Nine of the ten reports were released by the retention bound, so nine notes must
    // explain that instead of arriving as a bare header.
    let released: Vec<&&str> = injected.iter().filter(|n| n.contains("released")).collect();
    assert_eq!(
        released.len(),
        9,
        "the fixture must exercise released entries (got {})",
        released.len()
    );
    for note in injected.iter().filter(|n| n.contains("released")) {
        assert!(
            note.lines().count() >= 2,
            "a released report must carry an explanation line, not just a header: {note:?}"
        );
    }
    // Nothing may be handed over as a bare header with no explanation.
    for note in injected.iter() {
        assert!(
            note.lines().count() >= 2,
            "every injected report must say something after its header: {note:?}"
        );
    }
    // The newest report is the one that survived the budget, so it must arrive as a
    // real (bounded) excerpt with a blob pointer rather than as a released stub.
    assert!(
        injected.iter().any(|n| n.contains("stored as blob")),
        "the newest report must still be delivered with its full text kept as a blob"
    );
}

/// `background_list` must never leave a finished job looking like it has no result. A
/// report released by the retention bound is *not* the same thing as a child that is
/// still working, and the model reading this tool has to be able to tell them apart —
/// otherwise it waits on a job that will never produce anything, or concludes the child
/// returned nothing.
#[tokio::test]
async fn a_released_report_is_reported_as_released_not_as_silence() {
    let fx = fixture(vec![]);
    let big = "y".repeat(50_000);
    for i in 0..10 {
        let mock = MockProvider::with_scripts(vec![MockScript::text(format!("{big} child {i}"))]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let paths = test_paths(fx._tmp.path());
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
            .await
            .expect("spawn succeeds");
    }
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let out = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({}), &tool_ctx(&fx.ws))
        .await
        .expect("list succeeds");
    let text = out.text;

    // Job #1 is the oldest, so its report is the one the bound released.
    let first = text
        .lines()
        .position(|l| l.starts_with("#1 "))
        .expect("job 1 must still be listed");
    let after: String = text
        .lines()
        .skip(first)
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        after.contains("released") || after.contains("truncated") || after.contains("blob"),
        "a completed job whose report was released must say so, not render as an empty \
         body indistinguishable from a child that returned nothing. Got:\n{after}"
    );
}

/// The steering queue has a fixed capacity. Past it, `background_message` must report a
/// refusal the parent can act on — never accept a message and then lose it. This pins
/// the contract at the boundary: every call that returned Ok must be visible to the
/// child, so a silent drop (accepted, then discarded) fails here.
#[tokio::test]
async fn steering_past_capacity_is_refused_loudly_never_silently_dropped() {
    // A long delay parks the child before its first step boundary, so nothing drains
    // the queue while we push past capacity.
    let (f, mock) = fixture_with_mock(vec![MockScript::new(vec![
        MockStep::Delay(std::time::Duration::from_millis(1200)),
        MockStep::Text("finished after the delay".into()),
    ])]);
    f.call("background_spawn", json!({"task": "slow task"}))
        .await;

    let mut accepted: Vec<String> = Vec::new();
    let mut refused = 0u32;
    for i in 0..80 {
        let text = format!("steer-{i}");
        match f
            .try_call("background_message", json!({"id": 1, "message": text}))
            .await
        {
            Ok(_) => accepted.push(text),
            Err(_) => refused += 1,
        }
    }
    assert!(
        refused > 0,
        "80 messages against a bounded channel must produce at least one refusal; \
         otherwise the queue is unbounded or messages were silently dropped"
    );

    // Let the child run to completion, then check what it was actually sent.
    for _ in 0..400 {
        if f.hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let sent: String = mock
        .requests()
        .iter()
        .flat_map(|r| r.messages.iter())
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");

    let missing: Vec<&String> = accepted
        .iter()
        .filter(|t| !sent.contains(t.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "{} accepted message(s) never reached the child, so they were silently dropped: {:?}",
        missing.len(),
        missing
    );
}

/// `background_list` output is bounded, and the bound truncates the END of the text. Job
/// entries are retained for the session's whole life (only their report *text* is
/// released), so in a long session the listing grows past the bound and the tail is cut.
/// The parent reads this tool to find out what its most recent child produced — so the
/// newest outcome must be the part that survives, not the stale ones it already saw.
#[tokio::test]
async fn the_listing_keeps_the_newest_outcome_when_it_is_truncated() {
    let fx = fixture(vec![]);
    // 40 finished children with fat, individually identifiable reports: comfortably past
    // the 8 KiB tool-output bound once every entry's header and body is rendered.
    for i in 0..40 {
        let mock = MockProvider::with_scripts(vec![MockScript::text(format!(
            "RESULT-{i} {}",
            "p".repeat(400)
        ))]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let paths = test_paths(fx._tmp.path());
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
            .await
            .expect("spawn succeeds");
    }
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let out = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({}), &tool_ctx(&fx.ws))
        .await
        .expect("list succeeds");

    assert!(
        out.truncated,
        "fixture must exceed the bound to test truncation"
    );
    assert!(
        out.text.contains("RESULT-39"),
        "the newest child's result must survive truncation: the parent just waited for it, \
         and an oldest-first listing cuts it off. Got the tail: {:?}",
        out.text.chars().rev().take(120).collect::<String>()
    );
}

/// `background_list` is bounded, and a bounded listing cannot show every child in a long
/// session: with 40 children only the newest 19 outcomes fit, so the rest are cut off the
/// end. The parent still needs those — it started them, and `background_list` takes no
/// arguments, so there was no way to ask for a specific child at all. Naming an id must
/// return that child's report even when it falls outside what the full listing shows.
#[tokio::test]
async fn a_named_child_can_be_read_even_when_the_full_listing_is_truncated() {
    let fx = fixture(vec![]);
    for i in 0..40 {
        let mock = MockProvider::with_scripts(vec![MockScript::text(format!(
            "RESULT-{i} {}",
            "p".repeat(400)
        ))]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let paths = test_paths(fx._tmp.path());
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&fx.ws))
            .await
            .expect("spawn succeeds");
    }
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let all = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({}), &tool_ctx(&fx.ws))
        .await
        .expect("list succeeds");
    assert!(all.truncated, "fixture must exceed the bound");
    assert!(
        !all.text.contains("RESULT-0 "),
        "the oldest child falls outside the bounded listing; this test is about \
         reaching it by name"
    );

    // Child #1 is the oldest and cannot be reached by listing everything.
    let one = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({"id": 1}), &tool_ctx(&fx.ws))
        .await
        .expect("naming a child succeeds");
    assert!(
        one.text.contains("RESULT-0 "),
        "naming a child must return its report even when the full listing is truncated; \
         otherwise its result is unreachable forever. Got: {:?}",
        one.text
    );
}

/// Naming a child that was never started must say so and point at the listing, instead of
/// returning an empty body the model could mistake for "this child produced nothing".
#[tokio::test]
async fn naming_an_unknown_child_explains_itself() {
    let fx = fixture(vec![]);
    let out = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({"id": 99}), &tool_ctx(&fx.ws))
        .await
        .expect("naming an unknown child is not an error");
    assert!(
        out.text.contains("no background agent #99") && out.text.contains("background_list"),
        "an unknown id must be explained and must point at the listing, got: {:?}",
        out.text
    );
}

/// The per-entry clip exists so one long body cannot starve its siblings out of the
/// listing. When the parent names a single child there are no siblings to protect, and the
/// whole tool-output budget belongs to that one report — so the listing clip must not apply.
/// If it did, a 6 KiB report would come back cut at 4 KiB even though it fits the 8 KiB
/// bound, and the remaining bytes would be unreachable: the model is told the text is
/// complete (`truncated: false`) with no blob holding the rest.
#[tokio::test]
async fn a_named_read_uses_the_full_output_budget_not_the_listing_clip() {
    let fx = fixture(vec![]);
    // 6000 bytes: comfortably over the 4096 listing clip, comfortably under the 8192 bound.
    let body = "Q".repeat(5_900);
    let mock = MockProvider::with_scripts(vec![MockScript::text(format!(
        "HEAD-MARKER {body} TAIL-MARKER"
    ))]);
    let registry = Arc::new(ProviderRegistry::with_mock(mock));
    let paths = test_paths(fx._tmp.path());
    let tools =
        subagent::default_tools_with_hub(&paths, &fx.ws, &cfg(), registry, None, fx.hub.clone());
    tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "big"}), &tool_ctx(&fx.ws))
        .await
        .expect("spawn succeeds");
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // The full listing clips this one body: that is the per-entry bound doing its job.
    let all = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({}), &tool_ctx(&fx.ws))
        .await
        .expect("list succeeds");
    assert!(
        all.text.contains("[clipped:"),
        "fixture must exercise the listing clip"
    );
    assert!(
        !all.text.contains("TAIL-MARKER"),
        "the listing clip is expected to cut this report; the named read is what recovers it"
    );

    // Naming it must return the whole report, since it fits the output bound.
    let one = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({"id": 1}), &tool_ctx(&fx.ws))
        .await
        .expect("named read succeeds");
    assert!(
        one.text.contains("TAIL-MARKER"),
        "a named read must return the whole report when it fits the output bound, not the \
         listing clip; got {} bytes",
        one.text.len()
    );
    assert!(
        !one.text.contains("[clipped:"),
        "a named read of a report that fits the bound must not be clipped"
    );
    assert!(
        !one.truncated,
        "a 6 KiB report is within the output bound, so it must not report truncated"
    );
}

/// The counterpart to the test above: when a named report genuinely exceeds the output
/// bound it must be cut *honestly*. The listing clip cut silently, so a model reading a
/// named child could not tell a complete 4 KiB report from the first 4 KiB of a 50 KiB one.
/// The output bound instead reports `truncated` and keeps the full text as a blob.
#[tokio::test]
async fn a_named_read_past_the_output_bound_admits_truncation() {
    let fx = fixture(vec![]);
    let body = "Q".repeat(30_000);
    let mock = MockProvider::with_scripts(vec![MockScript::text(format!(
        "HEAD-MARKER {body} TAIL-MARKER"
    ))]);
    let registry = Arc::new(ProviderRegistry::with_mock(mock));
    let paths = test_paths(fx._tmp.path());
    let tools =
        subagent::default_tools_with_hub(&paths, &fx.ws, &cfg(), registry, None, fx.hub.clone());
    tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "huge"}), &tool_ctx(&fx.ws))
        .await
        .expect("spawn succeeds");
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let one = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({"id": 1}), &tool_ctx(&fx.ws))
        .await
        .expect("named read succeeds");
    assert!(
        one.truncated,
        "a 30 KiB report exceeds the 8 KiB bound, so a named read must report truncation \
         rather than presenting the excerpt as complete"
    );
    assert!(
        one.full.is_some(),
        "a truncated named read must keep the full text retrievable as a blob"
    );
}

/// When the full listing clips a report, it must point at the way to read the rest. Since
/// `background_list` takes an `id`, the clipped body is recoverable by naming that child —
/// so a marker saying "re-listing later is not guaranteed to show more" would steer the
/// model away from the only call that actually works, and it would abandon a report it
/// could have read in one step.
#[tokio::test]
async fn a_clipped_listing_points_at_the_named_read_that_recovers_it() {
    let fx = fixture(vec![]);
    let body = "Q".repeat(5_900);
    let mock = MockProvider::with_scripts(vec![MockScript::text(format!(
        "HEAD-MARKER {body} TAIL-MARKER"
    ))]);
    let registry = Arc::new(ProviderRegistry::with_mock(mock));
    let paths = test_paths(fx._tmp.path());
    let tools =
        subagent::default_tools_with_hub(&paths, &fx.ws, &cfg(), registry, None, fx.hub.clone());
    tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "big"}), &tool_ctx(&fx.ws))
        .await
        .expect("spawn succeeds");
    for _ in 0..400 {
        if fx
            .hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let all = fx
        .tools
        .get("background_list")
        .unwrap()
        .run(json!({}), &tool_ctx(&fx.ws))
        .await
        .expect("list succeeds");
    assert!(
        all.text.contains("[clipped:"),
        "fixture must exercise the clip"
    );
    assert!(
        all.text.contains("background_list with id 1"),
        "the clip marker must name the concrete id to call, so the model can recover the \
         clipped body in one step. Got: {:?}",
        all.text
    );
}

/// A named read must answer for the id it was given, or say that id is unknown. `id` is
/// declared `"type": "integer"`, and that includes negative numbers: `{"id": -1}` passes
/// argument validation, then `as_u64()` yields `None`. Treating that `None` as "no filter"
/// silently answered a *single-child* request with the WHOLE listing — the model asked to
/// read one report and got every child's report instead, with no error and no sign that its
/// filter was discarded. `{"id": 0}` already reports "no background agent #0"; a negative id
/// must be answered the same way rather than collapsing into an unfiltered listing.
#[tokio::test]
async fn a_negative_id_is_answered_as_unknown_not_as_an_unfiltered_listing() {
    let f = fixture(vec![
        MockScript::text("first body"),
        MockScript::text("second body"),
    ]);
    f.call("background_spawn", json!({"task": "one"})).await;
    f.call("background_spawn", json!({"task": "two"})).await;
    for _ in 0..400 {
        if f.hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // Control: the filter works for a real id, and an unknown non-negative id is named.
    let one = f.call("background_list", json!({"id": 1})).await;
    assert_eq!(
        one.lines().filter(|l| l.starts_with('#')).count(),
        1,
        "a named read must return exactly one child: {one}"
    );
    let zero = f.call("background_list", json!({"id": 0})).await;
    assert!(
        zero.contains("no background agent #0"),
        "an unknown id must be named, not ignored: {zero}"
    );

    // The defect: a negative id used to fall through to the unfiltered listing.
    let negative = f.try_call("background_list", json!({"id": -1})).await;
    let err = negative.expect_err("a negative id must be refused, not ignored");
    assert!(
        err.to_string().contains("non-negative"),
        "a negative id must be reported as unusable, not silently turned into a full \
         listing: {err}"
    );
}

/// A refused id must say what is actually wrong with it. `background_kill`,
/// `background_message` and `loop_cancel` read `id` with `as_u64()`, which is `None`
/// for a negative number — the same representability gap that made `background_list`
/// answer a one-child request with the whole listing. They refuse, which is right, but
/// the message they refuse with is `id required` for a call that *did* supply an id.
/// Only a negative number reaches that line (a string, float or null is rejected by
/// argument validation before the body runs), so the message is always wrong when it
/// appears: it tells the caller to add an argument it already sent, and hides that the
/// number was the problem. A model reading it retries the same id, or hunts for a
/// missing field that is present.
#[tokio::test]
async fn a_negative_id_is_refused_by_naming_it_not_by_asking_for_it() {
    let f = fixture(vec![]);
    let loops = std::sync::Arc::new(loon_core::loops::LoopRegistry::new());
    let loop_cancel = std::sync::Arc::new(loon_core::subagent::LoopCancelTool { registry: loops });

    for tool in ["background_kill", "background_message"] {
        let args = if tool == "background_message" {
            json!({"id": -1, "message": "hi"})
        } else {
            json!({"id": -1})
        };
        // The id is genuinely absent only in this shape; it must keep saying so.
        let absent = if tool == "background_message" {
            json!({"message": "hi"})
        } else {
            json!({})
        };
        let spec = f.tools.get(tool).unwrap().spec();
        let absent_err =
            loon_tools::validate_args(&spec, &absent).expect_err("a missing id must be rejected");
        assert!(
            absent_err
                .to_string()
                .contains("missing required field `id`"),
            "an absent id must be reported as missing: {absent_err}"
        );

        // A negative id was supplied. Saying "id required" misdirects the caller.
        let err = f
            .try_call(tool, args)
            .await
            .expect_err("a negative id must be refused");
        let text = err.to_string();
        assert!(
            !text.contains("id required"),
            "{tool} was given an id (-1) and must not answer 'id required': {text}"
        );
        assert!(
            text.contains("-1") && text.contains("non-negative"),
            "{tool} must name the unusable id and why it is unusable: {text}"
        );
    }

    // `loop_cancel` shares the same id shape (ids start at 1), so it must refuse
    // the same way rather than asking for an id it was already given.
    let ctx = f.ctx();
    let err = loon_tools::Tool::run(&*loop_cancel, json!({"id": -1}), &ctx)
        .await
        .expect_err("a negative id must be refused");
    let text = err.to_string();
    assert!(
        !text.contains("id required"),
        "loop_cancel was given an id (-1) and must not answer 'id required': {text}"
    );
    assert!(
        text.contains("-1") && text.contains("non-negative"),
        "loop_cancel must name the unusable id and why it is unusable: {text}"
    );
}

/// The drop notice must not promise a recovery that does not exist.
///
/// Two independent bounds release information about a finished child: the entry bound
/// drops announcements past `RETAINED_REPORT_ENTRIES`, and the text bound releases the
/// oldest reports past `RETAINED_REPORT_BYTES`. When both fire on the same child, the
/// notice used to say "call background_list to read them" for reports whose text had
/// already been released — measured 0 of 8 dropped ids still readable — so the model was
/// sent after a report that reads back as released. The notice has to name the ids that
/// can actually be read and say plainly when the rest cannot.
#[tokio::test]
async fn a_drop_notice_only_promises_reads_that_will_work() {
    use loon_core::subagent::JobStatus;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();
    // 40 children with 50 KB reports: the entry bound drops announcements (40 > 32) AND
    // the text bound releases the oldest reports (40 x 50 KB >> 64 KB), so the two
    // bounds overlap — the case where the old notice lied.
    for i in 0..40u64 {
        let big = format!("CHILD-{i}-") + &"x".repeat(50_000);
        let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![MockScript::text(big)],
        )));
        let child_tools = subagent::default_tools_with_hub(
            &paths,
            &ws,
            &cfg(),
            child_registry,
            None,
            hub.clone(),
        );
        child_tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": format!("task {i}")}), &tool_ctx(&ws))
            .await
            .expect("spawn succeeds");
    }
    for _ in 0..400 {
        if hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let mut agent = Agent::new_session(&paths, cfg(), parent_registry, parent_tools, ws, true)
        .expect("parent session");
    agent.background = hub.clone();
    let _ = agent.run_turn("carry on").await;

    let note = agent
        .history
        .iter()
        .find(|m| m.text.contains("were not announced individually"))
        .expect("the parent must be told that announcements were dropped")
        .text
        .clone();

    // Every id the notice tells the model to READ must actually be readable. Parse ids
    // only from the read clause: the note also contains a bare count ("8 report(s)"),
    // and treating that number as an id would test the parser rather than the notice.
    let listed = hub.listing();
    let mut promised = 0;
    if let Some(rest) = note.split("call background_list with id ").nth(1) {
        let ids_part = rest.split(" to read them").next().unwrap_or("");
        for id in ids_part
            .split(',')
            .filter_map(|t| t.trim().parse::<u64>().ok())
        {
            promised += 1;
            let readable = listed.iter().any(|(i, _, _, r, _)| *i == id && r.is_some());
            assert!(
                readable,
                "the notice told the model to read #{id}, but its report was released: {note}"
            );
        }
    }
    // In this shape both bounds fire, so there is nothing left to recover: the notice
    // must say that plainly rather than sending the model after released text.
    let released = listed.iter().filter(|(_, _, _, r, _)| r.is_none()).count();
    if released > 0 {
        assert!(
            note.contains("no longer readable"),
            "released reports exist ({released}), so the notice must say their text is gone: {note}"
        );
    }
    assert!(
        promised > 0 || note.contains("no longer readable"),
        "the notice must either name readable ids or say the text is gone: {note}"
    );
}

/// Readability has to be decided when the notice is built, not when the announcement was
/// dropped.
///
/// The two bounds run at different times. The entry bound drops an announcement when the
/// queue passes `RETAINED_REPORT_ENTRIES`; the text bound keeps evicting oldest-first every
/// time a child finishes. So a report can be retained at the moment its announcement is
/// dropped and released before the parent's next step boundary — and recording the answer
/// at drop time then names ids that `background_list` can no longer return.
///
/// Measured on exactly this shape before the fix (40 small children, then 6 large ones):
/// nine of fourteen dropped ids were recorded `readable=true` while their text was gone, so
/// the notice told the model to read nine released reports.
#[tokio::test]
async fn a_drop_notice_decides_readability_when_it_is_built_not_when_dropped() {
    use loon_core::subagent::JobStatus;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();

    async fn spawn_one(
        paths: &LoonPaths,
        ws: &std::path::Path,
        hub: &BackgroundHub,
        text: String,
        task: String,
    ) {
        let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![MockScript::text(text)],
        )));
        let tools =
            subagent::default_tools_with_hub(paths, ws, &cfg(), registry, None, hub.clone());
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": task}), &tool_ctx(ws))
            .await
            .expect("spawn succeeds");
    }

    async fn settle(hub: &BackgroundHub) {
        for _ in 0..400 {
            if hub
                .listing()
                .iter()
                .all(|(_, _, s, _, _)| *s != JobStatus::Running)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // Phase 1 — 40 small children: the text total stays far under the budget, so ONLY the
    // entry bound fires and every drop happens while its report is still retained.
    for i in 0..40u64 {
        spawn_one(&paths, &ws, &hub, format!("small-{i}"), format!("t{i}")).await;
        settle(&hub).await;
    }

    // Direction A — a dropped-but-retained announcement must be reported as readable, so
    // the model is actually told the id it can recover the text from.
    let (_queued, dropped_a) = hub.drain_reports();
    assert!(
        !dropped_a.is_empty(),
        "the entry bound must have dropped some"
    );
    let listed_a = hub.listing();
    for (id, recorded_readable) in &dropped_a {
        let actually_readable = listed_a
            .iter()
            .any(|(i, _, _, r, _)| i == id && r.is_some());
        assert_eq!(
            *recorded_readable, actually_readable,
            "the notice's claim about #{id} must match what background_list can return now \
             (recorded {recorded_readable}, actually {actually_readable})"
        );
    }
    assert!(
        dropped_a.iter().any(|(_, readable)| *readable),
        "small children under the text budget must stay readable after their announcement \
         is dropped, otherwise the recovery path is never offered"
    );

    // Phase 2 — 40 more small children (new drops), then large ones whose completion makes
    // the text bound evict oldest-first — releasing exactly the reports whose announcements
    // were dropped while they were still retained.
    for i in 40..80u64 {
        spawn_one(&paths, &ws, &hub, format!("small-{i}"), format!("t{i}")).await;
        settle(&hub).await;
    }
    for i in 0..6u64 {
        let big = format!("BIG-{i}-") + &"x".repeat(50_000);
        spawn_one(&paths, &ws, &hub, big, format!("b{i}")).await;
        settle(&hub).await;
    }

    // Direction B — the notice must not name an id whose text a later eviction released.
    let (_queued, dropped_b) = hub.drain_reports();
    assert!(
        !dropped_b.is_empty(),
        "the second burst must drop announcements too"
    );
    let listed_b = hub.listing();
    for (id, recorded_readable) in &dropped_b {
        let actually_readable = listed_b
            .iter()
            .any(|(i, _, _, r, _)| i == id && r.is_some());
        assert_eq!(
            *recorded_readable, actually_readable,
            "the notice's claim about #{id} must match what background_list can return now \
             (recorded {recorded_readable}, actually {actually_readable}) — a report released \
             after its announcement was dropped must not still be named as readable"
        );
    }
}

/// A child's only result must never be destroyed by the bound that exists to bound memory.
///
/// `trim_reports` evicts oldest-first while the retained total exceeds the budget. When a
/// single report is larger than the budget, that loop evicts it too — from both surfaces —
/// so a child that returned one big answer leaves the parent with nothing but "released".
/// The memory bound is about not accumulating unboundedly; it must not silently discard the
/// one piece of work the parent asked for.
///
/// Measured before the fix through the real tools: a 65 536-byte report gave
/// `report_len=None`, `queued_has_text=false`, and `background_list {"id": 1}` could no
/// longer return the child's result at all (65 000 bytes was still fine, so the boundary is
/// exactly the 64 KB budget).
#[tokio::test]
async fn one_report_larger_than_the_budget_is_still_delivered() {
    for size in [65_536usize, 100_000] {
        let fx = fixture(vec![]);
        let paths = test_paths(fx._tmp.path());
        let text = format!("ONLY-RESULT-{size}-") + &"x".repeat(size);
        let mock = MockProvider::with_scripts(vec![MockScript::text(text)]);
        let registry = Arc::new(ProviderRegistry::with_mock(mock));
        let tools = subagent::default_tools_with_hub(
            &paths,
            &fx.ws,
            &cfg(),
            registry,
            None,
            fx.hub.clone(),
        );
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": "only"}), &fx.ctx())
            .await
            .expect("spawn succeeds");
        for _ in 0..600 {
            if fx
                .hub
                .listing()
                .iter()
                .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // The parent must still be able to read the one result that exists.
        let read = tools
            .get("background_list")
            .unwrap()
            .run(json!({"id": 1}), &fx.ctx())
            .await
            .expect("a named read of a finished child succeeds")
            .text;
        assert!(
            read.contains("ONLY-RESULT"),
            "a single {size}-byte report must survive the retention bound: the child's only \
             result was released, so the parent can never read it (got {read:.200})"
        );
    }
}

/// The retention bound is a bound on accumulation, and the newest report is exempt.
///
/// Pinning the shape the exemption creates: an oversized report that is then followed by
/// smaller ones is released normally (it is no longer the newest), so peak retention stays
/// inside the documented `2 * budget`; while an oversized report that IS the newest is kept
/// and the peak is bounded by twice that single report instead. Both directions are measured
/// so a future change cannot widen the bound unnoticed.
#[tokio::test]
async fn the_retention_peak_is_twice_the_budget_or_the_newest_report() {
    const BUDGET: usize = 64 * 1024;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();

    async fn spawn_one(
        paths: &LoonPaths,
        ws: &std::path::Path,
        hub: &BackgroundHub,
        text: String,
        task: String,
    ) {
        let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![MockScript::text(text)],
        )));
        let tools =
            subagent::default_tools_with_hub(paths, ws, &cfg(), registry, None, hub.clone());
        tools
            .get("background_spawn")
            .unwrap()
            .run(json!({"task": task}), &tool_ctx(ws))
            .await
            .expect("spawn succeeds");
        for _ in 0..600 {
            if hub
                .listing()
                .iter()
                .all(|(_, _, s, _, _)| *s != loon_core::subagent::JobStatus::Running)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // An oversized report that is the NEWEST is kept, and nothing else accumulates with it.
    spawn_one(
        &paths,
        &ws,
        &hub,
        format!("HUGE-{}", "x".repeat(100_000)),
        "h".into(),
    )
    .await;
    for i in 0..3u64 {
        spawn_one(&paths, &ws, &hub, format!("small-{i}"), format!("s{i}")).await;
    }
    let listed: usize = hub
        .listing()
        .iter()
        .filter_map(|(_, _, _, r, _)| r.as_ref().map(|r| r.len()))
        .sum();
    let (delivered, _dropped) = hub.drain_reports();
    let queued: usize = delivered.iter().map(|(_, _, _, t)| t.len()).sum();
    // The oversized report is no longer newest, so it is released; the small ones fit.
    assert!(
        listed + queued <= 2 * BUDGET,
        "once an oversized report is no longer newest it must be released like any other, \
         so retention stays within twice the budget (listed {listed} + queued {queued})"
    );
    assert!(
        listed > 0,
        "the small reports that fit the budget must not be released with it"
    );

    // An oversized report that IS the newest is kept whole: its text must be readable.
    let hub2 = BackgroundHub::new();
    spawn_one(
        &paths,
        &ws,
        &hub2,
        format!("NEWEST-{}", "x".repeat(100_000)),
        "n".into(),
    )
    .await;
    let newest_len = hub2
        .listing()
        .iter()
        .filter_map(|(_, _, _, r, _)| r.as_ref().map(|r| r.len()))
        .sum::<usize>();
    assert!(
        newest_len > BUDGET,
        "the newest report must survive even though it exceeds the budget on its own \
         (retained {newest_len}, budget {BUDGET})"
    );
}

/// The drop notice must bound ITSELF, not just the reports it talks about.
///
/// The hub bounds how many announcements reach a step (`RETAINED_REPORT_ENTRIES`) and how
/// much report text survives (`RETAINED_REPORT_BYTES`), and every report body is bound
/// again on injection. The notice that explains the drops was the one injected surface with
/// no bound of its own: it named every dropped id inline, so it grew with the number of
/// children rather than with the step — the exact accumulation the entry bound exists to
/// prevent, reintroduced by the message announcing it.
///
/// Measured before the cap: 400 tiny children produced a 2 224-byte note naming ~368 ids
/// (~6 bytes each). The entry bound's own rationale cites an 8 000-report flood; the same
/// shape there is ~44 KB of pure bookkeeping.
#[tokio::test]
async fn the_drop_notice_is_bounded_even_when_many_children_finish() {
    use loon_core::subagent::JobStatus;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();
    // Tiny reports: the entry bound fires hard, the text bound never does, so the notice
    // is the only thing that can grow.
    const TOTAL: u64 = 200;
    const WAVE: u64 = 20;
    let mut spawned = 0u64;
    while spawned < TOTAL {
        let end = (spawned + WAVE).min(TOTAL);
        for i in spawned..end {
            let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
                vec![MockScript::text(format!("ok{i}"))],
            )));
            let child_tools = subagent::default_tools_with_hub(
                &paths,
                &ws,
                &cfg(),
                child_registry,
                None,
                hub.clone(),
            );
            child_tools
                .get("background_spawn")
                .unwrap()
                .run(json!({"task": format!("task {i}")}), &tool_ctx(&ws))
                .await
                .expect("spawn succeeds");
        }
        for _ in 0..400 {
            if hub
                .listing()
                .iter()
                .all(|(_, _, s, _, _)| *s != JobStatus::Running)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        spawned = end;
    }

    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let mut agent = Agent::new_session(&paths, cfg(), parent_registry, parent_tools, ws, true)
        .expect("parent session");
    agent.background = hub.clone();
    let _ = agent.run_turn("carry on").await;

    let note = agent
        .history
        .iter()
        .find(|m| m.text.contains("were not announced individually"))
        .expect("the parent must be told that announcements were dropped")
        .text
        .clone();

    // The note is injected into the parent's context like any other report, so it has to
    // respect the same budget those reports do. 200 dropped ids at ~6 bytes each was
    // ~1.2 KB; the cap keeps it to a few hundred bytes.
    let named = note
        .split("call background_list with id ")
        .nth(1)
        .and_then(|rest| rest.split(" to read them").next())
        .map(|ids| ids.split(',').filter(|t| !t.trim().is_empty()).count())
        .unwrap_or(0);
    assert!(
        named <= 16,
        "the notice must not enumerate every dropped id (named {named}): {note}"
    );
    assert!(
        note.len() < 1024,
        "the drop notice must stay small next to the step it is protecting, got {} bytes: {note}",
        note.len()
    );

    // Capping the enumeration must not cost the truth: the exact count survives, and the
    // model is still told how to reach the reports that were not named.
    //
    // The dropped count is not `TOTAL`: the entry bound KEEPS the newest
    // `RETAINED_REPORT_ENTRIES` announcements, so 200 children drop 200 - 32 = 168. Assert
    // the shape rather than a hardcoded number, so this test does not break if that
    // constant is retuned.
    let announced: usize = note
        .split(" report(s) were not announced individually")
        .next()
        .and_then(|head| head.rsplit(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(
        announced > 100,
        "the notice must still report how many announcements were dropped, got {announced}: {note}"
    );
    assert!(
        note.contains("background_list with id <N>"),
        "ids beyond the cap must stay reachable, so the notice must name the call that \
         actually reaches them (a named read; the unfiltered listing is truncated first): {note}"
    );
    assert!(
        !note.contains("with no arguments"),
        "the notice must not send the model to an unfiltered listing for the hidden ids: \
         that listing is bounded and newest-first, so it loses exactly the older reports \
         the cap hid (measured: 19 of 104 reachable): {note}"
    );
}

/// The recovery the drop notice recommends must actually reach the reports it names.
///
/// Capping the notice's enumeration keeps the step small, but the ids beyond the cap have
/// to stay readable by SOME call the notice recommends. An unfiltered `background_list` is
/// not that call: it is bounded too and renders newest-first, so the output cut removes
/// exactly the older reports the cap hid. Measured with 120 children (104 readable ids
/// beyond the 16 named): only 19 appeared in the unfiltered listing, so 85 were
/// unreachable by the call the notice pointed at — the same "promise a recovery that does
/// not work" defect this notice had once before.
///
/// This measures the two paths rather than trusting the wording: the unfiltered listing
/// loses most hidden ids, and a named read returns the one it asks for.
#[tokio::test]
async fn the_recovery_the_notice_names_actually_reaches_the_hidden_reports() {
    use loon_core::subagent::JobStatus;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let hub = BackgroundHub::new();
    // Bodies large enough that the listing's tool-output bound actually truncates, so the
    // difference between "listed" and "named" is real and not an artefact of a tiny hub.
    const TOTAL: u64 = 120;
    const WAVE: u64 = 20;
    let mut spawned = 0u64;
    while spawned < TOTAL {
        let end = (spawned + WAVE).min(TOTAL);
        for i in spawned..end {
            let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
                vec![MockScript::text(format!("BODY-{i}-{}", "y".repeat(400)))],
            )));
            let child_tools = subagent::default_tools_with_hub(
                &paths,
                &ws,
                &cfg(),
                child_registry,
                None,
                hub.clone(),
            );
            child_tools
                .get("background_spawn")
                .unwrap()
                .run(json!({"task": format!("task {i}")}), &tool_ctx(&ws))
                .await
                .expect("spawn succeeds");
        }
        for _ in 0..400 {
            if hub
                .listing()
                .iter()
                .all(|(_, _, s, _, _)| *s != JobStatus::Running)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        spawned = end;
    }

    let parent_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("parent done")],
    )));
    let parent_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let mut agent = Agent::new_session(
        &paths,
        cfg(),
        parent_registry.clone(),
        parent_tools,
        ws.clone(),
        true,
    )
    .expect("parent session");
    agent.background = hub.clone();
    let _ = agent.run_turn("carry on").await;

    let note = agent
        .history
        .iter()
        .find(|m| m.text.contains("were not announced individually"))
        .expect("the parent must be told that announcements were dropped")
        .text
        .clone();

    let named: Vec<u64> = note
        .split("call background_list with id ")
        .nth(1)
        .and_then(|rest| rest.split(" to read them").next())
        .map(|ids| {
            ids.split(',')
                .filter_map(|t| t.trim().trim_start_matches('#').parse().ok())
                .collect()
        })
        .unwrap_or_default();
    assert!(!named.is_empty(), "the notice must name some ids: {note}");

    let tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        parent_registry.clone(),
        None,
        hub.clone(),
    );
    let unfiltered = tools
        .get("background_list")
        .unwrap()
        .run(json!({}), &tool_ctx(&ws))
        .await
        .expect("listing");

    // Every id the notice NAMES must be readable by a named read — that is the whole point
    // of naming it. Compare against the hub's own copy rather than assuming an id/task
    // numbering relationship (ids start at 1, task labels at 0).
    for id in &named {
        let expected = hub
            .listing()
            .into_iter()
            .find(|(i, _, _, _, _)| i == id)
            .and_then(|(_, _, _, r, _)| r)
            .unwrap_or_else(|| panic!("#{id} was named as readable but the hub has no report"));
        let probe: String = expected.chars().take(20).collect();
        let out = tools
            .get("background_list")
            .unwrap()
            .run(json!({ "id": id }), &tool_ctx(&ws))
            .await
            .expect("named read");
        assert!(
            out.text.contains(&probe),
            "the notice named #{id} but a named read did not return its body ({probe:?}): {}",
            out.text
        );
    }

    // And the call the notice recommends for the ids it did NOT name must be a named read:
    // an unfiltered listing provably loses most of them, so recommending it would promise
    // a recovery that does not work.
    let readable_ids: Vec<u64> = hub
        .listing()
        .iter()
        .filter(|(_, _, _, r, _)| r.is_some())
        .map(|(i, _, _, _, _)| *i)
        .collect();
    let mut hidden: Vec<u64> = readable_ids;
    for id in &named {
        hidden.retain(|x| x != id);
    }
    assert!(!hidden.is_empty(), "this shape must hide some readable ids");
    let reachable_by_listing = hidden
        .iter()
        .filter(|id| unfiltered.text.contains(&format!("#{id} ")))
        .count();
    assert!(
        reachable_by_listing < hidden.len(),
        "this shape is only meaningful if the unfiltered listing really does lose hidden \
         ids (reached {reachable_by_listing} of {})",
        hidden.len()
    );
    assert!(
        note.contains("background_list with id <N>"),
        "the hidden ids must be recoverable by a NAMED read, which bypasses the listing \
         clip; an unfiltered listing loses {reachable_by_listing} of {} of them: {note}",
        hidden.len()
    );
}

/// A resumed transcript must show the tool calls its session actually made.
///
/// `emit_session_loaded` used to filter the replayed model history to user/assistant
/// *text* only, silently dropping every `Role::Tool` message the model had seen. A
/// resumed session therefore read as bare prose with all of its commands, reads and MCP
/// calls missing — "just text", exactly as reported. `visible_blocks` now carries tool
/// rows with the same name/detail/ok/excerpt the live `Block::Tool` renders, rebuilt
/// from the journal so resume shows what the session did.
#[tokio::test]
async fn a_resumed_transcript_includes_the_tool_calls_the_session_made() {
    use loon_core::replay::{visible_blocks, VisibleBlock};
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    // One turn: emit one model text script, then one tool call, then the answer.
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![
            MockScript::new(vec![loon_providers::MockStep::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                args_json: r#"{"command":"echo hi"}"#.into(),
            }]),
            MockScript::text("done after the tool"),
        ],
    )));
    let tools =
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), registry.clone(), None, hub_fresh());
    let mut a =
        Agent::new_session(&paths, cfg(), registry, tools, ws.clone(), true).expect("session");
    let r = a.run_turn("run a command").await;
    assert!(
        matches!(r.status, loon_core::TurnStatus::Completed),
        "turn must complete: {:?}",
        r.status
    );

    let rebuilt = visible_blocks(&a.journal.events().unwrap(), None);
    let tools_shown: Vec<&VisibleBlock> = rebuilt
        .iter()
        .filter(|b| matches!(b, VisibleBlock::Tool { .. }))
        .collect();
    assert_eq!(
        tools_shown.len(),
        1,
        "a resumed transcript must show the one tool call the session made: {rebuilt:?}"
    );
    match &tools_shown[0] {
        VisibleBlock::Tool {
            name,
            detail,
            ok,
            excerpt,
        } => {
            assert_eq!(name, "bash");
            // `detail` is the human-facing command, as the live row renders it.
            assert!(
                detail.contains("echo hi"),
                "the tool row must carry the command, not an empty detail: {detail:?}"
            );
            assert!(*ok, "the mock shell run succeeded");
            assert!(
                excerpt.contains("hi"),
                "the tool row must carry the result: {excerpt:?}"
            );
        }
        _ => unreachable!(),
    }
    // And the surrounding conversation survives alongside the tool row.
    assert!(
        rebuilt
            .iter()
            .any(|b| matches!(b, VisibleBlock::User { text } if text == "run a command")),
        "the user row must survive: {rebuilt:?}"
    );
    assert!(
        rebuilt
            .iter()
            .any(|b| matches!(b, VisibleBlock::Assistant { text } if text.contains("done after the tool"))),
        "the assistant text must survive: {rebuilt:?}"
    );
}

fn hub_fresh() -> BackgroundHub {
    BackgroundHub::new()
}

/// A refused call must be visible on resume too, not just successful ones.
#[tokio::test]
async fn a_rejected_tool_call_is_visible_on_resume() {
    use loon_core::replay::{visible_blocks, VisibleBlock};
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![
            MockScript::new(vec![loon_providers::MockStep::ToolCall {
                // `nope_tool` is not registered, so the call is rejected before execution.
                id: "call-9".into(),
                name: "nope_tool".into(),
                args_json: "{}".into(),
            }]),
            MockScript::text("done after the refusal"),
        ],
    )));
    let tools =
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), registry.clone(), None, hub_fresh());
    let mut a =
        Agent::new_session(&paths, cfg(), registry, tools, ws.clone(), true).expect("session");
    let _ = a.run_turn("try a missing tool").await;

    let rebuilt = visible_blocks(&a.journal.events().unwrap(), None);
    assert!(
        rebuilt.iter().any(
            |b| matches!(b, VisibleBlock::Tool { name, ok: false, .. } if name == "nope_tool")
        ),
        "a refused call must appear as a failed tool row on resume: {rebuilt:?}"
    );
}

/// The wiring, not just the builder: resuming a session and emitting `SessionLoaded`
/// must hand the UI the tool rows. The earlier test called `visible_blocks` directly,
/// which passed while the real resume path emitted nothing at all.
#[tokio::test]
async fn resuming_a_session_emits_tool_rows_to_the_ui() {
    use loon_core::replay::VisibleBlock;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![
            MockScript::new(vec![loon_providers::MockStep::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                args_json: r#"{"command":"echo hi"}"#.into(),
            }]),
            MockScript::text("done after the tool"),
        ],
    )));
    let tools =
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), registry.clone(), None, hub_fresh());
    let mut a =
        Agent::new_session(&paths, cfg(), registry, tools, ws.clone(), true).expect("session");
    let _ = a.run_turn("run a command").await;
    let sid = a.session_id();

    // Resume from a DIFFERENT session: `/resume <own id>` short-circuits without
    // emitting (it is already open), so the realistic path is switching from another
    // session to this one.
    // A fresh, different session — the state a user is in when they type /resume.
    let other_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::text("other")],
    )));
    let other_tools = subagent::default_tools_with_hub(
        &paths,
        &ws,
        &cfg(),
        other_registry.clone(),
        None,
        hub_fresh(),
    );
    let mut resumed =
        Agent::new_session(&paths, cfg(), other_registry, other_tools, ws.clone(), true)
            .expect("second session");
    assert_ne!(resumed.session_id(), sid, "must start elsewhere");

    // Drive the real path: `/resume <id>` replaces the agent and emits SessionLoaded,
    // which is exactly what the TUI consumes. No test-only hook involved.
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    resumed.set_event_sink(tx);
    let out = resumed
        .run_slash(loon_core::commands::SlashCommand::Resume {
            target: Some(sid.to_string()),
        })
        .await;
    assert!(out.is_ok(), "resume must succeed: {out:?}");
    let mut blocks = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::SessionLoaded { blocks: b } = ev {
            blocks = b;
        }
    }
    assert!(
        !blocks.is_empty(),
        "resuming must emit the session's transcript, not an empty load"
    );
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, VisibleBlock::User { text } if text == "run a command")),
        "the user's message must survive resume: {blocks:?}"
    );
    assert!(
        blocks.iter().any(
            |b| matches!(b, VisibleBlock::Tool { name, detail, ok: true, .. }
                if name == "bash" && detail.contains("echo hi"))
        ),
        "the tool call must survive resume, with its command: {blocks:?}"
    );
}

/// A child is a separate agent with its own prompt, so "the rules reach the model" has to
/// hold for children too — otherwise a delegated subtask would be free to guess and to
/// pipe file writes through bash while the parent is held to the rules.
///
/// The mechanism is worth stating because the obvious guess is wrong: children do NOT
/// inherit a prompt captured at construction. `run_turn` rebuilds `system_prompt` from
/// `BASE_SYSTEM` on every turn (lib.rs), so a child that outlives a prompt edit picks up
/// the current text. Stripping the rules at child-construction time therefore proves
/// nothing — an earlier probe did exactly that and this test still passed. The load-bearing
/// check is to strip them from `BASE_SYSTEM` itself, which is what was verified.
#[tokio::test]
async fn a_child_agent_inherits_the_evidence_and_tool_rules() {
    let (f, mock) = fixture_with_mock(vec![MockScript::text("child done")]);
    f.call("background_spawn", json!({"task": "do the subtask"}))
        .await;

    // Wait for the child's request to be recorded.
    let mut listed = String::new();
    for _ in 0..400 {
        listed = f.call("background_list", json!({})).await;
        if listed.contains("completed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(listed.contains("completed"), "child must finish: {listed}");

    let requests = mock.requests();
    let child_system = requests
        .first()
        .and_then(|r| r.messages.first())
        .map(|m| m.text.clone())
        .expect("the child must have sent a request leading with a system message");
    assert!(
        child_system.contains("Never guess"),
        "the child must be held to the no-guessing rule too: {child_system:.300}"
    );
    assert!(
        child_system.contains("path:line"),
        "the child must be required to cite the line"
    );
    assert!(
        child_system.contains("Prefer the dedicated tool over a shell equivalent"),
        "the child must prefer dedicated tools as well"
    );
}

/// A running job reports how long it has run: the parent (and the user's live panel) need
/// "3s means wait, 10m means steer or kill", which a bare "running" cannot give.
#[tokio::test]
async fn a_running_job_reports_elapsed_seconds() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let hub = BackgroundHub::new();

    // The child sleeps briefly, so it is genuinely "running" when we check.
    let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
        vec![MockScript::new(vec![
            MockStep::Delay(std::time::Duration::from_millis(400)),
            MockStep::Text("done".into()),
        ])],
    )));
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();
    let tools =
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), child_registry, None, hub.clone());
    let ctx = tool_ctx(&ws);
    tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "slow job"}), &ctx)
        .await
        .expect("spawn succeeds");

    // Immediately after spawn: running, with a small elapsed count.
    let running = hub.running();
    assert_eq!(running.len(), 1, "{running:?}");
    let (id, label, secs) = running[0].clone();
    assert!(secs <= 2, "freshly spawned: {secs}s");
    assert_eq!(label, "slow job");

    // `listing()` carries the same elapsed figure for the finished read.
    for _ in 0..200 {
        if hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != subagent::JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let finished = hub.listing();
    assert_eq!(finished.len(), 1);
    assert!(
        finished[0].4 <= 5,
        "elapsed is frozen at completion, not growing: {:?}",
        finished[0]
    );
    assert!(
        hub.running().is_empty(),
        "finished jobs leave the live view"
    );
    let _ = id;
}

/// The concurrent-children cap. Without it a parent could spawn forty "investigate the
/// parser" children — forty concurrent model loops — with nothing refusing. The cap is
/// checked under the hub lock at spawn, so a parallel fan-out cannot race past it, and a
/// finished child frees its slot for the next spawn.
#[tokio::test]
async fn the_concurrency_cap_refuses_then_frees() {
    use subagent::JobStatus;
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    let paths = test_paths(tmp.path());
    paths.ensure().unwrap();

    // Cap of 2, children that sleep so both slots stay occupied.
    let hub = subagent::BackgroundHub::new_capped(2);
    let mk_tools = || {
        let child_registry = Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            vec![MockScript::new(vec![
                MockStep::Delay(std::time::Duration::from_millis(300)),
                MockStep::Text("done".into()),
            ])],
        )));
        subagent::default_tools_with_hub(&paths, &ws, &cfg(), child_registry, None, hub.clone())
    };
    let ctx = tool_ctx(&ws);
    let tools = mk_tools();

    tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "first"}), &ctx)
        .await
        .expect("first spawn fits the cap");
    tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "second"}), &ctx)
        .await
        .expect("second spawn fills the cap");

    let third = tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "third"}), &ctx)
        .await;
    match third {
        Ok(out) => panic!("the third spawn must be refused: {}", out.text),
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains("limit reached (2 running)"), "{msg}");
            assert!(
                msg.contains("background_kill"),
                "the refusal must say what to do about it: {msg}"
            );
        }
    }
    assert_eq!(
        hub.running().len(),
        2,
        "the refused spawn must not have registered"
    );

    // Finish one child: its slot frees for the next spawn.
    for _ in 0..200 {
        if hub
            .listing()
            .iter()
            .all(|(_, _, s, _, _)| *s != JobStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let tools = mk_tools();
    tools
        .get("background_spawn")
        .unwrap()
        .run(json!({"task": "fourth"}), &ctx)
        .await
        .expect("a finished child frees its slot");
    assert_eq!(hub.running().len(), 1);
}
