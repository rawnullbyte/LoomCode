//! Verification loop against the real shell runner: baseline → introduced failure,
//! pre-existing failure, and flaky suspicion, all through `bash -lc` in a temp cwd.

use loon_core::verify::{shell_runner, Classification, Verifier};
use std::time::Duration;

fn verifier(script_results: &str) -> Verifier {
    // One command whose pass/fail is controlled by a state file, so each run can
    // differ: fails while flag=fail, passes while flag=pass.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("flag"), script_results).unwrap();
    let cwd = dir.path().to_path_buf();
    // Keep the tempdir alive for the process lifetime of the test.
    std::mem::forget(dir);
    let cmd = format!("cat {0}/flag | grep -q pass", cwd.display());
    Verifier::new(cmd, shell_runner(cwd, Duration::from_secs(30)))
}

#[tokio::test]
async fn shell_runner_reports_success_and_failure() {
    let mut v = verifier("pass");
    let b = v.establish_baseline().await;
    assert!(b.ok, "baseline run: {:?}", b.output_excerpt);
    let (r, cls) = v.verify().await;
    assert!(r.ok);
    assert_eq!(cls, Classification::BaselineGreen);
}

#[tokio::test]
async fn introduced_regression_through_real_shell() {
    let mut v = verifier("pass");
    v.establish_baseline().await;
    // flip the flag: the "edit" broke the check
    let flag = v.command.split(' ').next().unwrap().to_string(); // not used; find flag path below
    let _ = flag;
    let cmd_path = v
        .command
        .split_whitespace()
        .find_map(|tok| tok.strip_suffix("/flag").map(|p| p.to_string()))
        .unwrap();
    std::fs::write(format!("{cmd_path}/flag"), "fail").unwrap();
    let (r, cls) = v.verify().await;
    assert!(!r.ok);
    assert_eq!(cls, Classification::Introduced);
}

#[tokio::test]
async fn timeout_is_a_failure_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let v = Verifier::new("sleep 10", shell_runner(cwd, Duration::from_millis(200)));
    let mut v = v;
    let b = v.establish_baseline().await;
    assert!(!b.ok);
    assert!(b.output_excerpt.contains("timed out"));
}
