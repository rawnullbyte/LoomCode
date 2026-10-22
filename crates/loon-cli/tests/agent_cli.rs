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
