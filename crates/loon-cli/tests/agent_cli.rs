use std::process::Command;
#[test]
fn headless_defaults_still_work_and_new_flags_are_documented() {
    let home = tempfile::tempdir().unwrap();
    let normal = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["run", "hello"])
        .output()
        .unwrap();
    assert!(
        normal.status.success(),
        "{}",
        String::from_utf8_lossy(&normal.stderr)
    );
    let help = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--ultraloop"));
    assert!(text.contains("--supervisor-model"));
    assert!(text.contains("--ultraloop-limit"));
}

#[test]
fn sessions_delete_removes_the_session_and_refuses_unknown_ids() {
    use std::process::Command;
    let home = tempfile::tempdir().unwrap();

    // One message creates one session.
    let run = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["run", "hello delete me"])
        .output()
        .unwrap();
    assert!(run.status.success());
    let list = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["sessions", "list"])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&list.stdout).to_string();
    let id = listing
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().next())
        .expect("list must show the session")
        .to_string();

    // Unknown id: named error, exit 1.
    let missing = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["sessions", "export", "01a164c1-0000-7000-8000-000000000000"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no session 01a164c1"),
        "unknown id must be named, got: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    // Typo'd confirmation aborts without deleting.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["sessions", "delete", &id])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"wrong-prefix\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("aborted"));

    // --force deletes for real.
    let del = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["sessions", "delete", &id, "--force"])
        .output()
        .unwrap();
    assert!(del.status.success());
    let list = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["sessions", "list"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("no sessions yet"),
        "the session must be gone"
    );
}

#[test]
fn an_empty_prompt_is_refused_and_creates_no_session() {
    use std::process::Command;
    for args in [vec!["run", ""], vec!["run", "   "]] {
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_loom"))
            .env("LOON_HOME", home.path())
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(64), "usage error for {args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("empty prompt"),
            "the reason must be explicit, got: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let list = Command::new(env!("CARGO_BIN_EXE_loom"))
            .env("LOON_HOME", home.path())
            .args(["sessions", "list"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&list.stdout).contains("no sessions yet"),
            "an empty prompt must not create a session"
        );
    }
}
