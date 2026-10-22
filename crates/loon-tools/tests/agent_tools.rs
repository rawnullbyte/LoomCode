use loon_tools::{Tool, ToolCtx, ToolRegistry};
use serde_json::json;
use std::sync::Arc;
fn context(p: &std::path::Path) -> ToolCtx {
    ToolCtx {
        ws: Arc::new(loon_workspace::Workspace::new(p, vec![]).unwrap()),
        ..Default::default()
    }
}
#[tokio::test]
async fn bash_supports_arrays_and_host_environment_overrides() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = context(t.path());
    ctx.child_env
        .insert("LOOM_TEST_SETTING".into(), "active".into());
    let out=loon_tools::exec::BashTool.run(json!({"command":"values=(hello world); [[ ${values[1]} == world ]] && printf '%s' \"$LOOM_TEST_SETTING\""}),&ctx).await.unwrap();
    assert!(out.ok, "{}", out.text);
    assert!(out.text.contains("active"));
    assert_eq!(out.structured["shell"], "/bin/bash");
}
#[tokio::test]
async fn aliases_normalize_without_losing_canonical_arguments() {
    let t = tempfile::tempdir().unwrap();
    let ctx = context(t.path());
    let registry = ToolRegistry::standard();
    let bash = registry.get("exec_command").unwrap();
    let args = loon_tools::tool::normalize_args(
        &bash.spec(),
        json!({"cmd":"echo incorrect","command":"printf correct","workdir":"."}),
    );
    let out = bash.run(args, &ctx).await.unwrap();
    assert!(out.ok);
    assert!(out.text.contains("correct"));
    assert!(!out.text.contains("incorrect"));
}
#[tokio::test]
async fn managed_jobs_poll_new_output_and_stop() {
    let t = tempfile::tempdir().unwrap();
    let ctx = context(t.path());
    let registry = ToolRegistry::standard();
    let tool = registry.get("process").unwrap();
    let started = tool
        .run(
            json!({"action":"start","command":"printf ready; sleep 20"}),
            &ctx,
        )
        .await
        .unwrap();
    let id = started.structured["job_id"].as_u64().unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let out = tool
                .run(json!({"action":"poll","job_id":id}), &ctx)
                .await
                .unwrap();
            if out.text.contains("ready") {
                break out;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let again = tool
        .run(
            json!({"action":"poll","job_id":id,"cursor":first.structured["cursor"]}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!again.text.contains("ready"));
    tool.run(json!({"action":"stop","job_id":id}), &ctx)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let out = tool
                .run(json!({"action":"poll","job_id":id}), &ctx)
                .await
                .unwrap();
            if out.structured["running"] == false {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn fuzzy_edits_preserve_unicode_offsets() {
    let t = tempfile::tempdir().unwrap();
    let ctx = context(t.path());
    std::fs::write(t.path().join("a"), "// čeština 😀\nlet   中文 = 1;\n").unwrap();
    let out = ToolRegistry::standard()
        .get("edit")
        .unwrap()
        .run(
            json!({"path":"a","old_string":"let 中文 = 1;","new_string":"let 中文 = 2;"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(out.ok);
    assert_eq!(
        std::fs::read_to_string(t.path().join("a")).unwrap(),
        "// čeština 😀\nlet 中文 = 2;\n"
    );
}
#[tokio::test]
async fn patch_is_atomic_on_context_failure_and_undo_metadata_exists() {
    let t = tempfile::tempdir().unwrap();
    let ctx = context(t.path());
    std::fs::write(t.path().join("a"), "one\ntwo\nthree\n").unwrap();
    let patch = ToolRegistry::standard().get("apply_patch").unwrap();
    let failed=patch.run(json!({"path":"a","patch":"@@ -1,1 +1,1 @@\n-one\n+ONE\n@@ -2,1 +2,1 @@\n-missing\n+TWO\n"}),&ctx).await;
    assert!(failed.is_err());
    assert_eq!(
        std::fs::read_to_string(t.path().join("a")).unwrap(),
        "one\ntwo\nthree\n"
    );
    let done = patch
        .run(
            json!({"path":"a","patch":"@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(done.ok);
    assert!(done.structured.get("before_sha256").is_some());
    assert_eq!(
        std::fs::read_to_string(t.path().join("a")).unwrap(),
        "one\nTWO\nthree\n"
    );
}

#[tokio::test]
async fn glob_respects_gitignore_even_without_git_metadata() {
    let t = tempfile::tempdir().unwrap();
    let ctx = context(t.path());
    std::fs::write(t.path().join(".gitignore"), "ignored/\n").unwrap();
    std::fs::create_dir(t.path().join("ignored")).unwrap();
    std::fs::write(t.path().join("ignored/secret.rs"), "x").unwrap();
    std::fs::write(t.path().join("visible.rs"), "x").unwrap();
    let out = ToolRegistry::standard()
        .get("glob")
        .unwrap()
        .run(json!({"pattern":"**/*.rs"}), &ctx)
        .await
        .unwrap();
    assert!(out.text.contains("visible.rs"));
    assert!(!out.text.contains("secret.rs"));
}
#[tokio::test]
async fn fetch_returns_status_and_bounded_retrievable_text() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = socket.read(&mut request).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 12\r\nconnection: close\r\n\r\nhello source").await.unwrap();
    });
    let t = tempfile::tempdir().unwrap();
    let mut ctx = context(t.path());
    ctx.output_limit = 24;
    let out = ToolRegistry::standard()
        .get("fetch")
        .unwrap()
        .run(json!({"url":format!("http://{address}")}), &ctx)
        .await
        .unwrap();
    assert!(out.ok);
    assert!(out.truncated);
    assert!(String::from_utf8(out.full.unwrap())
        .unwrap()
        .contains("hello source"));
    assert_eq!(out.structured["status"], 200);
}
