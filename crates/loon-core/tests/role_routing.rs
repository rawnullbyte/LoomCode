//! Role and category model routing through real delegation.
//!
//! These tests configure routing and then inspect what the child session actually ran on, so the
//! assertion is on runtime behaviour rather than on the resolver in isolation (which has its own
//! unit tests in `roles.rs`).

use loon_config::LoonPaths;
use loon_core::roles::{Category, RoutingTable};
use loon_core::{Agent, AgentConfig};
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

fn config(model: &str) -> AgentConfig {
    AgentConfig {
        model_ref: model.into(),
        tool_output_bytes: 65_536,
        max_retries: 0,
        allow_retry_after_partial_stream: false,
        check_command: None,
        ultra_rounds: 0,
        permissions: Default::default(),
        context_window: 128_000,
        extra_tools: Vec::new(),
    }
}

/// An agent with a real delegation tool, so `delegate` is exercised end to end.
///
/// The registry is built from the test configuration (mock provider plus the `cheap`/`strong`
/// aliases), so a routing entry resolves exactly as it would for a user.
fn agent_with_delegation(
    tmp: &std::path::Path,
    ws: &std::path::Path,
    scripts: Vec<MockScript>,
) -> Agent {
    let p = paths(tmp);
    p.ensure().unwrap();
    let cfg = loon_config::Config::load_or_builtin(&p).unwrap().0;
    let registry = Arc::new(
        ProviderRegistry::from_config(&cfg)
            .unwrap()
            .with_mock_scripts(scripts),
    );
    let cfg = config("mock");
    // The host-level toolset: it already carries `delegate`, `background_*` and `task_fanout`, so
    // the test exercises the same delegation tools a real session has.
    let tools = loon_core::subagent::default_tools(&p, ws, &cfg, registry.clone(), None);
    let mut agent = Agent::new_session(&p, cfg, registry, tools, ws.to_path_buf(), true).unwrap();
    // Configuration is written by the test after construction, exactly as a user edits it while a
    // session is open; this is the same refresh a live session performs after a config save.
    agent.refresh_orchestration();
    agent
}

fn write_config(tmp: &std::path::Path, body: &str) {
    let p = paths(tmp);
    p.ensure().unwrap();
    std::fs::create_dir_all(p.config_dir.clone()).unwrap();
    std::fs::write(
        p.config_file(),
        format!(
            "schema_version = 1\n\
             [providers.mock]\n\
             base_url = 'http://mock.invalid/v1'\n\
             api = 'chat'\n\
             mock = true\n\
             [models.mock]\n\
             provider = 'mock'\n\
             id = 'mock-1'\n\
             context_window = 128000\n\
             [models.cheap]\n\
             provider = 'mock'\n\
             id = 'cheap-1'\n\
             [models.strong]\n\
             provider = 'mock'\n\
             id = 'strong-1'\n\
             {body}"
        ),
    )
    .unwrap();
}

/// A category route really changes which model the child runs on, and the child's report says so.
#[tokio::test]
async fn a_category_route_changes_the_child_model() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(tmp.path(), "[routing.models]\nquick = 'cheap'\n");
    let a = agent_with_delegation(tmp.path(), &ws, vec![MockScript::text("explored the tree")]);

    let out = a
        .call_tool_for_test(
            "delegate",
            json!({"agent": "explorer", "task": "find the entry points"}),
        )
        .await;
    assert!(out.contains("explored the tree"), "{out}");
    assert!(
        out.contains("cheap"),
        "the quick category must route to the cheap model: {out}"
    );
    assert!(
        out.contains("category") && out.contains("quick"),
        "the report must explain why: {out}"
    );
}

/// The role wins over its category: a reviewer pinned to a model uses that model even when the
/// category routes elsewhere.
#[tokio::test]
async fn an_explicit_role_model_beats_the_category() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(ws.join(".loon/agents")).unwrap();
    std::fs::write(
        ws.join(".loon/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: strict reviewer\nmodel: strong\ncategory: quick\n---\n\
         Review the change and report defects.\n",
    )
    .unwrap();
    write_config(tmp.path(), "[routing.models]\nquick = 'cheap'\n");
    let a = agent_with_delegation(tmp.path(), &ws, vec![MockScript::text("reviewed")]);
    let out = a
        .call_tool_for_test(
            "delegate",
            json!({"agent": "reviewer", "task": "review the diff"}),
        )
        .await;
    assert!(out.contains("reviewed"), "{out}");
    assert!(
        out.contains("strong"),
        "the role's model must win over the category: {out}"
    );
    assert!(
        !out.contains("model for this child: cheap"),
        "the category route must not win: {out}"
    );
}

/// An explicit `model` argument wins over everything, and it is validated: an unusable reference
/// fails the call rather than delegating to nothing.
#[tokio::test]
async fn an_explicit_model_argument_wins_and_is_validated() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(tmp.path(), "[routing.models]\nquick = 'cheap'\n");
    let a = agent_with_delegation(tmp.path(), &ws, vec![MockScript::text("did it")]);
    let out = a
        .call_tool_for_test(
            "delegate",
            json!({"agent": "explorer", "task": "look around", "model": "strong"}),
        )
        .await;
    assert!(out.contains("strong"), "{out}");
    assert!(!out.contains("model for this child: cheap"), "{out}");

    let bad = a
        .call_tool_for_test(
            "delegate",
            json!({"task": "look around", "model": "does/not-exist"}),
        )
        .await;
    assert!(
        bad.to_lowercase().contains("rejected") || bad.to_lowercase().contains("error"),
        "an unusable model must be refused: {bad}"
    );
}

/// A stale category route is skipped, delegation still runs, and the reason is reported.
#[tokio::test]
async fn an_unresolvable_route_is_skipped_and_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(tmp.path(), "[routing.models]\nquick = 'gone/model'\n");
    let a = agent_with_delegation(tmp.path(), &ws, vec![MockScript::text("still worked")]);
    let out = a
        .call_tool_for_test(
            "delegate",
            json!({"agent": "explorer", "task": "look around"}),
        )
        .await;
    assert!(
        out.contains("still worked"),
        "delegation must not fail: {out}"
    );
    assert!(
        out.contains("gone/model"),
        "the skipped route must be reported: {out}"
    );
}

/// Unknown role names are refused with the real list, so the model corrects itself instead of
/// retrying the same call.
#[tokio::test]
async fn an_unknown_role_lists_the_real_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(tmp.path(), "");
    let a = agent_with_delegation(tmp.path(), &ws, vec![MockScript::text("x")]);
    let out = a
        .call_tool_for_test("delegate", json!({"agent": "wizard", "task": "do magic"}))
        .await;
    assert!(out.contains("wizard"), "{out}");
    for role in ["reviewer", "explorer", "debugger", "security"] {
        assert!(out.contains(role), "{out} omits `{role}`");
    }
}

/// A read-only role's child has no mutating tools registered at all, so even an invented call
/// cannot write.
#[tokio::test]
async fn a_read_only_role_child_has_no_mutating_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(tmp.path(), "");
    // The child asks to write; the reviewer role must not have the tool.
    let a = agent_with_delegation(
        tmp.path(),
        &ws,
        vec![
            MockScript::new(vec![loon_providers::MockStep::tool_call(
                "call-1",
                "write",
                json!({"path": "should-not-exist.txt", "content": "x"}),
            )]),
            MockScript::text("reported findings"),
        ],
    );
    let out = a
        .call_tool_for_test(
            "delegate",
            json!({"agent": "reviewer", "task": "review the workspace"}),
        )
        .await;
    assert!(
        !ws.join("should-not-exist.txt").exists(),
        "a read-only role must not write: {out}"
    );
}

/// `/agents` shows the roles, their category, their read-only status and the model each would
/// use — the answer to "which model will a reviewer run on".
#[tokio::test]
async fn the_agents_command_reports_roles_and_routing() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(tmp.path(), "[routing.models]\nquick = 'cheap'\n");
    let mut a = agent_with_delegation(tmp.path(), &ws, vec![]);
    let out = a
        .run_slash(loon_core::commands::SlashCommand::Agents)
        .await
        .unwrap();
    for role in ["reviewer", "explorer", "debugger", "implementer"] {
        assert!(out.contains(role), "{out} omits `{role}`");
    }
    assert!(
        out.contains("read-only"),
        "the write policy must be visible: {out}"
    );
    assert!(
        out.contains("cheap"),
        "the routed model must be visible: {out}"
    );
    assert!(
        out.contains("the `quick` category routes here"),
        "the reason must be visible: {out}"
    );
}

/// Configuration drives the table: an unknown category key is ignored rather than breaking the
/// whole routing table.
#[tokio::test]
async fn an_unknown_category_in_config_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(
        tmp.path(),
        "[routing.models]\nnot-a-category = 'cheap'\nquick = 'strong'\n",
    );
    let a = agent_with_delegation(tmp.path(), &ws, vec![]);
    let (models, _) = a.routing_tables();
    assert_eq!(
        models.get(&Category::Quick).map(String::as_str),
        Some("strong")
    );
    assert_eq!(
        models.len(),
        1,
        "the unknown key must be dropped: {models:?}"
    );
}

/// An invalid effort route is dropped rather than applied as garbage.
#[tokio::test]
async fn an_invalid_effort_route_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    write_config(
        tmp.path(),
        "[routing.effort]\nquick = 'turbo'\ndeep = 'high'\n",
    );
    let a = agent_with_delegation(tmp.path(), &ws, vec![]);
    let (_, effort) = a.routing_tables();
    assert!(!effort.contains_key(&Category::Quick), "{effort:?}");
    assert_eq!(
        effort.get(&Category::Deep).map(String::as_str),
        Some("high")
    );
}

/// The routing table type is public and usable from an embedder, so the resolver is not a private
/// detail of the delegation tool.
#[test]
fn the_routing_table_is_public_and_ordered() {
    let mut table = RoutingTable::new();
    table.insert(Category::Deep, "a".into());
    table.insert(Category::Quick, "b".into());
    assert_eq!(table.len(), 2);
    let (routing, notes) =
        loon_core::roles::resolve(None, Category::Deep, &table, "session", |_| Ok(()));
    assert_eq!(routing.model, "a");
    assert!(notes.is_empty());
}
