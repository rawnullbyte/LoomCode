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

/// Resume lists the session you last messaged first, and `loom resume` continues that
/// same session instead of starting a new one.
#[test]
fn sessions_are_ordered_by_last_use_and_resume_continues_them() {
    let home = tempfile::tempdir().unwrap();
    let loom = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c
    };
    let session_ids = |out: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(out)
            .lines()
            .skip(1)
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .collect()
    };

    assert!(loom()
        .args(["run", "first"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(loom()
        .args(["run", "second"])
        .output()
        .unwrap()
        .status
        .success());
    let list = loom().args(["sessions", "list"]).output().unwrap();
    let order = session_ids(&list.stdout);
    assert_eq!(order.len(), 2, "both sessions are listed");
    let (second, first) = (order[0].clone(), order[1].clone());
    assert_ne!(first, second);

    // Resuming the older session must not create a third, and must move it to the top.
    let resumed = loom().args(["resume", &first, "third"]).output().unwrap();
    assert!(
        resumed.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let list = loom().args(["sessions", "list"]).output().unwrap();
    let order = session_ids(&list.stdout);
    assert_eq!(
        order.len(),
        2,
        "resume continues a session, it does not create one"
    );
    assert_eq!(
        order[0], first,
        "the resumed session is now the most recent"
    );
    assert_eq!(order[1], second);

    // `LAST USED` is shown so the ordering is self-explanatory.
    let header = String::from_utf8_lossy(&list.stdout);
    assert!(
        header.lines().next().unwrap().contains("LAST USED"),
        "header must name the sort key: {}",
        header.lines().next().unwrap()
    );
}

/// An unknown id and an ambiguous prefix are both refused by name.
#[test]
fn resume_refuses_unknown_and_ambiguous_ids() {
    let home = tempfile::tempdir().unwrap();
    let loom = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c
    };
    let unknown = loom()
        .args(["resume", "01a164c1-0000-7000-8000-000000000000", "x"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("no session 01a164c1"),
        "{}",
        String::from_utf8_lossy(&unknown.stderr)
    );

    // Two sessions sharing a prefix: list them, do not guess.
    for _ in 0..2 {
        assert!(loom()
            .args(["run", "hello"])
            .output()
            .unwrap()
            .status
            .success());
    }
    let list = loom().args(["sessions", "list"]).output().unwrap();
    let text = String::from_utf8_lossy(&list.stdout).to_string();
    let ids: Vec<&str> = text
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(ids.len(), 2);
    // Find the shortest shared prefix of the two ids.
    let shared: String = ids[0]
        .chars()
        .zip(ids[1].chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect();
    if shared.len() >= 8 {
        let ambiguous = loom().args(["resume", &shared, "x"]).output().unwrap();
        assert_eq!(ambiguous.status.code(), Some(1));
        let err = String::from_utf8_lossy(&ambiguous.stderr);
        assert!(err.contains("matches 2 sessions"), "{err}");
        assert!(err.contains(ids[0]), "candidates are listed: {err}");
    }
}

/// An empty (or whitespace-only) prefix matches every session id, because every string
/// starts with "". With exactly one session that silently resumed it — a wrong guess
/// dressed up as a match. An empty selector must be refused, never resolved.
#[test]
fn resume_refuses_an_empty_selector() {
    let home = tempfile::tempdir().unwrap();
    let loom = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c
    };
    // Exactly one session: the dangerous case, where "" has a single "match".
    assert!(loom()
        .args(["run", "only session"])
        .output()
        .unwrap()
        .status
        .success());

    for selector in ["", "   "] {
        let out = loom().args(["resume", selector, "x"]).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(64),
            "an empty selector is a usage error, not a resume (selector {selector:?})"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("empty"),
            "the error must name the problem, got: {err}"
        );
    }

    // The session must be untouched: no phantom turn was run against it.
    let list = loom().args(["sessions", "list"]).output().unwrap();
    let text = String::from_utf8_lossy(&list.stdout);
    assert_eq!(
        text.lines().skip(1).count(),
        1,
        "still exactly one session: {text}"
    );
}

/// `resume` shares `run`'s implementation, so its usage errors must name the command
/// the user actually typed — not the sibling.
#[test]
fn resume_usage_errors_name_resume_not_run() {
    let home = tempfile::tempdir().unwrap();
    let loom = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c
    };
    assert!(loom()
        .args(["run", "seed"])
        .output()
        .unwrap()
        .status
        .success());
    let list = loom().args(["sessions", "list"]).output().unwrap();
    let id = String::from_utf8_lossy(&list.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .expect("one session exists");

    let no_prompt = loom().args(["resume", &id]).output().unwrap();
    assert_eq!(no_prompt.status.code(), Some(64));
    let err = String::from_utf8_lossy(&no_prompt.stderr);
    assert!(
        err.contains("loon resume"),
        "a resume usage error must name resume, got: {err}"
    );
    assert!(
        !err.contains("loon run"),
        "it must not name the sibling command, got: {err}"
    );

    let run = loom().args(["run"]).output().unwrap();
    assert_eq!(run.status.code(), Some(64));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("loon run"),
        "and run still names itself"
    );
}

/// Every session selector accepts a unique prefix: `resume` did, but `export` and
/// `delete` used to reject a prefix with "is not a session id (uuid)", even though
/// `sessions list` shows short ids. One resolver, one behaviour.
#[test]
fn export_and_delete_accept_a_unique_prefix() {
    let home = tempfile::tempdir().unwrap();
    let loom = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c
    };
    assert!(loom()
        .args(["run", "seed"])
        .output()
        .unwrap()
        .status
        .success());
    let list = loom().args(["sessions", "list"]).output().unwrap();
    let full = String::from_utf8_lossy(&list.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .expect("one session exists");
    let prefix = &full[..12];

    let export = loom()
        .args(["sessions", "export", prefix])
        .output()
        .unwrap();
    assert_eq!(
        export.status.code(),
        Some(0),
        "a unique prefix must resolve"
    );
    let out = String::from_utf8_lossy(&export.stdout);
    assert!(
        out.contains(&full),
        "the export header should name the resolved session, got: {out}"
    );

    let delete = loom()
        .args(["sessions", "delete", prefix, "--force"])
        .output()
        .unwrap();
    assert_eq!(
        delete.status.code(),
        Some(0),
        "delete must accept a prefix too"
    );
    assert!(
        String::from_utf8_lossy(&delete.stdout).contains(&full),
        "and report the resolved session"
    );
    assert!(loom()
        .args(["sessions", "export", prefix])
        .output()
        .unwrap()
        .status
        .code()
        .is_some_and(|c| c != 0));
}

/// The delete confirmation must ask for the documented 8-char prefix of the
/// RESOLVED session — not echo the raw selector back. A 3-char prefix resolves
/// fine, but the old code demanded `01a` and rejected the real prefix.
#[test]
fn delete_confirm_uses_the_resolved_ids_prefix_not_the_raw_argument() {
    use std::io::Write as _;
    let home = tempfile::tempdir().unwrap();
    let loom = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c
    };
    assert!(loom()
        .args(["run", "seed"])
        .output()
        .unwrap()
        .status
        .success());
    let list = loom().args(["sessions", "list"]).output().unwrap();
    let full = String::from_utf8_lossy(&list.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .expect("one session exists");
    let tiny = full[..3].to_string();
    let expected = full[..8].to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["sessions", "delete", &tiny])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{expected}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "answering the documented 8-char prefix {expected} must confirm, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = loom().args(["sessions", "list"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("no sessions yet"),
        "the session must actually be gone"
    );
}

/// A multi-line prompt auto-titles the session with its raw text, so a newline in
/// the prompt used to emit a phantom row in `sessions list` — the second line looked
/// like a session with no id. Titles are one line, everywhere they are shown.
#[test]
fn a_multiline_prompt_does_not_break_the_sessions_table() {
    let home = tempfile::tempdir().unwrap();
    let loom = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c
    };
    assert!(loom()
        .args(["run", "first line\nsecond line\nthird line"])
        .output()
        .unwrap()
        .status
        .success());

    let out = loom().args(["sessions", "list"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let rows: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        rows.len(),
        2,
        "one header plus one session row, got: {text:?}"
    );
    assert!(
        rows[1].contains("first line"),
        "the title keeps its first words, got: {rows:1?}"
    );
    assert!(
        !text.contains("second line\nthird line"),
        "the rest of the prompt must not become table rows, got: {text:?}"
    );
}

/// A resumed session runs in the directory it was started in. When that directory
/// is gone the error must say so — it used to blame `--cwd`, a flag the user never
/// typed, sending them hunting for an argument that does not exist.
#[test]
fn resume_names_the_sessions_missing_directory_not_a_cwd_flag() {
    let home = tempfile::tempdir().unwrap();
    let origin = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let loom = |dir: &std::path::Path| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom"));
        c.env("LOON_HOME", home.path());
        c.current_dir(dir);
        c
    };
    assert!(loom(origin.path())
        .args(["run", "seed"])
        .output()
        .unwrap()
        .status
        .success());
    let list = loom(origin.path())
        .args(["sessions", "list"])
        .output()
        .unwrap();
    let id = String::from_utf8_lossy(&list.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .expect("one session exists");

    // The workspace disappears between the original run and the resume.
    let gone = origin.path().to_path_buf();
    drop(origin);
    let out = loom(elsewhere.path())
        .args(["resume", &id, "next"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("loon: --cwd"),
        "resume must not blame a flag the user did not pass, got: {err}"
    );
    assert!(
        err.contains(&gone.display().to_string()),
        "it should name the missing directory, got: {err}"
    );

    // An explicit bad --cwd still reports the flag, because there the user did pass it.
    let explicit = Command::new(env!("CARGO_BIN_EXE_loom"))
        .env("LOON_HOME", home.path())
        .args(["run", "--cwd", "/nope-not-here", "x"])
        .output()
        .unwrap();
    assert_eq!(explicit.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&explicit.stderr).contains("--cwd"));
}
