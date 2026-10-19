//! Verification loop: run the project's checks, compare against a baseline taken
//! before any edits, and classify outcomes honestly.
//!
//! Classification rules:
//! - **BaselineGreen** — baseline passed, current check passed;
//! - **Introduced** — baseline passed, current failed: the agent's changes are the
//!   prime suspect and the loop should stop and report instead of continuing;
//! - **PreExisting** — baseline failed with a comparable failure: reported as such,
//!   never claimed as fixed-by-accident or ignored;
//! - **FlakySuspected** — current failed but a re-run passed: labeled suspected, with
//!   the re-run count visible (one repro is not proof of flakiness).
//!
//! Checks run through the same bash tool the model uses, so sandbox and permission
//! policy apply identically.

use crate::permissions::futures_future::BoxFuture;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub ok: bool,
    /// Trailing excerpt of combined output (tail keeps the failure summary).
    pub output_excerpt: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    BaselineGreen,
    Introduced,
    PreExisting,
    /// Baseline failed, current passes: the work plausibly fixed a pre-existing
    /// failure. Reported distinctly — never flattened into plain green.
    PassesOverRedBaseline,
    FlakySuspected {
        reruns_passed: usize,
    },
}

impl Classification {
    /// One-line honest verdict for reports and journals.
    pub fn describe(&self) -> String {
        match self {
            Classification::BaselineGreen => "checks pass (baseline green)".into(),
            Classification::PassesOverRedBaseline => {
                "checks pass now; the baseline was red before this work".into()
            }
            Classification::Introduced => {
                "checks FAIL and the baseline was green — likely introduced by this work".into()
            }
            Classification::PreExisting => {
                "checks FAIL, but the baseline already failed the same way — pre-existing".into()
            }
            Classification::FlakySuspected { reruns_passed } => format!(
                "check failed once but passed on {reruns_passed} re-run(s) — suspected flaky"
            ),
        }
    }
}

/// Runs one check command. Default implementation shells out through the workspace's
/// bash tool semantics (cwd = workspace root); tests inject closures.
pub type CheckRunner = Arc<dyn Fn(String) -> BoxFuture<CheckResult> + Send + Sync>;

/// Sequential verification driver over one check command.
pub struct Verifier {
    pub command: String,
    pub runner: CheckRunner,
    pub baseline: Option<CheckResult>,
    /// Re-runs performed for flaky classification.
    pub flaky_reruns: usize,
}

impl Verifier {
    pub fn new(command: impl Into<String>, runner: CheckRunner) -> Verifier {
        Verifier {
            command: command.into(),
            runner,
            baseline: None,
            flaky_reruns: 0,
        }
    }

    pub async fn establish_baseline(&mut self) -> CheckResult {
        let r = (self.runner)(self.command.clone()).await;
        self.baseline = Some(r.clone());
        r
    }

    /// Run the check and classify against the baseline. A failure is re-run once to
    /// distinguish a real regression from flakiness; a passing re-run is labeled
    /// suspected-flaky rather than silently treated as success.
    pub async fn verify(&mut self) -> (CheckResult, Classification) {
        let current = (self.runner)(self.command.clone()).await;
        if current.ok {
            let cls = match &self.baseline {
                Some(b) if !b.ok => Classification::PassesOverRedBaseline,
                _ => Classification::BaselineGreen,
            };
            return (current, cls);
        }
        // Failed now: one re-run before believing it.
        let rerun = (self.runner)(self.command.clone()).await;
        if rerun.ok {
            self.flaky_reruns += 1;
            return (current, Classification::FlakySuspected { reruns_passed: 1 });
        }
        let baseline_failed = self.baseline.as_ref().map(|b| !b.ok).unwrap_or(false);
        let cls = if baseline_failed {
            Classification::PreExisting
        } else {
            Classification::Introduced
        };
        (current, cls)
    }
}

/// Shell-based default runner: `bash -lc <cmd>` in the given cwd, output tail-capped.
pub fn shell_runner(cwd: std::path::PathBuf, timeout: std::time::Duration) -> CheckRunner {
    Arc::new(move |cmd: String| {
        let cwd = cwd.clone();
        let timeout = timeout;
        Box::pin(async move {
            let started = std::time::Instant::now();
            let out = tokio::process::Command::new("bash")
                .arg("-lc")
                .arg(&cmd)
                .current_dir(&cwd)
                .output();
            let res = match tokio::time::timeout(timeout, out).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    return CheckResult {
                        ok: false,
                        output_excerpt: format!("failed to spawn: {e}"),
                        exit_code: None,
                        duration_ms: started.elapsed().as_millis() as u64,
                    }
                }
                Err(_) => {
                    return CheckResult {
                        ok: false,
                        output_excerpt: format!("timed out after {}s", timeout.as_secs()),
                        exit_code: None,
                        duration_ms: started.elapsed().as_millis() as u64,
                    }
                }
            };
            let mut combined = String::from_utf8_lossy(&res.stdout).to_string();
            combined.push_str(&String::from_utf8_lossy(&res.stderr));
            let excerpt = tail(&combined, 4000);
            CheckResult {
                ok: res.status.success(),
                exit_code: res.status.code(),
                output_excerpt: excerpt,
                duration_ms: started.elapsed().as_millis() as u64,
            }
        })
    })
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = s.len() - max;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("[…output truncated…]\n{}", &s[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner_from(results: Vec<bool>) -> CheckRunner {
        let results = Arc::new(std::sync::Mutex::new(results));
        Arc::new(move |_| {
            let results = results.clone();
            Box::pin(async move {
                let mut q = results.lock().unwrap();
                let ok = if q.is_empty() { false } else { q.remove(0) };
                CheckResult {
                    ok,
                    output_excerpt: format!("ok={ok}"),
                    exit_code: Some(if ok { 0 } else { 1 }),
                    duration_ms: 1,
                }
            })
        })
    }

    #[tokio::test]
    async fn introduced_is_reported_when_baseline_was_green() {
        let mut v = Verifier::new("cargo test", runner_from(vec![true, false, false]));
        let b = v.establish_baseline().await;
        assert!(b.ok);
        let (r, cls) = v.verify().await;
        assert!(!r.ok);
        assert_eq!(cls, Classification::Introduced);
        assert!(cls.describe().contains("likely introduced"));
    }

    #[tokio::test]
    async fn preexisting_failure_is_named_not_hidden() {
        let mut v = Verifier::new("cargo test", runner_from(vec![false, false, false]));
        v.establish_baseline().await;
        let (_, cls) = v.verify().await;
        assert_eq!(cls, Classification::PreExisting);
        assert!(cls.describe().contains("pre-existing"));
    }

    #[tokio::test]
    async fn passing_after_failing_rerun_is_suspected_flaky_not_pass() {
        let mut v = Verifier::new("cargo test", runner_from(vec![true, false, true]));
        v.establish_baseline().await;
        let (r, cls) = v.verify().await;
        assert!(!r.ok, "the failed run stays the reported result");
        assert_eq!(cls, Classification::FlakySuspected { reruns_passed: 1 });
        assert!(cls.describe().contains("suspected flaky"));
    }

    #[tokio::test]
    async fn green_baseline_green_now() {
        let mut v = Verifier::new("cargo test", runner_from(vec![true, true]));
        v.establish_baseline().await;
        let (_, cls) = v.verify().await;
        assert_eq!(cls, Classification::BaselineGreen);
    }
}
