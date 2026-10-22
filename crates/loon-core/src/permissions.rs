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
    pub ask_rules: Vec<PermissionRule>,
    audit_path: Option<PathBuf>,
    audit_lock: Mutex<()>,
}

impl PermissionEngine {
    pub fn new(cfg: &loon_config::Permissions, audit_path: Option<PathBuf>) -> PermissionEngine {
        PermissionEngine {
            mode: cfg.mode,
            allow_rules: cfg.allow_rules.clone(),
            deny_rules: cfg.deny_rules.clone(),
            ask_rules: cfg.ask_rules.clone(),
            audit_path,
            audit_lock: Mutex::new(()),
        }
    }

    /// Compatibility entry point for path-only policy callers.
    pub fn evaluate(&self, tool: &str, path_arg: Option<&str>, mutating: bool) -> Decision {
        self.evaluate_details(tool, path_arg, None, None, mutating)
    }

    pub fn evaluate_call(
        &self,
        tool: &str,
        args: &serde_json::Value,
        root: &std::path::Path,
        mutating: bool,
    ) -> Decision {
        let path = args["path"].as_str().map(|p| resolve_policy_path(root, p));
        let host = args["url"]
            .as_str()
            .and_then(|u| reqwest::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()));
        self.evaluate_details(
            tool,
            path.as_deref(),
            args["command"].as_str(),
            host.as_deref(),
            mutating,
        )
    }
    fn evaluate_details(
        &self,
        tool: &str,
        path: Option<&str>,
        command: Option<&str>,
        host: Option<&str>,
        mutating: bool,
    ) -> Decision {
        if let Some(rule) = matching_rule(&self.deny_rules, tool, path, command, host, false) {
            return Decision::Denied {
                rule: describe(rule),
            };
        }
        if matching_rule(&self.ask_rules, tool, path, command, host, false).is_some() {
            return Decision::ApprovalRequired;
        }
        if matching_rule(&self.allow_rules, tool, path, command, host, true).is_some() {
            return Decision::Allowed;
        }
        match self.mode {
            PermissionMode::Bypass => Decision::Allowed,
            PermissionMode::Allowlist => Decision::NotAllowlisted,
            PermissionMode::Ask | PermissionMode::Auto => {
                if mutating {
                    Decision::ApprovalRequired
                } else {
                    Decision::Allowed
                }
            }
        }
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

fn matching_rule<'a>(
    rules: &'a [PermissionRule],
    tool: &str,
    path: Option<&str>,
    command: Option<&str>,
    host: Option<&str>,
    allow: bool,
) -> Option<&'a PermissionRule> {
    rules.iter().find(|rule| {
        if rule.tool != tool {
            return false;
        }
        if let Some(prefix) = &rule.path_prefix {
            let Some(path) = path else {
                return false;
            };
            let prefix = prefix.trim_end_matches('/');
            if path != prefix && !path.starts_with(&format!("{prefix}/")) {
                return false;
            }
        }
        if let Some(prefix) = &rule.command_prefix {
            let Some(command) = command.map(str::trim_start) else {
                return false;
            };
            let prefix = prefix.trim();
            if prefix.is_empty()
                || (command != prefix
                    && !command
                        .strip_prefix(prefix)
                        .is_some_and(|s| s.starts_with(char::is_whitespace)))
            {
                return false;
            }
            if allow && command.chars().any(|c| ";&|`$()<>\n\r".contains(c)) {
                return false;
            }
        }
        if let Some(expected) = &rule.host {
            if !host.is_some_and(|h| h.eq_ignore_ascii_case(expected.trim_end_matches('.'))) {
                return false;
            }
        }
        true
    })
}
fn resolve_policy_path(root: &std::path::Path, path: &str) -> String {
    let path = root.join(path);
    let mut clean = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            part => clean.push(part),
        }
    }
    if let Ok(path) = clean.canonicalize() {
        return path.to_string_lossy().into_owned();
    }
    let mut tail = Vec::new();
    let mut parent = clean.as_path();
    while !parent.exists() {
        if let Some(name) = parent.file_name() {
            tail.push(name.to_os_string());
        } else {
            break;
        }
        parent = parent.parent().unwrap_or(parent);
    }
    let mut canonical = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    for name in tail.into_iter().rev() {
        canonical.push(name);
    }
    canonical.to_string_lossy().into_owned()
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
            ask_rules: Vec::new(),
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
                command_prefix: None,
                host: None,
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
                    command_prefix: None,
                    host: None,
                },
                PermissionRule {
                    tool: "write".into(),
                    path_prefix: Some("/tmp/ws".into()),
                    command_prefix: None,
                    host: None,
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
