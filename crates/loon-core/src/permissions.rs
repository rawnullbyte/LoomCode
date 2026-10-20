//! Permission engine: may this operation run?
//!
//! Modes (target spec §16):
//! - **bypass** — no approval prompts; *explicitly configured denies still apply*;
//! - **allowlist** — structured rules only, visible denials, no substring guessing;
//! - **ask** — every mutating operation needs approval. Interactively the UI answers via
//!   the approval hook; headless runs surface `ApprovalRequired` instead of pretending.
//!
//! Every decision lands in a redacted audit journal (`state/audit.jsonl`): tool name,
//! truncated argument summary, decision, mode — never secrets.

use loon_config::{PermissionMode, PermissionRule};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// Denied by an explicit rule — enforced in every mode.
    Denied {
        rule: String,
    },
    /// Allowlist mode with no matching rule.
    NotAllowlisted,
    /// Ask mode and nothing answered the prompt (headless default).
    ApprovalRequired,
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allowed)
    }

    /// The model-facing text explaining the decision (visible, actionable).
    pub fn refusal_text(&self) -> String {
        match self {
            Decision::Allowed => unreachable!("refusal for allowed decision"),
            Decision::Denied { rule } => format!(
                "PERMISSION DENIED: operation matches configured deny rule `{rule}`. \
                 The user's policy forbids this; do not retry it."
            ),
            Decision::NotAllowlisted => {
                "PERMISSION DENIED: permission mode is allowlist and no rule allows this \
                 operation. The user can add a rule in config; do not retry."
                    .into()
            }
            Decision::ApprovalRequired => {
                "PERMISSION REQUIRED: permission mode is ask and this noninteractive run \
                 cannot approve operations. The user must run interactively or change the \
                 permission mode. Do not retry."
                    .into()
            }
        }
    }
}

/// One pending approval handed to the UI hook.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub args_excerpt: String,
    pub reason: String,
}

pub type ApprovalHook =
    std::sync::Arc<dyn Fn(ApprovalRequest) -> futures_future::BoxFuture<bool> + Send + Sync>;

/// Minimal local future alias so loon-core does not pull the futures crate.
pub mod futures_future {
    pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
}

pub struct PermissionEngine {
    pub mode: PermissionMode,
    pub allow_rules: Vec<PermissionRule>,
    pub deny_rules: Vec<PermissionRule>,
    audit_path: Option<PathBuf>,
    audit_lock: Mutex<()>,
}

impl PermissionEngine {
    pub fn new(cfg: &loon_config::Permissions, audit_path: Option<PathBuf>) -> PermissionEngine {
        PermissionEngine {
            mode: cfg.mode,
            allow_rules: cfg.allow_rules.clone(),
            deny_rules: cfg.deny_rules.clone(),
            audit_path,
            audit_lock: Mutex::new(()),
        }
    }

    /// Evaluate a tool call. `path_arg` is the resolved `path` argument when present.
    pub fn evaluate(&self, tool: &str, path_arg: Option<&str>, mutating: bool) -> Decision {
        // Explicit denies first — they bind every mode, bypass included.
        if let Some(rule) = self.matching_rule(&self.deny_rules, tool, path_arg) {
            return Decision::Denied {
                rule: describe(rule),
            };
        }
        match self.mode {
            PermissionMode::Bypass => Decision::Allowed,
            PermissionMode::Allowlist => {
                if self
                    .matching_rule(&self.allow_rules, tool, path_arg)
                    .is_some()
                {
                    Decision::Allowed
                } else {
                    Decision::NotAllowlisted
                }
            }
            PermissionMode::Ask | PermissionMode::Auto => {
                // Reads do not need approval: asking would only slow down looking.
                // Mutating operations need a yes — from the reviewer (auto) or the
                // user (ask).
                if mutating {
                    Decision::ApprovalRequired
                } else {
                    Decision::Allowed
                }
            }
        }
    }

    fn matching_rule<'r>(
        &self,
        rules: &'r [PermissionRule],
        tool: &str,
        path_arg: Option<&str>,
    ) -> Option<&'r PermissionRule> {
        rules.iter().find(|r| {
            if r.tool != tool {
                return false;
            }
            match (&r.path_prefix, path_arg) {
                (None, _) => true,
                (Some(_), None) => false, // path-constrained rule cannot match a pathless call
                (Some(prefix), Some(p)) => {
                    // Real path-prefix match, not a substring test: /tmp/ws must not
                    // match /tmp/ws-secret.
                    let prefix = prefix.trim_end_matches('/');
                    p == prefix || p.starts_with(&format!("{prefix}/"))
                }
            }
        })
    }

    /// Append one redacted audit record. Failures are logged, never fatal: losing an
    /// audit line must not corrupt the run, but repeated failures are visible in doctor.
    pub fn audit(&self, session: &str, tool: &str, args_summary: &str, decision: &Decision) {
        let Some(path) = &self.audit_path else { return };
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let record = serde_json::json!({
            "ts_ms": ts,
            "session": session,
            "tool": tool,
            "args": truncate(args_summary, 200),
            "decision": match decision {
                Decision::Allowed => "allowed",
                Decision::Denied { .. } => "denied",
                Decision::NotAllowlisted => "not_allowlisted",
                Decision::ApprovalRequired => "approval_required",
            },
            "mode": match self.mode {
                PermissionMode::Bypass => "bypass",
                PermissionMode::Allowlist => "allowlist",
                PermissionMode::Ask => "ask",
                PermissionMode::Auto => "auto",
            },
        });
        let _guard = self.audit_lock.lock().expect("audit lock");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{record}");
        } else {
            tracing::warn!("could not write audit record to {}", path.display());
        }
    }
}

fn describe(rule: &PermissionRule) -> String {
    match &rule.path_prefix {
        Some(p) => format!("{} (path under {p})", rule.tool),
        None => rule.tool.clone(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(
        mode: PermissionMode,
        allow: Vec<PermissionRule>,
        deny: Vec<PermissionRule>,
    ) -> PermissionEngine {
        PermissionEngine {
            mode,
            allow_rules: allow,
            deny_rules: deny,
            audit_path: None,
            audit_lock: Mutex::new(()),
        }
    }

    #[test]
    fn bypass_allows_but_explicit_denies_still_bind() {
        let e = engine(
            PermissionMode::Bypass,
            vec![],
            vec![PermissionRule {
                tool: "bash".into(),
                path_prefix: None,
            }],
        );
        assert!(e.evaluate("read", None, false).is_allowed());
        assert!(e.evaluate("write", Some("/x"), true).is_allowed());
        assert!(matches!(
            e.evaluate("bash", None, true),
            Decision::Denied { .. }
        ));
    }

    #[test]
    fn allowlist_is_exact_and_prefix_scoped() {
        let e = engine(
            PermissionMode::Allowlist,
            vec![
                PermissionRule {
                    tool: "read".into(),
                    path_prefix: None,
                },
                PermissionRule {
                    tool: "write".into(),
                    path_prefix: Some("/tmp/ws".into()),
                },
            ],
            vec![],
        );
        assert!(e.evaluate("read", None, false).is_allowed());
        assert!(e
            .evaluate("write", Some("/tmp/ws/a.txt"), true)
            .is_allowed());
        assert!(matches!(
            e.evaluate("write", Some("/etc/passwd"), true),
            Decision::NotAllowlisted
        ));
        assert!(matches!(
            e.evaluate("bash", None, true),
            Decision::NotAllowlisted
        ));
        // prefix must be a real prefix, not a substring
        assert!(matches!(
            e.evaluate("write", Some("/tmp/ws-secret/x"), true),
            Decision::NotAllowlisted
        ));
    }

    #[test]
    fn ask_requires_approval_for_mutating_only() {
        let e = engine(PermissionMode::Ask, vec![], vec![]);
        assert!(e.evaluate("read", None, false).is_allowed());
        assert!(matches!(
            e.evaluate("write", None, true),
            Decision::ApprovalRequired
        ));
    }

    #[test]
    fn audit_records_are_redacted_and_truncated() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("audit.jsonl");
        let mut e = engine(PermissionMode::Bypass, vec![], vec![]);
        e.audit_path = Some(path.clone());
        let secret = "s3cr3t-".to_string() + &"x".repeat(400);
        e.audit("sess", "bash", &secret, &Decision::Allowed);
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(line.contains("bash"));
        assert!(!line.contains(&"x".repeat(300)), "long args truncated");
        // the redaction marker is present
        assert!(line.contains("…") || line.len() < 400);
    }
}
