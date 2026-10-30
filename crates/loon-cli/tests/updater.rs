//! The updater, exercised against a local HTTP server. No test here touches the network.
//!
//! Each test copies the built `loom` binary into a scratch directory and runs *that* copy, so
//! `current_exe()` resolves inside the temp dir and an install replaces the copy instead of
//! the binary under `target/`. The fake server's release JSON points back at itself, so the
//! download, the digest check and the atomic replace are all real code paths.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Substituted with the server's own base URL when a fixture is served, so the asset URLs
/// always point at the running instance.
const BASE: &str = "{base}";

/// Every triple `build.rs` can produce, so whichever platform runs the suite finds its own
/// asset in the fixture. All of them share one payload and one digest.
const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// The payload every fixture serves. It only has to be identifiable in the installed file.
const PAYLOAD: &[u8] = b"#!/bin/sh\necho fake loom 9.9.9\n";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Release JSON for `tag`, with an asset for every target in [`TARGETS`].
///
/// `include_checksum` controls whether the sibling `.sha256` assets exist, which is how the
/// "a release publishes no digest" case is expressed.
fn release(tag: &str, prerelease: bool, include_checksum: bool) -> serde_json::Value {
    let version = tag.trim_start_matches('v');
    let mut assets = Vec::new();
    for target in TARGETS {
        let name = format!("loom-{version}-{target}");
        assets.push(serde_json::json!({
            "name": name,
            "browser_download_url": format!("{BASE}/download/{name}"),
        }));
        if include_checksum {
            assets.push(serde_json::json!({
                "name": format!("{name}.sha256"),
                "browser_download_url": format!("{BASE}/download/{name}.sha256"),
            }));
        }
    }
    // A tarball is published too, to prove the updater never mistakes an archive for the
    // binary it is about to install.
    assets.push(serde_json::json!({
        "name": format!("loom-{version}-x86_64-unknown-linux-gnu.tar.gz"),
        "browser_download_url": format!("{BASE}/download/loom-{version}-x86_64-unknown-linux-gnu.tar.gz"),
    }));
    serde_json::json!({
        "tag_name": tag,
        "prerelease": prerelease,
        "draft": false,
        "assets": assets,
    })
}

/// A minimal HTTP/1.1 server: one request per connection, `Connection: close`.
struct FakeGitHub {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeGitHub {
    /// `releases` is the JSON array the API returns; `checksum_body` is what a `.sha256`
    /// download returns (None makes that route 404).
    fn start(releases: serde_json::Value, checksum_body: Option<String>) -> FakeGitHub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        // Substitute the placeholder once, against the port that was actually assigned.
        let releases = serde_json::to_string(&releases)
            .unwrap()
            .replace(BASE, &base);
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        serve(stream, &releases, checksum_body.as_deref());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        FakeGitHub {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(mut stream: std::net::TcpStream, releases: &str, checksum: Option<&str>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    // Drain the headers; the updater sends no body.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.trim().is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    // The exact route proves both documented overrides reach the request: a wrong API root
    // or a wrong repository would 404 here instead of quietly passing.
    let (status, body): (&str, Vec<u8>) = if path == "/repos/owner/repo/releases?per_page=10" {
        ("200 OK", releases.as_bytes().to_vec())
    } else if path.ends_with(".sha256") {
        match checksum {
            Some(text) => ("200 OK", text.as_bytes().to_vec()),
            None => ("404 Not Found", b"no checksum published".to_vec()),
        }
    } else if path.starts_with("/download/loom-") && !path.ends_with(".tar.gz") {
        ("200 OK", PAYLOAD.to_vec())
    } else {
        ("404 Not Found", b"unknown route".to_vec())
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// A scratch install: a copy of the built binary the updater is allowed to replace.
///
/// `home` is a *separate* temp dir on purpose. Config/state writes must not depend on the
/// permissions of the install directory, or a test that makes the install read-only would
/// fail while loading config instead of at the write it means to exercise.
struct Install {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
    binary: PathBuf,
}

impl Install {
    fn new() -> Install {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let binary = dir.path().join("loom");
        std::fs::copy(env!("CARGO_BIN_EXE_loom"), &binary).unwrap();
        Install { dir, home, binary }
    }

    fn path(&self) -> &Path {
        &self.binary
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.binary).unwrap()
    }

    fn home(&self) -> PathBuf {
        self.home.path().to_path_buf()
    }

    fn dot_files(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with('.'))
            .collect()
    }
}

fn run(install: &Install, api_base: &str, args: &[&str]) -> Output {
    Command::new(install.path())
        .env("LOON_HOME", install.home())
        // Point the updater at the fake server through the documented overrides, which is
        // also what makes them a tested contract rather than a claim in the README.
        .env("LOON_UPDATE_API_BASE", api_base)
        .env("LOON_UPDATE_REPOSITORY", "owner/repo")
        .env("LOON_UPDATE", "auto")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn correct_checksum() -> String {
    format!("{}  loom\n", sha256_hex(PAYLOAD))
}

#[test]
fn a_newer_release_is_downloaded_verified_and_installed() {
    let server = FakeGitHub::start(
        serde_json::json!([release("v9.9.9", false, true)]),
        Some(correct_checksum()),
    );
    let install = Install::new();
    let output = run(&install, &server.base(), &["update", "--no-restart"]);
    assert!(
        output.status.success(),
        "update failed: stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("9.9.9"),
        "the command reports what it installed: {}",
        stdout(&output)
    );
    assert_eq!(
        install.bytes(),
        PAYLOAD,
        "the installed file is exactly the verified download"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(install.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "the binary stays executable");
    }
    assert!(
        install.dot_files().is_empty(),
        "no staging file survives: {:?}",
        install.dot_files()
    );
}

#[test]
fn a_checksum_mismatch_refuses_to_replace_the_binary() {
    let server = FakeGitHub::start(
        serde_json::json!([release("v9.9.9", false, true)]),
        // A syntactically valid digest that is not the payload's.
        Some(format!("{}  loom\n", "0".repeat(64))),
    );
    let install = Install::new();
    let original = install.bytes();
    let output = run(&install, &server.base(), &["update", "--no-restart"]);
    assert!(
        !output.status.success(),
        "an unverified download must fail the command"
    );
    let err = stderr(&output);
    assert!(
        err.contains("checksum mismatch"),
        "the error names the cause: {err}"
    );
    assert_eq!(install.bytes(), original, "the working binary is untouched");
    assert!(install.dot_files().is_empty(), "nothing is staged");
}

#[test]
fn a_release_without_a_published_checksum_is_not_installed() {
    let server = FakeGitHub::start(serde_json::json!([release("v9.9.9", false, false)]), None);
    let install = Install::new();
    let original = install.bytes();
    let output = run(&install, &server.base(), &["update", "--no-restart"]);
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(
        err.contains("no checksum") || err.contains("cannot be verified"),
        "the error explains the refusal: {err}"
    );
    assert_eq!(install.bytes(), original);
}

#[test]
fn a_newer_prerelease_is_skipped_by_default_and_used_when_asked_for() {
    let server = FakeGitHub::start(
        serde_json::json!([
            release("v99.0.0", true, true),
            release("v9.9.9", false, true),
        ]),
        Some(correct_checksum()),
    );

    // Default: the stable release is chosen, even though the prerelease is newer and listed
    // first.
    let install = Install::new();
    let output = run(&install, &server.base(), &["update", "--check"]);
    assert!(
        output.status.success(),
        "{} {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("9.9.9") && !out.contains("99.0.0"),
        "the stable release wins: {out}"
    );
    assert_eq!(
        install.bytes(),
        std::fs::read(env!("CARGO_BIN_EXE_loom")).unwrap(),
        "--check installs nothing"
    );

    // With --prerelease the newer prerelease is the answer.
    let output = run(
        &install,
        &server.base(),
        &["update", "--check", "--prerelease"],
    );
    assert!(
        output.status.success(),
        "{} {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("99.0.0"),
        "an explicit prerelease is honoured: {}",
        stdout(&output)
    );
}

#[test]
fn checking_reports_an_available_release_without_installing_it() {
    let server = FakeGitHub::start(
        serde_json::json!([release("v9.9.9", false, true)]),
        Some(correct_checksum()),
    );
    let install = Install::new();
    let original = install.bytes();
    let output = run(&install, &server.base(), &["update", "--check"]);
    let out = stdout(&output);
    assert!(output.status.success(), "{out} {}", stderr(&output));
    assert!(
        out.contains("9.9.9") && out.contains("available"),
        "--check reports rather than installs: {out}"
    );
    assert_eq!(install.bytes(), original);
}

#[test]
fn an_older_release_reports_up_to_date_and_replaces_nothing() {
    let server = FakeGitHub::start(
        serde_json::json!([release("v0.0.1", false, true)]),
        Some(correct_checksum()),
    );
    let install = Install::new();
    let original = install.bytes();
    let output = run(&install, &server.base(), &["update", "--check"]);
    let out = stdout(&output);
    assert!(output.status.success(), "{out} {}", stderr(&output));
    assert!(out.contains("up to date"), "{out}");
    assert_eq!(install.bytes(), original);
}

#[test]
fn an_unreachable_api_fails_the_explicit_command_without_touching_the_binary() {
    // Bind and release a port: connections to it are refused.
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let install = Install::new();
    let original = install.bytes();
    let output = run(
        &install,
        &format!("http://127.0.0.1:{port}"),
        &["update", "--no-restart"],
    );
    assert!(
        !output.status.success(),
        "an unusable release feed is a failure, not a silent success"
    );
    let err = stderr(&output);
    assert!(
        err.contains("not reachable") || err.contains("update check failed"),
        "the error says the feed could not be read: {err}"
    );
    assert_eq!(install.bytes(), original);
}

#[test]
fn the_api_root_override_reaches_the_documented_endpoint() {
    // The fake server answers only on the `/repos/owner/repo/releases` route it recognises;
    // a wrong repository or API root would 404 and the command would fail. A success here
    // proves LOON_UPDATE_API_BASE and LOON_UPDATE_REPOSITORY are both wired through.
    let server = FakeGitHub::start(
        serde_json::json!([release("v9.9.9", false, true)]),
        Some(correct_checksum()),
    );
    let install = Install::new();
    let output = Command::new(install.path())
        .env("LOON_HOME", install.home())
        .env("LOON_UPDATE_API_BASE", server.base())
        .env("LOON_UPDATE_REPOSITORY", "owner/repo")
        .env("LOON_UPDATE", "check")
        .args(["update", "--check"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stdout(&output).contains("9.9.9"));
}

#[test]
fn loom_update_is_documented_in_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["update", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = stdout(&output);
    for flag in ["--check", "--prerelease", "--no-restart"] {
        assert!(text.contains(flag), "`{flag}` must be documented: {text}");
    }
}
/// A binary in a directory this user cannot write — the root-owned `/usr/local/bin` case.
/// The update must fail with an actionable message and leave the working binary in place;
/// it must never attempt a privileged write or a fallback copy the user was not told about.
#[cfg(unix)]
#[test]
fn an_unwritable_install_directory_is_reported_and_the_binary_survives() {
    use std::os::unix::fs::PermissionsExt;

    let server = FakeGitHub::start(
        serde_json::json!([release("v9.9.9", false, true)]),
        Some(correct_checksum()),
    );
    let install = Install::new();
    let original = install.bytes();
    // A directory without the write bit stands in for a root-owned install location.
    std::fs::set_permissions(install.dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // Running as root ignores the bit, so only assert where the permission actually applies.
    let enforced = !nix_is_root();

    let output = run(&install, &server.base(), &["update", "--no-restart"]);
    let err = stderr(&output);
    if enforced {
        assert!(!output.status.success(), "the update must fail: {err}");
        assert!(
            err.contains("not writable"),
            "the error names the permission problem: {err}"
        );
        assert!(
            err.contains("sudo install"),
            "the error hands over a command the user can run: {err}"
        );
        assert!(
            err.contains(&install.path().display().to_string()),
            "the error names the file that could not be replaced: {err}"
        );
        assert!(
            !err.contains("sudo ") || !err.starts_with("loon: sudo"),
            "Loon never runs the privileged command itself: {err}"
        );
    }
    assert_eq!(
        install.bytes(),
        original,
        "the working binary is never half-replaced"
    );
    std::fs::set_permissions(install.dir.path(), std::fs::Permissions::from_mode(0o755)).ok();
}

#[cfg(unix)]
fn nix_is_root() -> bool {
    // Non-destructive: a real syscall, not a shell out to `id`.
    extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() == 0 }
}
