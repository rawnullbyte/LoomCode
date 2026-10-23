//! Persistent-shell, background-monitor and SSH tools: the property under test is
//! *reuse* — state survives between calls instead of a fresh process each time.

use loon_tools::{Tool, ToolCtx, ToolRegistry};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

fn ctx(path: &std::path::Path) -> ToolCtx {
    ToolCtx {
        ws: Arc::new(loon_workspace::Workspace::new(path, vec![]).unwrap()),
        ..Default::default()
    }
}

/// One registry for the whole test binary: tools hold live process state (sessions,
/// monitors, SSH masters), so a fresh registry per call would discard it.
fn registry() -> &'static ToolRegistry {
    static REGISTRY: OnceLock<ToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(ToolRegistry::standard)
}

fn tool(name: &str) -> Arc<dyn Tool> {
    registry()
        .get(name)
        .unwrap_or_else(|| panic!("{name} must be registered"))
}

async fn call(name: &str, args: Value, ctx: &ToolCtx) -> loon_tools::ToolOutput {
    let t = tool(name);
    loon_tools::validate_args(&t.spec(), &args).expect("valid args");
    t.run(args, ctx).await.expect("tool runs")
}

// ── shell sessions ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn shell_session_reuses_state_across_calls() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    std::fs::create_dir_all(d.path().join("sub")).unwrap();

    let out = call(
        "shell_session",
        json!({"action":"start","name":"reuse"}),
        &c,
    )
    .await;
    assert!(out.ok, "{}", out.text);

    // An exported variable persists: a fresh shell per call could not see it.
    let out = call(
        "shell_session",
        json!({"action":"run","name":"reuse","command":"export LOON_MARK=kept; echo set"}),
        &c,
    )
    .await;
    assert!(out.text.contains("set"), "{}", out.text);
    let out = call(
        "shell_session",
        json!({"action":"run","name":"reuse","command":"echo value=$LOON_MARK"}),
        &c,
    )
    .await;
    assert!(
        out.text.contains("value=kept"),
        "exported variable must survive: {}",
        out.text
    );

    // `cd` persists too, and the reported cwd follows it.
    let out = call(
        "shell_session",
        json!({"action":"run","name":"reuse","command":"cd sub && pwd"}),
        &c,
    )
    .await;
    assert!(out.text.contains("sub"), "{}", out.text);
    let out = call("shell_session", json!({"action":"cwd","name":"reuse"}), &c).await;
    assert!(
        out.text.contains("sub"),
        "cwd must reflect the earlier cd: {}",
        out.text
    );

    // Exit status is per call, not per session.
    let out = call(
        "shell_session",
        json!({"action":"run","name":"reuse","command":"exit_code_marker=1; false"}),
        &c,
    )
    .await;
    assert!(!out.ok, "false must report failure: {}", out.text);
    let out = call(
        "shell_session",
        json!({"action":"run","name":"reuse","command":"true"}),
        &c,
    )
    .await;
    assert!(
        out.ok,
        "a later success must not inherit failure: {}",
        out.text
    );

    // The session is listed and closable.
    let out = call("shell_session", json!({"action":"list"}), &c).await;
    assert!(out.text.contains("reuse"), "{}", out.text);
    let out = call(
        "shell_session",
        json!({"action":"close","name":"reuse"}),
        &c,
    )
    .await;
    assert!(out.text.contains("closed"), "{}", out.text);
    let out = call("shell_session", json!({"action":"list"}), &c).await;
    assert!(out.text.contains("no sessions"), "{}", out.text);
}

#[tokio::test]
async fn shell_session_start_is_idempotent_and_run_requires_a_session() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    // Running before starting is an honest error, not a silent new shell.
    let t = tool("shell_session");
    let err = t
        .run(
            json!({"action":"run","name":"nope","command":"echo hi"}),
            &c,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no session"), "{err}");

    let out = call("shell_session", json!({"action":"start","name":"idem"}), &c).await;
    assert!(out.ok);
    let out = call("shell_session", json!({"action":"start","name":"idem"}), &c).await;
    assert!(
        out.text.contains("already running"),
        "a second start reuses the shell: {}",
        out.text
    );
    call("shell_session", json!({"action":"close","name":"idem"}), &c).await;
}

#[tokio::test]
async fn shell_session_timeout_closes_the_session_instead_of_desynchronising() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    call(
        "shell_session",
        json!({"action":"start","name":"timeout"}),
        &c,
    )
    .await;
    let out = call(
        "shell_session",
        json!({"action":"run","name":"timeout","command":"sleep 5","timeout_secs":1}),
        &c,
    )
    .await;
    assert!(!out.ok, "{}", out.text);
    assert!(out.text.contains("TIMEOUT"), "{}", out.text);
    assert_eq!(out.structured["closed"], true);
    // The session is retired, so the next run reports it rather than returning stale bytes.
    let t = tool("shell_session");
    let err = t
        .run(
            json!({"action":"run","name":"timeout","command":"echo hi"}),
            &c,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no session"), "{err}");
}

// ── monitors ───────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn monitor_polls_incrementally_and_reports_exit() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    let out = call(
        "monitor",
        json!({"action":"start","label":"greeter","command":"echo first; sleep 0.2; echo second; exit 3"}),
        &c,
    )
    .await;
    assert!(out.ok, "{}", out.text);
    let id = out.structured["monitor_id"].as_u64().unwrap();

    // The process starts asynchronously, so wait for its first line rather than
    // assuming output exists the instant `start` returns.
    let out = call(
        "monitor",
        json!({"action":"wait_for","monitor_id":id,"pattern":"first","timeout_secs":10}),
        &c,
    )
    .await;
    assert!(out.structured["matched"].as_bool().unwrap(), "{}", out.text);
    // First poll (no cursor) reads from the beginning of the log.
    let out = call("monitor", json!({"action":"poll","monitor_id":id}), &c).await;
    assert!(out.text.contains("first"), "{}", out.text);
    let cursor = out.structured["cursor"].as_u64().unwrap();

    // wait_for finds the later line without re-reading the earlier one.
    let out = call(
        "monitor",
        json!({"action":"wait_for","monitor_id":id,"pattern":"second","timeout_secs":10}),
        &c,
    )
    .await;
    assert!(out.structured["matched"].as_bool().unwrap(), "{}", out.text);
    assert!(out.text.contains("second"), "{}", out.text);

    // Eventually the process is reported as exited with its real code.
    let mut last = out;
    for _ in 0..40 {
        last = call(
            "monitor",
            json!({"action":"poll","monitor_id":id,"cursor":cursor}),
            &c,
        )
        .await;
        if last.structured["running"].as_bool() == Some(false) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(last.structured["exit_code"], 3, "{}", last.text);
}

#[tokio::test]
async fn monitor_wait_for_times_out_honestly_and_matches_nothing_on_exit() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    let out = call(
        "monitor",
        json!({"action":"start","label":"quiet","command":"sleep 30"}),
        &c,
    )
    .await;
    let id = out.structured["monitor_id"].as_u64().unwrap();
    let out = call(
        "monitor",
        json!({"action":"wait_for","monitor_id":id,"pattern":"never","timeout_secs":1}),
        &c,
    )
    .await;
    assert!(!out.ok, "{}", out.text);
    assert_eq!(out.structured["timed_out"], true);
    assert_eq!(out.structured["matched"], false);
    call("monitor", json!({"action":"stop","monitor_id":id}), &c).await;

    // A process that exits without the pattern reports the exit, not a fake match.
    let out = call(
        "monitor",
        json!({"action":"start","label":"short","command":"echo done"}),
        &c,
    )
    .await;
    let id = out.structured["monitor_id"].as_u64().unwrap();
    let out = call(
        "monitor",
        json!({"action":"wait_for","monitor_id":id,"pattern":"absent","timeout_secs":10}),
        &c,
    )
    .await;
    assert_eq!(out.structured["matched"], false, "{}", out.text);
}

#[tokio::test]
async fn monitor_logs_returns_the_retained_tail() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    let out = call(
        "monitor",
        json!({"action":"start","label":"noisy","command":"printf 'a\\nb\\nc\\n'"}),
        &c,
    )
    .await;
    let id = out.structured["monitor_id"].as_u64().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let out = call(
        "monitor",
        json!({"action":"logs","monitor_id":id,"max_bytes":2}),
        &c,
    )
    .await;
    assert!(
        out.text.contains('c'),
        "tail keeps the newest bytes: {}",
        out.text
    );
    assert!(out.text.contains("monitor"), "{}", out.text);
}

// ── ssh ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ssh_reports_a_failed_connection_instead_of_pretending() {
    // Nothing listens on port 1 of localhost: the master cannot be established, and the
    // tool must say so rather than reporting a usable connection.
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    let t = tool("ssh");
    let out = t
        .run(
            json!({"action":"connect","name":"nope","host":"127.0.0.1","port":1}),
            &c,
        )
        .await
        .expect("tool returns a structured failure, not an Err");
    assert!(!out.ok, "must not claim success: {}", out.text);
    assert_eq!(out.structured["connected"], false, "{}", out.text);
    // And no connection is listed afterwards.
    let list = call("ssh", json!({"action":"list"}), &c).await;
    assert!(list.text.contains("no open connections"), "{}", list.text);
}

#[tokio::test]
async fn ssh_run_without_a_connection_is_an_honest_error() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    let t = tool("ssh");
    let err = t
        .run(
            json!({"action":"run","name":"ghost","command":"uptime"}),
            &c,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no connection"), "{err}");
}

#[test]
fn persistent_tools_are_registered_with_useful_specs() {
    for name in ["shell_session", "monitor", "ssh"] {
        let spec = tool(name).spec();
        assert_eq!(spec.name, name);
        assert!(
            spec.description.len() > 80,
            "{name} needs a description the model can act on"
        );
        assert!(spec.parameters["properties"]["action"]["enum"].is_array());
    }
}

#[tokio::test]
async fn aborted_shell_command_is_retired_and_syntax_errors_do_not_desync_it() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    let reg = ToolRegistry::standard();
    let shell = reg.get("shell_session").unwrap();
    shell
        .run(json!({"action":"start","name":"abort"}), &c)
        .await
        .unwrap();
    let out = shell
        .run(
            json!({"action":"run","name":"abort","command":"echo 'unterminated"}),
            &c,
        )
        .await
        .unwrap();
    assert!(!out.ok);
    let out = shell
        .run(
            json!({"action":"run","name":"abort","command":"echo recovered"}),
            &c,
        )
        .await
        .unwrap();
    assert!(out.ok && out.text.contains("recovered"));
    let handle = {
        let tool = shell.clone();
        let c = c.clone();
        tokio::spawn(async move {
            tool.run(
                json!({"action":"run","name":"abort","command":"echo started > started; sleep 20"}),
                &c,
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !d.path().join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    handle.abort();
    let _ = handle.await;
    assert!(shell
        .run(
            json!({"action":"run","name":"abort","command":"echo must-not-run"}),
            &c
        )
        .await
        .is_err());
    let out = shell.run(json!({"action":"list"}), &c).await.unwrap();
    assert!(out.text.contains("no sessions"));
}
