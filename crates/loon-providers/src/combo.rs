//! Combo resolution: fallback and round-robin execution across members.
//!
//! Hard rules enforced here (target spec §4.5):
//! - capability filtering happens **before** any send; unmet members are skipped with a
//!   visible reason, never silently coerced;
//! - no member switch after the first token was streamed to the user, unless the session
//!   explicitly opted into retry-from-scratch;
//! - credentials are resolved per member origin (see `registry::endpoint_for`) and opaque
//!   state from one member is never replayed to another (each attempt rebuilds from the
//!   same semantic request);
//! - usage is attributed only to the member that actually served;
//! - a dropped stream after partial output is an **unknown** outcome, never silently
//!   retried or re-executed elsewhere;
//! - every attempt arrives at the caller attributed (`AttemptEvent`), so each one is its
//!   own causal/rewind record.

use crate::registry::{CapabilitySnapshot, ProviderRegistry, ResolvedModel};
use crate::{FinishReason, ProviderError, ProviderEvent};
use loon_config::{CapState, Combo, ComboMode, ComboRequires};
use loon_events::{MemberAttempt, RequestOutcome, Usage};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Every provider event, attributed to the member that produced it.
#[derive(Debug, Clone)]
pub struct AttemptEvent {
    pub attempt: MemberAttempt,
    pub event: ProviderEvent,
}

/// Round-robin cursor. Session-scoped by default.
#[derive(Default)]
pub struct RotationState {
    cursors: Mutex<HashMap<String, usize>>,
}

impl RotationState {
    pub fn new() -> RotationState {
        RotationState::default()
    }
    fn next_index(&self, combo: &str, len: usize) -> usize {
        let mut m = self.cursors.lock().expect("rotation lock");
        let c = m.entry(combo.to_string()).or_insert(0);
        let idx = *c % len.max(1);
        *c = (*c + 1) % len.max(1);
        idx
    }
}

#[derive(Debug, Clone)]
pub struct ComboOutcome {
    pub served: Option<MemberAttempt>,
    pub outcome: RequestOutcome,
    pub usage: Usage,
    pub usage_reported: bool,
    /// Members skipped pre-send or failed mid-run, with reasons, in priority order.
    pub trail: Vec<(String, String)>,
    /// True if any partial output reached the user before the request failed.
    pub partial_stream: bool,
}

pub struct ComboRunner {
    pub registry: Arc<ProviderRegistry>,
    pub rotation: Arc<RotationState>,
    /// Per-member retry budget for retriable transport-class errors.
    pub max_retries: u32,
    /// Session opt-in: allow restarting from scratch after a partial stream.
    pub allow_retry_after_partial_stream: bool,
    pub backoff_base: Duration,
}

impl ComboRunner {
    pub fn new(registry: Arc<ProviderRegistry>) -> ComboRunner {
        ComboRunner {
            registry,
            rotation: Arc::new(RotationState::new()),
            max_retries: 2,
            allow_retry_after_partial_stream: false,
            backoff_base: Duration::from_millis(300),
        }
    }

    /// Execute one logical request. `req.tools` non-empty ⇒ tool calling is required.
    pub async fn run(
        &self,
        model_ref: &str,
        req: &crate::ChatRequest,
        tx: mpsc::Sender<AttemptEvent>,
    ) -> Result<ComboOutcome, String> {
        let needs_tools = !req.tools.is_empty();
        match self.registry.resolve(model_ref)? {
            ResolvedModel::Single {
                provider_id,
                model_id,
            } => {
                let member = MemberAttempt {
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                    combo: None,
                    combo_mode: None,
                    rotation_index: None,
                    endpoint_origin: self
                        .registry
                        .endpoint_for(&provider_id)
                        .map(|e| e.origin)
                        .unwrap_or_else(|_| "?".into()),
                    api_family: self.registry.api_family(&provider_id).as_str().to_string(),
                    note: "direct model selection".into(),
                };
                if let Some(reason) = capability_block_reason(
                    &self.registry.capability(&provider_id, &model_id),
                    needs_tools,
                ) {
                    return Err(format!(
                        "model {provider_id}/{model_id} cannot serve this request: {reason}"
                    ));
                }
                let mut runner = MemberRunner {
                    registry: self.registry.clone(),
                    max_retries: self.max_retries,
                    backoff_base: self.backoff_base,
                };
                let (outcome, usage, usage_reported, streamed, _class) =
                    runner.attempt(&member, req, &tx).await;
                Ok(ComboOutcome {
                    served: Some(member),
                    outcome,
                    usage,
                    usage_reported,
                    trail: vec![],
                    partial_stream: streamed,
                })
            }
            ResolvedModel::Combo { name } => {
                let combo = self
                    .registry
                    .combo(&name)
                    .ok_or_else(|| format!("combo `{name}` disappeared"))?
                    .clone();
                if combo.members.is_empty() {
                    return Err(format!("combo `{name}` has no members"));
                }
                self.run_combo(&name, &combo, req, needs_tools, tx).await
            }
        }
    }

    async fn run_combo(
        &self,
        name: &str,
        combo: &Combo,
        req: &crate::ChatRequest,
        needs_tools: bool,
        tx: mpsc::Sender<AttemptEvent>,
    ) -> Result<ComboOutcome, String> {
        // ── pre-send capability filtering over the member list ────────────────────
        let mut trail: Vec<(String, String)> = Vec::new();
        let n = combo.members.len();
        let start = match combo.mode {
            ComboMode::RoundRobin => self.rotation.next_index(name, n),
            ComboMode::Fallback => 0,
        };
        let mut candidates: Vec<MemberAttempt> = Vec::new();
        for offset in 0..n {
            let i = (start + offset) % n;
            let m = &combo.members[i];
            let position_note = format!(
                "combo `{name}` {} position {i}",
                match combo.mode {
                    ComboMode::Fallback => "fallback",
                    ComboMode::RoundRobin => "rotation",
                }
            );
            let endpoint_ok = self.registry.endpoint_for(&m.provider);
            let member = MemberAttempt {
                provider_id: m.provider.clone(),
                model_id: m.model.clone(),
                combo: Some(name.to_string()),
                combo_mode: Some(
                    match combo.mode {
                        ComboMode::Fallback => "fallback",
                        ComboMode::RoundRobin => "round_robin",
                    }
                    .into(),
                ),
                rotation_index: Some(i),
                endpoint_origin: endpoint_ok
                    .as_ref()
                    .map(|e| e.origin.clone())
                    .unwrap_or_else(|_| "?".into()),
                api_family: self.registry.api_family(&m.provider).as_str().to_string(),
                note: position_note,
            };
            let label = format!("{}/{}", m.provider, m.model);
            if let Err(e) = endpoint_ok {
                trail.push((label.clone(), e));
                continue;
            }
            let cap = self.registry.capability(&m.provider, &m.model);
            let ctx = self.registry.context_window(&m.provider, &m.model);
            if let Some(reason) = combo_block_reason(&combo.requires, &cap, ctx) {
                trail.push((label, reason));
                continue;
            }
            if let Some(reason) = capability_block_reason(&cap, needs_tools) {
                trail.push((label, reason));
                continue;
            }
            candidates.push(member);
        }

        // Providers that recently rate-limited us are moved to the back rather than
        // dropped: a fallback chain still works, it just prefers a member that is
        // actually available now.
        candidates.sort_by_key(|m| self.registry.cooldown_remaining(&m.provider_id).is_some());

        if candidates.is_empty() {
            let reasons = trail
                .iter()
                .map(|(m, r)| format!("{m}: {r}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "no combo member can serve this request ({reasons})"
            ));
        }

        // ── attempt members in order ──────────────────────────────────────────────
        let mut served: Option<MemberAttempt> = None;
        let mut streamed_total = false;
        let mut usage = Usage::default();
        let mut usage_reported = false;
        let mut outcome = RequestOutcome::Unknown {
            reason: "not attempted".into(),
        };

        for (pos, member) in candidates.iter().enumerate() {
            let mut runner = MemberRunner {
                registry: self.registry.clone(),
                max_retries: self.max_retries,
                backoff_base: self.backoff_base,
            };
            let (o, u, ur, s, class) = runner.attempt(member, req, &tx).await;
            streamed_total |= s;
            usage = u;
            usage_reported = ur;
            match &o {
                RequestOutcome::Completed { .. }
                | RequestOutcome::Cancelled
                | RequestOutcome::Unknown { .. } => {
                    // Unknown ⇒ the request may have landed remotely; never blind-retry
                    // it against another member. Surface and stop.
                    served = Some(member.clone());
                    outcome = o;
                    break;
                }
                RequestOutcome::Errored { error, .. } => {
                    outcome = o.clone();
                    // A model/endpoint refusal that cannot be helped by another member
                    // stops the combo here — switching would be theater.
                    if class == Some(false) {
                        served = Some(member.clone());
                        break;
                    }
                    if streamed_total && !self.allow_retry_after_partial_stream {
                        served = Some(member.clone());
                        outcome = RequestOutcome::Errored {
                            error: format!(
                                "{error} (partial output was already streamed; combo did not switch members)"
                            ),
                            retriable: false,
                        };
                        break;
                    }
                    if pos + 1 < candidates.len() {
                        let next = &candidates[pos + 1];
                        let _ = tx
                            .send(AttemptEvent {
                                attempt: next.clone(),
                                event: ProviderEvent::Failure {
                                    error: ProviderError::Rejected(format!(
                                        "member {}/{} failed ({error}); combo advancing to next member",
                                        member.provider_id, member.model_id
                                    )),
                                },
                            })
                            .await;
                        continue;
                    }
                    break;
                }
            }
        }

        Ok(ComboOutcome {
            served,
            outcome,
            usage,
            usage_reported,
            trail,
            partial_stream: streamed_total,
        })
    }
}

/// Longest a cooldown may block a request; providers asking for more are retried
/// sooner rather than leaving the turn waiting indefinitely.
const MAX_COOLDOWN_WAIT: Duration = Duration::from_secs(30);

/// Runs one member with bounded same-member retries for retriable transport-class errors.
struct MemberRunner {
    registry: Arc<ProviderRegistry>,
    max_retries: u32,
    backoff_base: Duration,
}

impl MemberRunner {
    /// Returns (outcome, usage, usage_reported, streamed, failure_class).
    /// `failure_class` is Some(switchable) when the attempt ended in a classified error:
    /// only transport/server/rate/auth-class failures may advance a combo.
    async fn attempt(
        &mut self,
        member: &MemberAttempt,
        req: &crate::ChatRequest,
        tx: &mpsc::Sender<AttemptEvent>,
    ) -> (RequestOutcome, Usage, bool, bool, Option<bool>) {
        let mut usage = Usage::default();
        let mut usage_reported = false;
        let mut streamed = false;
        let mut retried = 0u32;

        // Honour a cooldown this provider earned earlier in the session. Bounded so a
        // stale cooldown can never stall a turn indefinitely.
        if let Some(wait) = self.registry.cooldown_remaining(&member.provider_id) {
            let wait = wait.min(MAX_COOLDOWN_WAIT);
            if !wait.is_zero() {
                let _ = tx
                    .send(AttemptEvent {
                        attempt: member.clone(),
                        event: ProviderEvent::Failure {
                            error: ProviderError::Rejected(format!(
                                "{}/{} is cooling down; waiting {}s",
                                member.provider_id,
                                member.model_id,
                                wait.as_secs_f32().ceil()
                            )),
                        },
                    })
                    .await;
                tokio::time::sleep(wait).await;
            }
        }

        loop {
            let (etx, mut erx) = mpsc::channel::<ProviderEvent>(256);
            let registry = self.registry.clone();
            let member_owned = member.clone();
            let member_for_task = member.clone();
            let mut req_owned = req.clone();
            req_owned.model = member.model_id.clone();
            req_owned.reasoning_effort = req
                .reasoning_effort
                .as_deref()
                .and_then(|e| self.registry.reasoning_level(&member.provider_id, e));
            if let Some(cap) = self
                .registry
                .output_limit(&member.provider_id, &member.model_id)
            {
                req_owned.max_output_tokens =
                    Some(req_owned.max_output_tokens.unwrap_or(cap).min(cap));
            }
            let handle = tokio::spawn(async move {
                let member_owned = member_for_task;
                let is_mock = registry.is_mock(&member_owned.provider_id);
                let family = registry.api_family(&member_owned.provider_id);
                match registry.endpoint_for(&member_owned.provider_id) {
                    Err(e) => {
                        let _ = etx
                            .send(ProviderEvent::Failure {
                                error: ProviderError::Connect(e),
                            })
                            .await;
                    }
                    Ok(endpoint) => {
                        let res = if is_mock {
                            registry.mock.stream(&req_owned, &etx).await
                        } else {
                            match family {
                                loon_config::ApiFamily::Chat => {
                                    registry
                                        .chat
                                        .stream(&endpoint, &req_owned, etx.clone())
                                        .await
                                }
                                loon_config::ApiFamily::Responses => {
                                    registry
                                        .responses
                                        .stream(&endpoint, &req_owned, etx.clone())
                                        .await
                                }
                            }
                        };
                        if let Err(e) = res {
                            let _ = etx.send(ProviderEvent::Failure { error: e }).await;
                        }
                    }
                }
            });

            let mut terminal: Option<(RequestOutcome, bool)> = None;
            let mut finished: Option<FinishReason> = None;
            let mut do_retry = false;

            while let Some(ev) = erx.recv().await {
                match &ev {
                    ProviderEvent::TextDelta { .. } | ProviderEvent::ReasoningDelta { .. } => {
                        streamed = true
                    }
                    ProviderEvent::Usage(u) => {
                        usage = u.clone();
                        usage_reported = true;
                    }
                    ProviderEvent::Finished { finish } => finished = Some(*finish),
                    ProviderEvent::Failure { error } => {
                        let (outcome, retry_same, switchable) = classify_failure(error, streamed);
                        // A rate limit is a provider-wide condition, not a one-off: cool
                        // the provider down so the next request (and other combos) do not
                        // walk straight back into it.
                        if matches!(
                            error,
                            ProviderError::RateLimited(_) | ProviderError::Transient { .. }
                        ) {
                            let wait = error.retry_after().unwrap_or_else(|| {
                                self.backoff_base.mul_f64(2f64.powi(retried as i32))
                            });
                            self.registry.note_cooldown(&member.provider_id, wait);
                        }
                        if retry_same && retried < self.max_retries {
                            retried += 1;
                            let delay = error.retry_after().unwrap_or_else(|| {
                                self.backoff_base.mul_f64(2f64.powi(retried as i32 - 1))
                            }) + Duration::from_millis(jitter_ms());
                            let _ = tx
                                .send(AttemptEvent {
                                    attempt: member_owned.clone(),
                                    event: ProviderEvent::Retrying {
                                        reason: format!(
                                            "{error} — retry {retried}/{} in {}s",
                                            self.max_retries,
                                            delay.as_secs_f32().ceil()
                                        ),
                                    },
                                })
                                .await;
                            do_retry = true;
                            streamed = false;
                            break;
                        }
                        terminal = Some((outcome, switchable));
                    }
                    _ => {}
                }
                let _ = tx
                    .send(AttemptEvent {
                        attempt: member_owned.clone(),
                        event: ev,
                    })
                    .await;
            }
            let _ = handle.await; // rejoin the pump task

            if do_retry {
                tokio::time::sleep(
                    self.backoff_base.mul_f64(2f64.powi(retried as i32 - 1))
                        + Duration::from_millis(jitter_ms()),
                )
                .await;
                continue;
            }
            if let Some((o, switchable)) = terminal {
                return (o, usage, usage_reported, streamed, Some(switchable));
            }
            return match finished {
                Some(f) => (
                    RequestOutcome::Completed {
                        finish: format!("{f:?}"),
                    },
                    usage,
                    usage_reported,
                    streamed,
                    None,
                ),
                None => (
                    RequestOutcome::Unknown {
                        reason: "stream ended without completion or failure event".into(),
                    },
                    usage,
                    usage_reported,
                    streamed,
                    None,
                ),
            };
        }
    }
}

/// Map a classified error to (outcome, retry-same-member?, switch-to-next-member?).
/// Same-member retry is only safe before any output was streamed. Model-level
/// rejections and malformed streams never authorize a combo switch except where the
/// endpoint itself is misbehaving (malformed = endpoint bug ⇒ switchable).
fn classify_failure(error: &ProviderError, streamed: bool) -> (RequestOutcome, bool, bool) {
    let retriable = error.retriable();
    let outcome = match error {
        ProviderError::Connect(_) | ProviderError::Timeout => RequestOutcome::Errored {
            error: error.to_string(),
            retriable,
        },
        // A stream dropped after output stays "unknown": the request may have landed
        // remotely, so the combo must not silently repeat it against another member.
        // It is still retried on the *same* member — see `retry_same` below.
        ProviderError::Dropped(_) => {
            if streamed {
                RequestOutcome::Unknown {
                    reason: format!("dropped mid-stream after partial output: {error}"),
                }
            } else {
                RequestOutcome::Errored {
                    error: error.to_string(),
                    retriable,
                }
            }
        }
        ProviderError::RateLimited(_)
        | ProviderError::Transient { .. }
        | ProviderError::Server { .. } => RequestOutcome::Errored {
            error: error.to_string(),
            retriable,
        },
        ProviderError::Auth(_) => RequestOutcome::Errored {
            error: error.to_string(),
            retriable: false,
        },
        ProviderError::Rejected(_) | ProviderError::Malformed(_) => RequestOutcome::Errored {
            error: error.to_string(),
            retriable: false,
        },
    };
    // A dropped stream is worth repeating on the same member: no complete answer was
    // received, and the caller discards whatever the failed attempt streamed. Other
    // errors after output reached the user are not repeated.
    let retry_same = retriable
        && !matches!(error, ProviderError::Auth(_))
        && (!streamed || matches!(error, ProviderError::Dropped(_)));
    let switchable = error.switchable() || matches!(error, ProviderError::Malformed(_));
    (outcome, retry_same, switchable)
}

fn combo_block_reason(
    requires: &ComboRequires,
    cap: &CapabilitySnapshot,
    ctx_window: Option<u64>,
) -> Option<String> {
    // `unknown` is not `supported`: a hard combo requirement demands confirmation.
    if requires.tool_calling && cap.tool_calling != CapState::Supported {
        return Some(
            "combo requires tool calling but member capability is not confirmed supported".into(),
        );
    }
    if requires.vision && cap.vision != CapState::Supported {
        return Some(
            "combo requires vision but member capability is not confirmed supported".into(),
        );
    }
    if let (Some(min), Some(cw)) = (requires.min_context_window, ctx_window) {
        if cw < min {
            return Some(format!(
                "combo requires context >= {min} tokens, member has {cw}"
            ));
        }
    }
    None
}

fn capability_block_reason(cap: &CapabilitySnapshot, needs_tools: bool) -> Option<String> {
    if needs_tools && cap.tool_calling == CapState::Unsupported {
        return Some(
            "request carries tools but this model declares tool calling unsupported".into(),
        );
    }
    None
}

/// Cheap full-precision clock jitter (0..150ms), Send-safe.
fn jitter_ms() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % 150
}

/// JSON view of a combo trail for headless output.
pub fn trail_json(trail: &[(String, String)]) -> serde_json::Value {
    json!(trail
        .iter()
        .map(|(m, r)| json!({"member": m, "reason": r}))
        .collect::<Vec<_>>())
}
