use loon_config::{Config, LoonPaths};
use loon_core::{commands::parse, Agent, AgentConfig, AgentEvent};
use loon_providers::{MockProvider, MockScript, MockStep, ProviderRegistry};
use serde_json::json;
use std::{path::Path, sync::Arc};
fn paths(p: &Path) -> LoonPaths {
    LoonPaths {
        config_dir: p.join("config"),
        data_dir: p.join("data"),
        state_dir: p.join("state"),
        cache_dir: p.join("cache"),
        runtime_dir: p.join("runtime"),
    }
}
fn agent(p: &Path, scripts: Vec<MockScript>) -> (LoonPaths, Agent) {
    let paths = paths(p);
    paths.ensure().unwrap();
    let cwd = p.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let a = Agent::new_session(
        &paths,
        AgentConfig::from(&Default::default()),
        Arc::new(ProviderRegistry::with_mock(MockProvider::with_scripts(
            scripts,
        ))),
        loon_core::standard_tools(&paths, &cwd),
        cwd,
        true,
    )
    .unwrap();
    (paths, a)
}
async fn command(a: &mut Agent, s: &str) -> String {
    a.run_slash(parse(s).unwrap().unwrap()).await.unwrap()
}
async fn server(body: &str) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let body = body.to_string();
    let job = tokio::spawn(async move {
        let (mut socket, _) = tcp.accept().await.unwrap();
        let mut bytes = vec![0; 16384];
        let n = socket.read(&mut bytes).await.unwrap();
        let head = String::from_utf8_lossy(&bytes[..n]).into_owned();
        let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
        socket.write_all(response.as_bytes()).await.unwrap();
        head
    });
    (format!("http://{addr}/v1"), job)
}
#[tokio::test]
async fn providers_discover_immediately_without_colliding_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(tmp.path(), vec![]);
    let (url, request) =
        server(r#"{"data":[{"id":"same/model"},{"id":"same/model"},{"id":"other"}]}"#).await;
    let output = command(
        &mut a,
        &format!("/providers add one {url}/responses secret responses - low,medium,high,xhigh,max"),
    )
    .await;
    assert!(output.contains("2 models fetched"), "{output}");
    let head = request.await.unwrap().to_lowercase();
    assert!(head.starts_with("get /v1/models "));
    assert!(head.contains("authorization: bearer secret"));
    assert!(a.registry.resolve("one/same/model").is_ok());
    assert_eq!(
        a.registry.provider("one").unwrap().api,
        loon_config::ApiFamily::Responses
    );
    let (url, request) = server(r#"{"models":["same/model"]}"#).await;
    command(&mut a, &format!("/providers add two {url}")).await;
    request.await.unwrap();
    assert_eq!(
        a.catalog()
            .models
            .iter()
            .filter(|s| s.ends_with("/same/model"))
            .count(),
        2
    );
    command(&mut a, "/model two/same/model").await;
    let (cfg, findings) = Config::load_or_builtin(&paths).unwrap();
    assert!(findings.iter().all(|f| !f.is_error));
    assert_eq!(cfg.session.model, "two/same/model");
    assert_eq!(a.catalog().recent[0], "two/same/model");
}
#[tokio::test]
async fn plans_notes_effort_and_model_survive_resume_and_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(tmp.path(), vec![]);
    let session = a.session_id();
    command(&mut a, "/todos add Inspect the parser").await;
    command(&mut a, "/todos start 1").await;
    command(
        &mut a,
        "/notes add Root cause: parser.rs loses empty fields",
    )
    .await;
    command(&mut a, "/effort ultra").await;
    command(&mut a, "/model mock/mock-1").await;
    command(&mut a, "/goal Fix the parser").await;
    for i in 0..4 {
        a.history.push(loon_providers::ChatMessage::user(
            "long history ".repeat(100),
        ));
        a.history
            .push(loon_providers::ChatMessage::assistant(format!(
                "answer {i}"
            )));
    }
    a.summarizer = Some(Arc::new(|_| {
        Box::pin(async { Ok("Task: fix parser; implementation pending.".into()) })
    }));
    assert!(a.compact_now().await);
    let resumed = Agent::resume(
        &paths,
        session,
        a.cfg.clone(),
        a.registry.clone(),
        a.tools.clone(),
    )
    .unwrap();
    assert_eq!(resumed.effort.as_deref(), Some("ultra"));
    assert_eq!(resumed.cfg.model_ref, "mock/mock-1");
    let state = resumed.worklog.read().unwrap();
    assert_eq!(state.plan[0].status, "in_progress");
    assert!(state.notes[0].contains("parser.rs"));
    assert_eq!(state.goal.as_deref(), Some("Fix the parser"));
    assert!(resumed.estimated_context_tokens() > 0);
}
#[tokio::test]
async fn stream_backpressure_preserves_every_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let steps = (0..2000)
        .map(|i| MockStep::Text(format!("{i},")))
        .collect::<Vec<_>>();
    let expected = (0..2000).map(|i| format!("{i},")).collect::<String>();
    let (_, mut a) = agent(tmp.path(), vec![MockScript::new(steps)]);
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    a.set_event_sink(tx);
    let job = tokio::spawn(async move { a.run_turn("hi").await });
    let mut text = String::new();
    let mut finished = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => text.push_str(&t),
            AgentEvent::TurnFinished { .. } => finished = true,
            _ => {}
        }
    }
    assert!(finished);
    assert_eq!(text, expected);
    assert_eq!(job.await.unwrap().assistant_text, expected);
}
#[tokio::test]
async fn combo_rotation_survives_separate_agent_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(
        tmp.path(),
        vec![MockScript::text("first"), MockScript::text("second")],
    );
    let mut cfg = Config::builtin();
    cfg.providers
        .insert("backup".into(), cfg.providers["mock"].clone());
    cfg.save(&paths).unwrap();
    Arc::make_mut(&mut a.registry).reconfigure(&cfg);
    command(
        &mut a,
        "/combos add pool round_robin mock/mock-1 backup/mock-1",
    )
    .await;
    command(&mut a, "/model combo:pool").await;
    a.run_turn("one").await;
    a.run_turn("two").await;
    let served = a
        .journal
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            loon_events::EventKind::RequestStarted { member, .. } => Some(member.provider_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(served, vec!["mock", "backup"]);
    assert!(a
        .run_slash(parse("/providers remove backup").unwrap().unwrap())
        .await
        .is_err());
}
#[tokio::test]
async fn permission_mode_changes_the_gate_and_plan_validation_is_enforced() {
    use loon_tools::Tool;
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(tmp.path(), vec![]);
    command(&mut a, "/config set permission_mode ask").await;
    assert_eq!(a.permissions.mode, loon_config::PermissionMode::Ask);
    let tool = loon_core::worklog::PlanTool {
        log: a.worklog.clone(),
    };
    assert!(tool.run(json!({"plan":[{"step":"one","status":"in_progress"},{"step":"two","status":"in_progress"}]}),&a.ctx).await.is_err());
}
#[tokio::test]
async fn empty_compaction_keeps_history() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(tmp.path(), vec![]);
    for _ in 0..4 {
        a.history
            .push(loon_providers::ChatMessage::user("must not be lost"));
    }
    a.summarizer = Some(Arc::new(|_| Box::pin(async { Ok(String::new()) })));
    assert!(!a.compact_now().await);
    assert_eq!(a.history.len(), 4);
}

struct GatedRead {
    gate: Arc<tokio::sync::Barrier>,
}
#[async_trait::async_trait]
impl loon_tools::Tool for GatedRead {
    fn spec(&self) -> loon_tools::ToolSpec {
        loon_tools::ToolSpec {
            name: "gated_read".into(),
            description: "Concurrency fixture".into(),
            effects: loon_tools::Effects::ReadOnly,
            parameters: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
        }
    }
    async fn run(
        &self,
        args: serde_json::Value,
        _ctx: &loon_tools::ToolCtx,
    ) -> Result<loon_tools::ToolOutput, loon_tools::ToolError> {
        self.gate.wait().await;
        Ok(loon_tools::ToolOutput::ok(args["value"].as_str().unwrap()))
    }
}
#[tokio::test]
async fn readonly_tools_run_concurrently_and_resume_keeps_each_result_bound_to_its_call() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut a) = agent(
        tmp.path(),
        vec![
            MockScript::new(vec![
                MockStep::tool_call("call_a", "gated_read", json!({"value":"AAA"})),
                MockStep::tool_call("call_b", "gated_read", json!({"value":"BBB"})),
            ]),
            MockScript::text("done"),
        ],
    );
    a.tools = a.tools.clone().with_extra(vec![Arc::new(GatedRead {
        gate: Arc::new(tokio::sync::Barrier::new(2)),
    })]);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), a.run_turn("read both"))
        .await
        .unwrap();
    assert_eq!(result.assistant_text, "done");
    let replay = loon_core::replay::replay_history(&a.journal.events().unwrap(), None);
    let results = replay
        .iter()
        .filter(|m| m.role == loon_providers::Role::Tool)
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_call_id.as_deref(), Some("call_a"));
    assert_eq!(results[0].text, "AAA");
    assert_eq!(results[1].tool_call_id.as_deref(), Some("call_b"));
    assert_eq!(results[1].text, "BBB");
}
#[tokio::test]
async fn full_outputs_can_be_read_in_slices_without_rerunning_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, a) = agent(tmp.path(), vec![]);
    let blob = a.journal.put_blob(b"one\ntwo\nthree\nfour\n").unwrap();
    let tool = a.tools.get("read_output").unwrap();
    let output = tool
        .run(json!({"sha256":blob.sha256,"offset":2,"limit":2}), &a.ctx)
        .await
        .unwrap();
    assert!(output.text.contains("2\ttwo\n3\tthree"));
    assert!(!output.text.contains("4\tfour"));
}

#[tokio::test]
async fn config_accepts_inline_key_value_forms() {
    let tmp = tempfile::tempdir().unwrap();
    let (paths, mut a) = agent(tmp.path(), vec![]);
    command(&mut a, "/config tips=true").await;
    assert!(a.tips);
    command(&mut a, "/config tips = false").await;
    assert!(!a.tips);
    command(&mut a, "/config tips true").await;
    assert!(a.tips);
    command(&mut a, "/config set tips false").await;
    assert!(!a.tips);
    let (cfg, _) = Config::load_or_builtin(&paths).unwrap();
    assert!(!cfg.ui.tips);

    let out = command(&mut a, "/config tool_output_bytes=8192").await;
    assert!(out.contains("8192"), "{out}");
    assert_eq!(a.ctx.output_limit, 8192);

    let err = a
        .run_slash(parse("/config nope=1").unwrap().unwrap())
        .await
        .unwrap_err();
    assert!(err.contains("valid keys"), "{err}");
    assert!(err.contains("tips"), "{err}");
}
