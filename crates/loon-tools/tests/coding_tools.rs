use loon_tools::{Tool, ToolCtx, ToolRegistry};
use serde_json::{json, Value};
use std::sync::Arc;
fn ctx(path: &std::path::Path) -> ToolCtx {
    ToolCtx {
        ws: Arc::new(loon_workspace::Workspace::new(path, vec![]).unwrap()),
        ..Default::default()
    }
}
async fn call(name: &str, args: Value, ctx: &ToolCtx) -> loon_tools::ToolOutput {
    let tool = ToolRegistry::standard().get(name).unwrap();
    loon_tools::validate_args(&tool.spec(), &args).unwrap();
    tool.run(args, ctx).await.unwrap()
}
#[tokio::test]
async fn repository_inspection_is_scoped_and_handles_individual_read_errors() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    std::fs::write(
        d.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(
        d.path().join("lib.rs"),
        "pub fn answer() -> u32 {\n 42\n}\n",
    )
    .unwrap();
    let out = call("project_info", json!({}), &c).await;
    assert!(out.text.contains("Cargo.toml"));
    assert!(out.text.contains("lib.rs"));
    let out = call(
        "read_many",
        json!({"paths":["lib.rs","missing.rs"],"offset":2,"limit":1}),
        &c,
    )
    .await;
    assert!(out.text.contains("2:  42"));
    assert!(out.text.contains("error"));
    let out = call("find_symbol", json!({"symbol":"answer"}), &c).await;
    assert!(out.text.contains("pub fn answer"));
    assert!(out.text.contains("\"line\":1"));
    let out = call("list_directory", json!({"limit":1}), &c).await;
    let value: Value = serde_json::from_str(&out.text).unwrap();
    assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    assert_eq!(value["truncated"], true);
}
#[tokio::test]
async fn json_edits_preserve_neighbors_and_reject_stale_or_outside_writes() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    std::fs::write(
        d.path().join("config.json"),
        r#"{"a/b":{"~name":1},"keep":[1,2]}"#,
    )
    .unwrap();
    let original = std::fs::read(d.path().join("config.json")).unwrap();
    let hash = loon_workspace::sha256_hex(&original);
    let out=call("json_edit",json!({"path":"config.json","pointer":"/a~1b/~0name","action":"set","value":3,"expect_sha256":hash}),&c).await;
    assert!(out.ok);
    assert!(out.structured.get("before_sha256").is_some());
    let value: Value =
        serde_json::from_slice(&std::fs::read(d.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(value["a/b"]["~name"], 3);
    assert_eq!(value["keep"], json!([1, 2]));
    let tool = ToolRegistry::standard().get("json_edit").unwrap();
    assert!(tool.run(json!({"path":"config.json","pointer":"/keep","action":"append","value":4,"expect_sha256":hash}),&c).await.is_err());
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "{}").unwrap();
    assert!(tool
        .run(
            json!({"path":outside.path(),"pointer":"/bad","action":"set","value":1}),
            &c
        )
        .await
        .is_err());
    assert_eq!(std::fs::read_to_string(outside.path()).unwrap(), "{}");
}
#[tokio::test]
async fn notebook_edits_clear_stale_execution_and_keep_other_cells_and_metadata() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    let original = json!({"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"name":"python3"}},"cells":[{"id":"first","cell_type":"code","metadata":{"tag":"keep"},"source":["1+1"],"outputs":[{"output_type":"stream","text":["2"]}],"execution_count":1},{"id":"second","cell_type":"markdown","metadata":{},"source":["keep me"]}]});
    std::fs::write(d.path().join("work.ipynb"), original.to_string()).unwrap();
    let read = call("notebook_read", json!({"path":"work.ipynb","cell":0}), &c).await;
    assert!(read.text.contains("output_count"));
    assert!(!read.text.contains("output_type"));
    call(
        "notebook_edit",
        json!({"path":"work.ipynb","cell":0,"action":"replace","source":"print('hello')\n"}),
        &c,
    )
    .await;
    let value: Value =
        serde_json::from_slice(&std::fs::read(d.path().join("work.ipynb")).unwrap()).unwrap();
    assert_eq!(value["cells"][0]["outputs"], json!([]));
    assert!(value["cells"][0]["execution_count"].is_null());
    assert_eq!(
        value["cells"][0]["metadata"],
        original["cells"][0]["metadata"]
    );
    assert_eq!(value["cells"][1], original["cells"][1]);
    assert_eq!(value["metadata"], original["metadata"]);
    call("notebook_edit",json!({"path":"work.ipynb","cell":2,"action":"insert","source":"new","cell_type":"markdown"}),&c).await;
    let value: Value =
        serde_json::from_slice(&std::fs::read(d.path().join("work.ipynb")).unwrap()).unwrap();
    assert_eq!(value["cells"].as_array().unwrap().len(), 3);
    assert!(value["cells"][2]["id"].as_str().is_some());
}
fn git(root: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[tokio::test]
async fn git_tools_read_revisions_and_quote_literal_paths_without_running_shell_fragments() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    git(d.path(), &["init", "-q"]);
    git(d.path(), &["config", "user.name", "Fixture"]);
    git(d.path(), &["config", "user.email", "fixture@local"]);
    let name = "odd'$(touch INJECTED).txt";
    std::fs::write(d.path().join(name), "first\nsecond\n").unwrap();
    git(d.path(), &["add", "--", name]);
    git(d.path(), &["commit", "-qm", "initial fixture"]);
    std::fs::write(d.path().join(name), "first\nchanged\n").unwrap();
    for (tool, args, expected) in [
        ("git_status", json!({}), "odd"),
        ("git_diff", json!({"path":name}), "changed"),
        ("git_log", json!({"path":name}), "initial fixture"),
        ("git_show", json!({"path":name,"revision":"HEAD"}), "second"),
        ("git_blame", json!({"path":name}), "second"),
        ("git_branches", json!({}), "initial fixture"),
    ] {
        let out = call(tool, args, &c).await;
        assert!(out.ok, "{tool}: {}", out.text);
        assert!(out.text.contains(expected), "{tool}: {}", out.text);
    }
    assert!(!d.path().join("INJECTED").exists());
    assert_eq!(
        std::fs::read_to_string(d.path().join(name)).unwrap(),
        "first\nchanged\n"
    );
}
#[tokio::test]
async fn project_checks_run_actual_tests_and_report_missing_scripts() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    std::fs::create_dir(d.path().join("src")).unwrap();
    std::fs::write(
        d.path().join("Cargo.toml"),
        "[package]\nname='loom_fixture_checks'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(
        d.path().join("src/lib.rs"),
        "#[test] fn works() { assert_eq!(2+2,4); }",
    )
    .unwrap();
    let out = call("run_tests", json!({"timeout_secs":60}), &c).await;
    assert!(out.ok, "{}", out.text);
    assert!(out.text.contains("1 passed"));
    let js = tempfile::tempdir().unwrap();
    std::fs::write(
        js.path().join("package.json"),
        r#"{"scripts":{"build":"echo ok"}}"#,
    )
    .unwrap();
    let tool = loon_tools::checks::CheckTool(loon_tools::checks::Check::Tests);
    assert!(tool
        .command(js.path(), &json!({}))
        .unwrap_err()
        .to_string()
        .contains("no test script"));
}
#[tokio::test]
async fn comparison_reports_a_trailing_newline_change() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    std::fs::write(d.path().join("a"), "same").unwrap();
    std::fs::write(d.path().join("b"), "same\n").unwrap();
    let out = call("file_compare", json!({"left":"a","right":"b"}), &c).await;
    assert!(
        out.text
            .contains("-same\n\\ No newline at end of file\n+same"),
        "{}",
        out.text
    );
}
async fn http_fixture(
    status: &str,
    body: &str,
    extra: &str,
) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let response=format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",body.len());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let n = socket.read(&mut buffer).await.unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(bytes).unwrap()
    });
    (format!("http://{addr}"), server)
}
#[tokio::test]
async fn context7_encodes_queries_authenticates_and_redacts_echoed_keys() {
    use loon_tools::context7::{Context7Tool, Operation, Options};
    for (operation, args, endpoint, keyname, expected) in [
        (
            Operation::Search,
            json!({"query":"how to stream?","libraries":["Next.js","OpenAI"],"version":"16","language":"TypeScript"}),
            "/api/v3/search",
            "library",
            "Next.js",
        ),
        (
            Operation::Resolve,
            json!({"libraryName":"react","query":"state hooks"}),
            "/api/v2/libs/search",
            "libraryName",
            "react",
        ),
        (
            Operation::Query,
            json!({"libraryId":"/vercel/next.js/v16","query":"stream"}),
            "/api/v2/context",
            "libraryId",
            "/vercel/next.js/v16",
        ),
    ] {
        let (url, server) = http_fixture(
            "200 OK",
            r#"{"codeSnippets":[],"infoSnippets":[],"echo":"fixture-context7-key"}"#,
            "",
        )
        .await;
        let tool = Context7Tool {
            operation,
            options: Options {
                base_url: url,
                api_key_env: "TEST_KEY".into(),
                timeout_secs: 2,
                ..Default::default()
            },
        };
        let out = tool.request(&args, "fixture-context7-key").await.unwrap();
        assert!(!out.contains("fixture-context7-key"));
        assert!(out.contains("[redacted]"));
        let request = server.await.unwrap();
        assert!(request
            .to_lowercase()
            .contains("authorization: bearer fixture-context7-key"));
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = reqwest::Url::parse(&format!("http://fixture{target}")).unwrap();
        assert_eq!(url.path(), endpoint);
        assert!(url
            .query_pairs()
            .any(|(k, v)| k == keyname && v == expected));
        assert!(url
            .query_pairs()
            .any(|(k, v)| k == "query" && v == args["query"].as_str().unwrap()));
    }
}
#[tokio::test]
async fn context7_errors_are_actionable_and_never_claim_missing_documentation() {
    use loon_tools::context7::{Context7Tool, Operation, Options};
    for (status, extra, expected) in [
        ("401 Unauthorized", "", "authentication/access"),
        (
            "429 Too Many Requests",
            "Retry-After: 60\r\n",
            "Retry-After: 60",
        ),
    ] {
        let (url, server) =
            http_fixture(status, r#"{"message":"secret-context7-key"}"#, extra).await;
        let tool = Context7Tool {
            operation: Operation::Search,
            options: Options {
                base_url: url,
                api_key_env: "TEST_KEY".into(),
                timeout_secs: 2,
                ..Default::default()
            },
        };
        let error = tool
            .request(&json!({"query":"test"}), "secret-context7-key")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected));
        assert!(!error.contains("secret-context7-key"));
        server.await.unwrap();
    }
    let (url, server) = http_fixture("404 Not Found", "{}", "").await;
    let tool = Context7Tool {
        operation: Operation::Search,
        options: Options {
            base_url: url,
            api_key_env: "LOOM_CONTEXT7_MISSING_KEY_147e978f".into(),
            timeout_secs: 2,
            ..Default::default()
        },
    };
    assert!(tool
        .request(&json!({"query":"unknown"}), "fixture")
        .await
        .unwrap()
        .contains("No matching"));
    server.await.unwrap();
    let error = tool
        .run(json!({"query":"test"}), &ToolCtx::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("LOOM_CONTEXT7_MISSING_KEY_147e978f"));
    assert!(error.contains("/settings"));
}
#[tokio::test]
async fn ui_managed_context7_key_authenticates_without_environment_configuration() {
    use loon_tools::context7::{Context7Tool, Operation, Options};
    let d = tempfile::tempdir().unwrap();
    let (url, server) = http_fixture("200 OK", "{\"results\":[]}", "").await;
    let tool = Context7Tool {
        operation: Operation::Search,
        options: Options {
            base_url: url,
            api_key_env: "LOOM_UI_AUTH_MISSING_ENV_873b".into(),
            api_key: Some("fixture-ui-saved-key".into()),
            ..Default::default()
        },
    };
    let result = tool
        .run(json!({"query":"library docs"}), &ctx(d.path()))
        .await
        .unwrap();
    assert!(result.ok);
    let request = server.await.unwrap();
    assert!(request
        .to_lowercase()
        .contains("authorization: bearer fixture-ui-saved-key"));
    assert!(!result.text.contains("fixture-ui-saved-key"));
}

/// A mutation must report *what changed*, not just that it happened: the transcript
/// renders the tool result verbatim, so the diff has to come from the tool itself.
#[tokio::test]
async fn edit_write_and_multi_edit_report_their_diff() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    std::fs::write(d.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();

    let out = call(
        "edit",
        json!({"path":"a.txt","old_string":"two","new_string":"TWO"}),
        &c,
    )
    .await;
    assert!(out.ok);
    assert!(out.text.contains("--- a.txt"), "diff header: {}", out.text);
    assert!(out.text.contains("-two"), "removal line: {}", out.text);
    assert!(out.text.contains("+TWO"), "addition line: {}", out.text);
    assert!(
        !out.text.contains("-one"),
        "unchanged context stays out of the diff: {}",
        out.text
    );

    // multi_edit reports one diff for the whole transaction.
    let out = call(
        "multi_edit",
        json!({"path":"a.txt","edits":[
            {"old_string":"one","new_string":"ONE"},
            {"old_string":"three","new_string":"THREE"}
        ]}),
        &c,
    )
    .await;
    assert!(out.ok);
    assert!(out.text.contains("+ONE"), "{}", out.text);
    assert!(out.text.contains("+THREE"), "{}", out.text);

    // A fresh write shows the whole file as additions.
    let out = call("write", json!({"path":"b.txt","content":"fresh\n"}), &c).await;
    assert!(out.ok);
    assert!(out.text.contains("+fresh"), "new file diff: {}", out.text);

    // Rewriting identical content adds no diff noise.
    let out = call("write", json!({"path":"b.txt","content":"fresh\n"}), &c).await;
    assert!(out.ok);
    assert!(
        !out.text.contains("--- b.txt"),
        "an unchanged write must not show an empty diff: {}",
        out.text
    );
}

/// `fuzzy_edit` shares the edit implementation, so it must report a diff too — the
/// README promises all mutating file tools do.
#[tokio::test]
async fn fuzzy_edit_reports_its_diff() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx(d.path());
    std::fs::write(d.path().join("f.txt"), "let  x   = 1;\nlet y = 2;\n").unwrap();
    let out = call(
        "fuzzy_edit",
        json!({"path":"f.txt","old_string":"let x = 1;","new_string":"let x = 42;"}),
        &c,
    )
    .await;
    assert!(out.ok, "{}", out.text);
    assert!(out.text.contains("--- f.txt"), "{}", out.text);
    // fuzzy_edit normalizes the replacement's whitespace, so the added line is the
    // normalized form; the point of the assertion is that a diff is present at all.
    assert!(out.text.contains("+let x = 42;"), "{}", out.text);
    assert!(out.text.contains("-let  x   = 1;"), "{}", out.text);
}
