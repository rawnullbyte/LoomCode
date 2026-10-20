//! A10 crash recovery, real-process edition: start a headless `loon run`, let it commit
//! its first journaled events, then SIGKILL it while a provider request is still
//! in-flight (the test holds the TCP listener silent so the request cannot finish).
//! Everything asserted afterwards is derived from the surviving files alone:
//!
//! 1. the store passes integrity_check,
//! 2. every journaled event parses and the partial history replays,
//! 3. the session can be resumed and continued to a completed turn.
//!
//! The provider in this test is a silent socket plus, after the crash, the deterministic
//! mock — stated as such; no live-model claim.

use loon_config::LoonPaths;
use loon_core::{Agent, AgentConfig};
use loon_events::model::EventKind;
use loon_providers::{MockScript, ProviderRegistry};
use loon_tools::ToolRegistry;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn test_paths(home: &std::path::Path) -> LoonPaths {
    LoonPaths {
        config_dir: home.join("config"),
        data_dir: home.join("data"),
        state_dir: home.join("state"),
        cache_dir: home.join("cache"),
        runtime_dir: home.join("runtime"),
    }
}

/// Accept connections and never answer — the CLI blocks in-flight on this.
struct SilentListener(std::net::TcpListener);

impl SilentListener {
    fn bind() -> std::io::Result<SilentListener> {
        Ok(SilentListener(std::net::TcpListener::bind("127.0.0.1:0")?))
    }
    fn port(&self) -> u16 {
        self.0.local_addr().unwrap().port()
    }
}

fn write_config(home: &std::path::Path, port: u16) {
    let cfg_dir = home.join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            r#"[session]
model = "slow"

[providers.slow]
base_url = "http://127.0.0.1:{port}/v1"
api = "chat"
timeout_secs = 30

[models.slow]
provider = "slow"
id = "slow-1"
"#
        ),
    )
    .unwrap();
}

fn spawn_loon(home: &std::path::Path, ws: &std::path::Path, prompt: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_loon"))
        .args([
            "run",
            prompt,
            "--cwd",
            ws.to_str().unwrap(),
            "--output",
            "jsonl",
        ])
        .env("LOON_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn loon")
}

/// Poll until the journaled events include a TurnStarted. TurnStarted is committed
/// before the provider request is sent, so from that moment the process is mid-turn;
/// with the silent listener holding the socket, no response can ever arrive. (The
/// serving member is journaled only when the first provider event lands — before that
/// it is genuinely unknown — so RequestStarted is not the right trigger here.)
fn wait_for_turn_started(
    store: &loon_events::Store,
    timeout: Duration,
) -> Option<loon_events::EvId> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(sessions) = store.list_sessions() {
            if let Some(meta) = sessions.first() {
                if let Ok(events) = store.events(meta.id) {
                    if events
                        .iter()
                        .any(|e| matches!(e.kind, EventKind::TurnStarted { .. }))
                    {
                        return Some(meta.id);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[test]
fn kill9_mid_request_leaves_a_consistent_resumable_session() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();

    let listener = SilentListener::bind().expect("bind listener");
    write_config(&home, listener.port());
    let paths = test_paths(&home);

    let mut child = spawn_loon(&home, &ws, "task that will be interrupted");
    let store = loon_core::open_store(&paths).expect("open store");

    let session_id = wait_for_turn_started(&store, Duration::from_secs(15)).expect(
        "loon must reach TurnStarted within 15s — a silent skip here would hide a broken test",
    );

    // ── the kill: SIGKILL mid-turn ───────────────────────────────────────────────
    assert!(
        child.try_wait().unwrap().is_none(),
        "loon must still be running (blocked on the silent socket) at kill time"
    );
    child.kill().expect("kill -9 loon");
    let _ = child.wait();
    // Drain pending connections so the killed client's socket state doesn't linger.
    listener.0.set_nonblocking(true).unwrap();
    while listener.0.accept().is_ok() {}

    // ── 1. store integrity from the surviving files ─────────────────────────────
    let store = loon_core::open_store(&paths).expect("reopen store after crash");
    let integrity = store.integrity_check().expect("integrity_check");
    assert_eq!(
        integrity, "ok",
        "store must pass integrity_check after kill"
    );

    // ── 2. every event parses; the partial history replays ──────────────────────
    let events = store.events(session_id).expect("read events");
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::SessionStarted { .. })),
        "SessionStarted must have survived"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e.kind,
            EventKind::RequestFinished { .. } | EventKind::TurnFinished { .. }
        )),
        "neither request nor turn can have finished: the process was killed in-flight"
    );
    let history = loon_core::replay::replay_history(&events, None);
    assert!(
        history.iter().any(|m| m.role == loon_providers::Role::User),
        "replayed history must contain the user message"
    );

    // ── 3. resume the crashed session and finish it ─────────────────────────────
    // Fresh silent listener dropped; the resumed agent uses the mock (labeled as such).
    let registry = std::sync::Arc::new(ProviderRegistry::with_mock(
        loon_providers::MockProvider::with_scripts(vec![MockScript::text(
            "recovered and completed after the crash",
        )]),
    ));
    let mut resumed = Agent::resume(
        &paths,
        session_id,
        AgentConfig {
            model_ref: "mock".into(),
            max_steps: 8,
            tool_output_bytes: 4096,
            max_retries: 0,
            allow_retry_after_partial_stream: false,
            permissions: Default::default(),
            context_window: 128_000,
            extra_tools: Vec::new(),
        },
        registry,
        ToolRegistry::standard(),
    )
    .expect("resume crashed session");
    let result = futures_block_on(resumed.run_turn("continue after the crash"));
    assert!(
        matches!(result.status, loon_core::TurnStatus::Completed),
        "resumed turn must complete, got {:?}",
        result.status
    );
    assert_eq!(
        result.assistant_text,
        "recovered and completed after the crash"
    );

    // The journal survived both the crash and the resumed turn, in order.
    let after = store.events(session_id).expect("read events after resume");
    let seqs: Vec<u64> = after.iter().map(|e| e.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "event sequence must remain monotonic");
}

/// Tiny single-thread executor: the test body is sync and this keeps it honest about
/// blocking on one future.
fn futures_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}
