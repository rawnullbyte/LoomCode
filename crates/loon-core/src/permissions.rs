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
            "args": truncate(&redact(args_summary), 200),
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

/// Mask credentials before anything is written to the durable audit journal.
/// The journal outlives the session, so a token that reaches it is exposed to
/// anyone who can read `state/audit.jsonl`.
///
/// Only a *key* position is masked: the name must be followed by a separator
/// (`=`, `:`, `"`), which is what distinguishes `"api_key":"…"` from a value
/// that merely mentions the word. Matching bare substrings would corrupt the
/// record — the journal's job is to say what the agent did.
fn redact(s: &str) -> String {
    const KEYS: &[&str] = &[
        "api_key",
        "apikey",
        "api-key",
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "access_key",
        "private_key",
        "credential",
    ];
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let hit = KEYS.iter().find_map(|k| {
            // The key must start a word: the string start, or after a non-alphanumeric
            // char. `_` deliberately counts as a separator: credentials arrive as
            // environment assignments (`OPENAI_API_KEY=`, `DB_PASSWORD=`) where the key
            // is a suffix, and requiring a non-word prefix journaled every one of them
            // in plaintext. Plain identifiers (`mysecret=`) still pass through, which
            // the separator rule below handles.
            let boundary = i == 0 || {
                let prev = s[..i].chars().next_back().unwrap_or(' ');
                !prev.is_alphanumeric()
            };
            // Compare in place rather than indexing a lowercased copy: lowercasing
            // can change byte length ('İ' → 'i̇'), so an offset from `s` is not a
            // valid boundary in the copy. That mismatch both panicked and, worse,
            // silently skipped redaction.
            let end = i + k.len();
            if boundary
                && end <= s.len()
                && s.is_char_boundary(end)
                && s[i..end].eq_ignore_ascii_case(k)
            {
                Some(k.len())
            } else {
                None
            }
        });
        let Some(klen) = hit else {
            let c = s[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        };
        // The key must be followed by a separator, otherwise it is prose or a value.
        let after = i + klen;
        let sep_start = after;
        let mut sep = String::new();
        let mut j = after;
        while j < s.len() {
            let c = s[j..].chars().next().unwrap();
            if c == ' ' || c == '=' || c == ':' || c == '"' || c == '\'' {
                sep.push(c);
                j += 1;
            } else {
                break;
            }
        }
        if sep.is_empty() || !sep.contains(['=', ':']) {
            // Not a key/value shape: copy the key and move on.
            out.push_str(&s[i..sep_start]);
            i = sep_start;
            continue;
        }
        out.push_str(&s[i..sep_start]);
        out.push_str(&sep);
        i = j;
        // `Authorization: Bearer <token>` — the scheme word is not the secret.
        let rest = &s[i..];
        if klen == "authorization".len()
            && rest.len() >= 6
            && rest.is_char_boundary(6)
            && rest[..6].eq_ignore_ascii_case("bearer")
        {
            out.push_str(&rest[..6]);
            i += 6;
            while i < s.len() && s[i..].starts_with(' ') {
                out.push(' ');
                i += 1;
            }
        }
        let mut masked = false;
        while i < s.len() {
            let c = s[i..].chars().next().unwrap();
            if c == '"'
                || c == '\''
                || c == ','
                || c == '}'
                || c == ']'
                || c == '&'
                || c.is_whitespace()
            {
                break;
            }
            if !masked {
                out.push_str("[redacted]");
                masked = true;
            }
            i += c.len_utf8();
        }
    }
    out
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
    fn audit_records_are_truncated() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("audit.jsonl");
        let mut e = engine(PermissionMode::Bypass, vec![], vec![]);
        e.audit_path = Some(path.clone());
        let secret = "s3cr3t-".to_string() + &"x".repeat(400);
        e.audit("sess", "bash", &secret, &Decision::Allowed);
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(line.contains("bash"));
        assert!(!line.contains(&"x".repeat(300)), "long args truncated");
        assert!(line.contains("…") || line.len() < 400);
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    fn engine_with(path: std::path::PathBuf) -> PermissionEngine {
        PermissionEngine {
            mode: PermissionMode::Bypass,
            allow_rules: Vec::new(),
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            audit_path: Some(path),
            audit_lock: Mutex::new(()),
        }
    }

    /// Tool arguments reach the audit journal verbatim. A credential passed to a
    /// tool must not be written to disk in plaintext — the record is durable and
    /// outlives the session.
    #[test]
    fn a_secret_in_tool_arguments_is_not_written_verbatim() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("audit.jsonl");
        let e = engine_with(path.clone());
        let args =
            r#"{"command":"curl -H 'Authorization: Bearer sk-live-ABCDEF123456' https://x"}"#;
        e.audit("sess", "bash", args, &Decision::Allowed);
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(
            !line.contains("sk-live-ABCDEF123456"),
            "a bearer token must not be journaled in plaintext, got: {line}"
        );
        assert!(line.contains("bash"), "the tool name is still recorded");
    }

    /// The same for common key names, which is how most secrets arrive.
    #[test]
    fn a_key_value_secret_is_not_written_verbatim() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("audit.jsonl");
        let e = engine_with(path.clone());
        let args = r#"{"env":{"API_KEY":"sk-test-9f8e7d6c5b4a"},"password":"hunter2"}"#;
        e.audit("sess", "bash", args, &Decision::Allowed);
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(
            !line.contains("sk-test-9f8e7d6c5b4a"),
            "an API key must not be journaled in plaintext, got: {line}"
        );
        assert!(
            !line.contains("hunter2"),
            "a password must not be journaled in plaintext, got: {line}"
        );
    }
}

#[cfg(test)]
mod redaction_false_positive_tests {
    use super::redact;

    /// Redaction must not mangle ordinary arguments: the journal is the record of
    /// what the agent did, and losing that is its own failure.
    #[test]
    fn ordinary_arguments_pass_through() {
        for args in [
            r#"{"path":"src/main.rs","content":"fn main() {}"}"#,
            r#"{"command":"cargo test --workspace"}"#,
            r#"{"pattern":"token","path":"src"}"#,
            r#"{"query":"how does auth work"}"#,
        ] {
            assert_eq!(redact(args), args, "must not alter: {args}");
        }
    }

    /// A secret-looking word used as a plain value is not a credential: masking it
    /// would corrupt the record of what the agent actually did.
    #[test]
    fn a_secret_word_used_as_a_value_is_left_alone() {
        assert_eq!(
            redact(r#"{"field":"password"}"#),
            r#"{"field":"password"}"#,
            "the word alone is not a credential"
        );
    }

    /// Accepting a key that ends a longer identifier (for `OPENAI_API_KEY=`) must not
    /// start eating ordinary paths and prose that merely contain a key word.
    #[test]
    fn a_key_word_inside_a_path_or_phrase_is_left_alone() {
        for args in [
            "src/token.rs",
            "src/api_key.rs",
            "docs/password.md",
            r#"{"note":"rotate the token soon"}"#,
            r#"{"path":"src/secret/mod.rs"}"#,
        ] {
            assert_eq!(redact(args), args, "must not alter: {args}");
        }
    }

    /// A key with no separator is prose, not a credential.
    #[test]
    fn a_key_word_in_prose_is_left_alone() {
        assert_eq!(
            redact(r#"{"note":"rotate the token soon"}"#),
            r#"{"note":"rotate the token soon"}"#
        );
    }
}

#[cfg(test)]
mod redaction_unicode_tests {
    use super::redact;

    /// Lowercasing can change a string's byte length ('İ' is 2 bytes, its
    /// lowercase is 3), so a byte offset taken from the original cannot index
    /// the lowered copy. That mismatch panicked the audit path, which runs on
    /// every tool call — a non-ASCII argument could crash the agent.
    #[test]
    fn a_unicode_argument_does_not_panic() {
        for args in [
            "İ token=abc",
            "{\"note\":\"İstanbul\",\"api_key\":\"sk-1\"}",
            "Übergriff password=hunter2",
            "naïve secret=x",
            "日本語 token=abc",
        ] {
            let out = redact(args);
            assert!(
                !out.contains("sk-1") && !out.contains("hunter2"),
                "still redacts alongside unicode: {out}"
            );
        }
    }
}

#[cfg(test)]
mod redaction_panic_probe {
    use super::redact;

    /// The bare form: a lowercase-changing char before a key shifts the byte
    /// offsets between the original and the lowered copy.
    #[test]
    fn a_lowercase_length_change_before_a_key_does_not_panic() {
        let out = redact("İİİ token=abc");
        assert!(out.contains("[redacted]") || !out.contains("abc"));
    }
}

#[cfg(test)]
mod redaction_unicode_key_tests {
    use super::redact;

    /// The key itself can be non-ASCII-adjacent: the fix must still match the
    /// credential name when a multibyte char sits right before it.
    #[test]
    fn a_key_after_a_multibyte_char_still_redacts() {
        let out = redact("{\"note\":\"日本語\",\"api_key\":\"sk-secret-1\"}");
        assert!(
            !out.contains("sk-secret-1"),
            "a multibyte neighbour must not defeat redaction, got: {out}"
        );
    }
}
#[cfg(test)]
mod redaction_env_shape_tests {
    use super::redact;

    /// Credentials usually reach a tool as environment assignments, where the key
    /// is a *suffix* of a longer name: `OPENAI_API_KEY=...`, `GITHUB_TOKEN=...`.
    /// The boundary rule required the key to start a token, so every one of these
    /// was journaled in plaintext — the shapes a user is most likely to leak.
    #[test]
    fn an_environment_assignment_does_not_leak_its_value() {
        for (args, secret) in [
            ("OPENAI_API_KEY=sk-live-123", "sk-live-123"),
            ("GITHUB_TOKEN=ghp_abc", "ghp_abc"),
            ("export DB_PASSWORD=hunter2", "hunter2"),
            ("MY_SECRET=xyz", "xyz"),
            (r#"{"env":{"ANTHROPIC_API_KEY":"sk-ant-1"}}"#, "sk-ant-1"),
        ] {
            let out = redact(args);
            assert!(
                !out.contains(secret),
                "an env-shaped credential must not be journaled: {args} -> {out}"
            );
        }
    }
}
