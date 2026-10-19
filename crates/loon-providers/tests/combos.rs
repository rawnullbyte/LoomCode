//! Combo acceptance tests (A23) at the resolution layer, fully offline via the mock
//! provider: fallback switching, round-robin rotation, partial-stream protection,
//! capability filtering, credential isolation, per-member usage attribution.

use loon_config::{
    ApiFamily, CapState, CapabilityOverrides, Combo, ComboMember, ComboMode, ComboRequires,
    ModelDef, Provider,
};

use loon_events::RequestOutcome;
use loon_providers::{
    AttemptEvent, ComboRunner, MockScript, MockStep, ProviderError, ProviderEvent, ProviderRegistry,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

fn reg_with(
    providers: &[(&str, bool)],
    models: &[(&str, &str, Option<CapState>)],
    combos: BTreeMap<String, Combo>,
) -> Arc<ProviderRegistry> {
    let mut provs = BTreeMap::new();
    for (name, is_mock) in providers {
        provs.insert(
            name.to_string(),
            Provider {
                base_url: format!("http://{name}.invalid/v1"),
                api: ApiFamily::Chat,
                api_key_env: None,
                headers: Default::default(),
                timeout_secs: 5,
                models: vec![],
                mock: *is_mock,
            },
        );
    }
    let mut mods = BTreeMap::new();
    for (alias, provider, tool_cap) in models {
        mods.insert(
            alias.to_string(),
            ModelDef {
                provider: provider.to_string(),
                id: alias.to_string(),
                context_window: Some(32_000),
                max_output_tokens: None,
                capabilities: CapabilityOverrides {
                    tool_calling: *tool_cap,
                    ..Default::default()
                },
            },
        );
    }
    let mut cfg = loon_config::Config::builtin();
    cfg.providers = provs;
    cfg.models = mods;
    cfg.combos = combos;
    Arc::new(ProviderRegistry::from_config(&cfg).expect("registry"))
}

fn fallback_combo_tuple(name: &str, members: &[(&str, &str)]) -> (String, Combo) {
    (
        name.to_string(),
        Combo {
            mode: ComboMode::Fallback,
            members: members
                .iter()
                .map(|(p, m)| ComboMember {
                    provider: p.to_string(),
                    model: m.to_string(),
                    max_retries: None,
                })
                .collect(),
            requires: ComboRequires::default(),
            cost_ceiling: None,
            persistent_rotation: false,
        },
    )
}

async fn run(
    registry: &Arc<ProviderRegistry>,
    model_ref: &str,
    tools: bool,
) -> (loon_providers::ComboOutcome, Vec<AttemptEvent>) {
    let mut runner = ComboRunner::new(registry.clone());
    runner.max_retries = 0; // deterministic: no same-member retry noise in these tests
    let req = loon_providers::ChatRequest {
        model: model_ref.into(),
        messages: vec![loon_providers::ChatMessage::user("go")],
        tools: if tools {
            vec![serde_json::json!({"type":"function","function":{"name":"read","parameters":{}}})]
        } else {
            vec![]
        },
        ..Default::default()
    };
    let (tx, mut rx) = mpsc::channel(256);
    let outcome = runner.run(model_ref, &req, tx).await.expect("combo run");
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (outcome, events)
}

#[tokio::test]
async fn fallback_switches_on_member_failure_and_reports_trail() {
    let mut combos = BTreeMap::new();
    combos.extend([fallback_combo_tuple(
        "daily",
        &[("mocka", "mocka"), ("mockb", "mockb")],
    )]);
    let registry = reg_with(
        &[("mocka", true), ("mockb", true)],
        &[("mocka", "mocka", None), ("mockb", "mockb", None)],
        combos,
    );
    registry
        .mock
        .push_script(MockScript::new(vec![MockStep::FailImmediate(
            ProviderError::Server {
                status: 503,
                message: "down".into(),
            },
        )]));
    registry
        .mock
        .push_script(MockScript::text("served by mockb"));

    let (outcome, events) = run(&registry, "combo:daily", false).await;
    let served = outcome.served.expect("served");
    assert_eq!(served.provider_id, "mockb");
    assert!(events.iter().any(|e| e.attempt.provider_id == "mocka"));
    assert!(events.iter().any(|e| e.attempt.provider_id == "mockb"
        && matches!(e.event, ProviderEvent::TextDelta { ref text } if text == "served by mockb")));
    assert!(
        matches!(outcome.outcome, RequestOutcome::Completed { .. }),
        "{:?}",
        outcome.outcome
    );
}

#[tokio::test]
async fn fallback_does_not_switch_on_model_refusal() {
    let mut combos = BTreeMap::new();
    combos.extend([fallback_combo_tuple(
        "daily",
        &[("mocka", "mocka"), ("mockb", "mockb")],
    )]);
    let registry = reg_with(
        &[("mocka", true), ("mockb", true)],
        &[("mocka", "mocka", None), ("mockb", "mockb", None)],
        combos,
    );
    registry
        .mock
        .push_script(MockScript::new(vec![MockStep::FailImmediate(
            ProviderError::Rejected("invalid request".into()),
        )]));

    let (outcome, _) = run(&registry, "combo:daily", false).await;
    // A model-produced rejection must NOT advance to the next member.
    assert_eq!(
        outcome.served.as_ref().map(|s| s.provider_id.as_str()),
        Some("mocka")
    );
    assert!(matches!(outcome.outcome, RequestOutcome::Errored { .. }));
}

#[tokio::test]
async fn no_switch_after_partial_stream() {
    let mut combos = BTreeMap::new();
    combos.extend([fallback_combo_tuple(
        "daily",
        &[("mocka", "mocka"), ("mockb", "mockb")],
    )]);
    let registry = reg_with(
        &[("mocka", true), ("mockb", true)],
        &[("mocka", "mocka", None), ("mockb", "mockb", None)],
        combos,
    );
    registry
        .mock
        .push_script(MockScript::new(vec![MockStep::FailMidStream {
            text: "half an answer".into(),
            error: ProviderError::Dropped("connection reset".into()),
        }]));
    registry
        .mock
        .push_script(MockScript::text("should never be consumed"));

    let (outcome, events) = run(&registry, "combo:daily", false).await;
    assert!(outcome.partial_stream);
    assert!(events.iter().any(
        |e| matches!(&e.event, ProviderEvent::TextDelta { text } if text == "half an answer")
    ));
    // second member's script was not consumed
    assert_eq!(
        outcome.served.as_ref().map(|s| s.provider_id.as_str()),
        Some("mocka")
    );
    assert!(
        matches!(&outcome.outcome, RequestOutcome::Unknown { reason } if reason.contains("partial output")),
        "dropped-after-partial is outcome-unknown, got {:?}",
        outcome.outcome
    );
}

#[tokio::test]
async fn switchable_failure_after_partial_stream_stops_combo_without_opt_in() {
    let mut combos = BTreeMap::new();
    combos.extend([fallback_combo_tuple(
        "daily",
        &[("mocka", "mocka"), ("mockb", "mockb")],
    )]);
    let registry = reg_with(
        &[("mocka", true), ("mockb", true)],
        &[("mocka", "mocka", None), ("mockb", "mockb", None)],
        combos,
    );
    registry
        .mock
        .push_script(MockScript::new(vec![MockStep::FailMidStream {
            text: "start of answer".into(),
            error: ProviderError::Server {
                status: 500,
                message: "boom".into(),
            },
        }]));
    registry.mock.push_script(MockScript::text("never"));

    let (outcome, _) = run(&registry, "combo:daily", false).await;
    assert!(outcome.partial_stream);
    assert_eq!(
        outcome.served.as_ref().map(|s| s.provider_id.as_str()),
        Some("mocka")
    );
    assert!(
        matches!(&outcome.outcome, RequestOutcome::Errored { error, .. } if error.contains("partial output")),
        "switchable failure after stream still must not switch without opt-in: {:?}",
        outcome.outcome
    );
}

#[tokio::test]
async fn round_robin_rotates_per_request() {
    let mut combos = BTreeMap::new();
    combos.insert(
        "rr".to_string(),
        Combo {
            mode: ComboMode::RoundRobin,
            members: vec![
                ComboMember {
                    provider: "mocka".into(),
                    model: "mocka".into(),
                    max_retries: None,
                },
                ComboMember {
                    provider: "mockb".into(),
                    model: "mockb".into(),
                    max_retries: None,
                },
                ComboMember {
                    provider: "mockc".into(),
                    model: "mockc".into(),
                    max_retries: None,
                },
            ],
            requires: ComboRequires::default(),
            cost_ceiling: None,
            persistent_rotation: false,
        },
    );
    let registry = reg_with(
        &[("mocka", true), ("mockb", true), ("mockc", true)],
        &[
            ("mocka", "mocka", None),
            ("mockb", "mockb", None),
            ("mockc", "mockc", None),
        ],
        combos,
    );
    for name in ["from-a", "from-b", "from-c"] {
        registry.mock.push_script(MockScript::text(name));
    }
    let mut runner = ComboRunner::new(registry.clone());
    runner.max_retries = 0;
    let mut served_seq = Vec::new();
    for _ in 0..3 {
        let req = loon_providers::ChatRequest::default();
        let (tx, _rx) = mpsc::channel(64);
        let outcome = runner.run("combo:rr", &req, tx).await.unwrap();
        served_seq.push(outcome.served.expect("served").provider_id);
    }
    assert_eq!(
        served_seq,
        vec!["mocka", "mockb", "mockc"],
        "rotation order"
    );
}

#[tokio::test]
async fn capability_mismatch_skips_member_with_reason() {
    let mut combos = BTreeMap::new();
    combos.insert(
        "tools".to_string(),
        Combo {
            mode: ComboMode::Fallback,
            members: vec![
                ComboMember {
                    provider: "notools".into(),
                    model: "notools".into(),
                    max_retries: None,
                },
                ComboMember {
                    provider: "mockb".into(),
                    model: "mockb".into(),
                    max_retries: None,
                },
            ],
            requires: ComboRequires::default(),
            cost_ceiling: None,
            persistent_rotation: false,
        },
    );
    let registry = reg_with(
        &[("notools", true), ("mockb", true)],
        &[
            ("notools", "notools", Some(CapState::Unsupported)),
            ("mockb", "mockb", None),
        ],
        combos,
    );
    registry.mock.push_script(MockScript::text("mockb served"));
    let (outcome, events) = run(&registry, "combo:tools", true).await;
    assert_eq!(
        outcome.served.as_ref().map(|s| s.provider_id.as_str()),
        Some("mockb")
    );
    // visible skip reason
    assert!(
        outcome
            .trail
            .iter()
            .any(|(m, r)| m.starts_with("notools/") && r.contains("tool calling")),
        "trail: {:?}",
        outcome.trail
    );
    assert!(
        !events.iter().any(|e| e.attempt.provider_id == "notools"),
        "skipped member never sent"
    );
}

#[tokio::test]
async fn unknown_capability_blocks_hard_combo_requirement() {
    let mut combos = BTreeMap::new();
    combos.insert(
        "strict".to_string(),
        Combo {
            mode: ComboMode::Fallback,
            members: vec![ComboMember {
                provider: "mocka".into(),
                model: "mocka".into(),
                max_retries: None,
            }],
            requires: ComboRequires {
                tool_calling: true,
                ..Default::default()
            },
            cost_ceiling: None,
            persistent_rotation: false,
        },
    );
    // capability Unknown (not confirmed) — a hard requirement must refuse, not risk it.
    let registry = reg_with(&[("mocka", true)], &[("mocka", "mocka", None)], combos);
    let err = {
        let runner = ComboRunner::new(registry.clone());
        let req = loon_providers::ChatRequest {
            messages: vec![],
            tools: vec![
                serde_json::json!({"type":"function","function":{"name":"x","parameters":{}}}),
            ],
            ..Default::default()
        };
        let (tx, _rx) = mpsc::channel(16);
        runner
            .run("combo:strict", &req, tx)
            .await
            .expect_err("unknown capability must fail fast")
    };
    assert!(err.contains("not confirmed supported"), "{err}");
}

#[tokio::test]
async fn credentials_resolve_per_member_origin() {
    // mocka requires an env key that is absent: it must be skipped with that reason,
    // and the fallback (no key needed) serves.
    let mut combos = BTreeMap::new();
    combos.extend([fallback_combo_tuple(
        "daily",
        &[("needskey", "nk"), ("mockb", "mockb")],
    )]);
    let mut provs = BTreeMap::new();
    provs.insert(
        "needskey".into(),
        Provider {
            base_url: "http://needskey.invalid/v1".into(),
            api: ApiFamily::Chat,
            api_key_env: Some("DEFINITELY_NOT_SET_LOON_TEST_KEY".into()),
            headers: Default::default(),
            timeout_secs: 5,
            models: vec![],
            mock: false, // real endpoint shape: its api_key_env must actually resolve
        },
    );
    provs.insert(
        "mockb".into(),
        Provider {
            base_url: "http://mockb.invalid/v1".into(),
            mock: true,
            ..Default::default()
        },
    );
    let mut cfg = loon_config::Config::builtin();
    cfg.providers = provs;
    cfg.models.insert(
        "mockb".into(),
        ModelDef {
            provider: "mockb".into(),
            id: "mockb".into(),
            context_window: None,
            max_output_tokens: None,
            capabilities: Default::default(),
        },
    );
    cfg.combos = combos;
    let registry = Arc::new(ProviderRegistry::from_config(&cfg).unwrap());
    registry.mock.push_script(MockScript::text("mockb served"));

    // endpoint_for must refuse without the env var.
    assert!(registry.endpoint_for("needskey").is_err());

    let runner = ComboRunner::new(registry.clone());
    let req = loon_providers::ChatRequest::default();
    let (tx, _rx) = mpsc::channel(64);
    let outcome = runner.run("combo:daily", &req, tx).await.unwrap();
    assert_eq!(
        outcome.served.as_ref().map(|s| s.provider_id.as_str()),
        Some("mockb")
    );
    assert!(
        outcome
            .trail
            .iter()
            .any(|(m, r)| m.starts_with("needskey/")
                && r.contains("DEFINITELY_NOT_SET_LOON_TEST_KEY")),
        "missing credential is a visible skip reason: {:?}",
        outcome.trail
    );
}

#[tokio::test]
async fn usage_attributed_to_serving_member_only() {
    let mut combos = BTreeMap::new();
    combos.extend([fallback_combo_tuple(
        "daily",
        &[("mocka", "mocka"), ("mockb", "mockb")],
    )]);
    let registry = reg_with(
        &[("mocka", true), ("mockb", true)],
        &[("mocka", "mocka", None), ("mockb", "mockb", None)],
        combos,
    );
    registry
        .mock
        .push_script(MockScript::new(vec![MockStep::FailImmediate(
            ProviderError::Server {
                status: 500,
                message: "boom".into(),
            },
        )]));
    registry.mock.push_script(MockScript::new(vec![
        MockStep::Text("ok".into()),
        MockStep::Usage(loon_providers::mock::usage(42, 7)),
    ]));

    let (outcome, _) = run(&registry, "combo:daily", false).await;
    assert_eq!(
        outcome.served.as_ref().map(|s| s.provider_id.as_str()),
        Some("mockb")
    );
    assert_eq!(
        outcome.usage.input_tokens,
        Some(42),
        "usage comes from the serving member"
    );
    assert!(outcome.usage_reported);
}

#[tokio::test]
async fn dropped_stream_after_output_is_unknown_not_completed() {
    let registry = reg_with(
        &[("mocka", true)],
        &[("mocka", "mocka", None)],
        BTreeMap::new(),
    );
    registry
        .mock
        .push_script(MockScript::new(vec![MockStep::FailMidStream {
            text: "partial".into(),
            error: ProviderError::Dropped("reset by peer".into()),
        }]));
    let (outcome, _) = run(&registry, "mocka", false).await;
    assert!(
        matches!(outcome.outcome, RequestOutcome::Unknown { .. }),
        "partial output + drop = unknown, got {:?}",
        outcome.outcome
    );
}

#[tokio::test]
async fn direct_single_model_capability_guard() {
    let registry = reg_with(
        &[("notools", true)],
        &[("notools", "notools", Some(CapState::Unsupported))],
        BTreeMap::new(),
    );
    let runner = ComboRunner::new(registry);
    let req = loon_providers::ChatRequest {
        messages: vec![],
        tools: vec![serde_json::json!({"type":"function","function":{"name":"x","parameters":{}}})],
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel(8);
    let err = runner
        .run("notools", &req, tx)
        .await
        .expect_err("must refuse pre-send");
    assert!(err.contains("declares tool calling unsupported"), "{err}");
}
