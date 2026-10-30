//! Self-update from GitHub Releases.
//!
//! The contract, in order:
//!
//! 1. Ask the releases API for the release list (`/repos/{repo}/releases`), keep the
//!    newest acceptable one, and stop unless it is newer than the running build.
//! 2. Pick the asset for this platform, and its sibling `.sha256`.
//! 3. Verify the digest **before** the installed binary is touched. A missing or mismatched
//!    checksum aborts the update: an unverified binary is never installed.
//! 4. Replace the binary atomically — temp file in the *same* directory, `fsync`, `rename` —
//!    so an interrupted or failed write leaves the working binary in place.
//! 5. Re-exec the new binary with the same arguments.
//!
//! Privileges are never escalated. The install path is the one the running executable
//! already occupies; if its directory is not writable the update fails with a message that
//! names the path, the reason, and a command the user can run instead. Loon does not run
//! sudo, install a setuid helper, or scatter a second copy somewhere `PATH` will not find.

use loon_config::schema::UpdateCfg;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The target triple this build was compiled for (emitted by `build.rs`). Release assets are
/// named after it, so a hardcoded value would download a binary for the wrong platform.
pub const BUILD_TARGET: &str = env!("LOON_BUILD_TARGET");

/// Set in the environment of a relaunched process so a running update cannot recurse: if the
/// new binary still believes a newer release exists, it must report that instead of
/// installing and exec'ing again.
pub const RELAUNCH_GUARD: &str = "LOON_UPDATE_DONE";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("update check failed: {0}")]
    Network(String),
    #[error("the releases API returned {status}{detail}")]
    Status { status: u16, detail: String },
    #[error("the releases API response could not be read: {0}")]
    Parse(String),
    #[error("release {tag} has no binary for {target}; published assets: {assets}")]
    NoAsset {
        tag: String,
        target: String,
        assets: String,
    },
    #[error("release {tag} publishes no checksum for {asset}, so the download cannot be verified; refusing to install it")]
    MissingChecksum { tag: String, asset: String },
    #[error("checksum mismatch for {asset}: published {expected}, downloaded {actual}; the binary was not replaced")]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error("cannot replace {path}: {reason}. Loon does not run privileged commands; install the new binary yourself with `{hint}`")]
    NotWritable {
        path: String,
        reason: String,
        hint: String,
    },
    #[error("{0}")]
    Io(String),
}

// ── Versions ────────────────────────────────────────────────────────────────────────

/// A parsed `major.minor.patch`. A pre-release or build suffix is dropped: this updater only
/// installs plain releases, so `1.2.3-rc1` compares equal to `1.2.3` and a pre-release never
/// outranks the release it precedes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let core = text.split(['-', '+']).next().unwrap_or(text);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        // `v1.2` is accepted as 1.2.0; `1.2.3.4` is not a version we publish.
        let patch = match parts.next() {
            Some(p) if !p.is_empty() => p.parse().ok()?,
            Some(_) | None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
        })
    }

    /// The running build's version, as compiled in.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is a semver triple")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

// ── Releases ────────────────────────────────────────────────────────────────────────

/// One asset attached to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// The release as this updater needs it; the rest of the API response is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

impl Release {
    /// The raw binary for `target`. Both layouts the release workflow can produce are
    /// accepted: the versioned name (`loom-0.2.0-x86_64-unknown-linux-gnu`) and the bare
    /// target (`loom-x86_64-unknown-linux-gnu`). Archives are skipped on purpose — the
    /// updater replaces a binary and will not unpack something it only verified as a tarball.
    pub fn binary_asset(&self, target: &str) -> Option<&Asset> {
        let versioned = format!("loom-{}-{target}", self.version);
        let bare = format!("loom-{target}");
        [versioned, bare]
            .iter()
            .find_map(|name| self.assets.iter().find(|a| &a.name == name))
    }

    /// The published digest for `asset`, taken from its sibling `.sha256` asset.
    pub fn checksum_asset(&self, asset: &Asset) -> Option<&Asset> {
        let wanted = format!("{}.sha256", asset.name);
        self.assets.iter().find(|a| a.name == wanted)
    }
}

/// What a check decided. Every variant here is a *successful* check: "no update" is not an
/// error, and an unreachable feed is reported as its own outcome so the interactive launch
/// can stay quiet on a machine that is simply offline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The running build is the newest release.
    UpToDate { current: Version, latest: Version },
    /// A newer release exists, but this run only reports it.
    Available { current: Version, release: Release },
    /// A newer release was installed at `path`.
    Installed {
        current: Version,
        release: Release,
        path: PathBuf,
    },
    /// Checked too recently; the cached answer is still fresh.
    Cached { latest: Version, age: Duration },
    /// Offline, rate limited, or the release feed is unavailable.
    Unreachable { reason: String },
}

impl Outcome {
    /// One line for the interactive startup notice, or None when there is nothing worth
    /// interrupting the user for. `Unreachable` is deliberately silent here: the caller may
    /// print it in an explicit `loom update`, but a launch must not nag about a bad network.
    pub fn notice(&self) -> Option<String> {
        match self {
            Outcome::UpToDate { .. } | Outcome::Cached { .. } | Outcome::Unreachable { .. } => None,
            Outcome::Available { current, release } => Some(format!(
                "loom {} is available (running {current}); run `loom update` to install it",
                release.version
            )),
            Outcome::Installed {
                current,
                release,
                path,
            } => Some(format!(
                "updated loom {current} → {} at {}",
                release.version,
                path.display()
            )),
        }
    }
}

// ── Settings ────────────────────────────────────────────────────────────────────────

/// Everything one check needs, resolved from config and environment once. Kept explicit
/// rather than read from the environment inside the check so tests can point it at a local
/// server and a scratch directory without touching process-global state.
#[derive(Debug, Clone)]
pub struct Settings {
    pub repository: String,
    pub api_base: String,
    pub target: String,
    pub timeout: Duration,
    /// Download and install a newer release instead of only reporting it.
    pub install: bool,
    pub include_prereleases: bool,
    /// Minimum hours between launch checks; 0 always checks.
    pub interval_hours: u64,
    /// Where the "checked at" cache lives. None disables the cache.
    pub cache_file: Option<PathBuf>,
    /// The binary to replace. None means the running executable.
    pub install_path: Option<PathBuf>,
}

impl Settings {
    /// Build settings from the `[update]` table, applying the documented env overrides.
    pub fn from_config(cfg: &UpdateCfg, cache_file: Option<PathBuf>) -> Settings {
        let cfg = cfg.with_env_overrides();
        Settings {
            repository: cfg.repository,
            api_base: cfg.api_base,
            target: BUILD_TARGET.to_string(),
            timeout: Duration::from_secs(cfg.timeout_secs.clamp(1, 120)),
            install: cfg.auto_install,
            include_prereleases: cfg.include_prereleases,
            interval_hours: cfg.check_interval_hours,
            cache_file,
            install_path: None,
        }
    }

    fn releases_url(&self) -> String {
        format!(
            "{}/repos/{}/releases?per_page=10",
            self.api_base.trim_end_matches('/'),
            self.repository
        )
    }
}

// ── Cache ────────────────────────────────────────────────────────────────────────────

/// A "checked at" record, so opening twenty terminal tabs is one API call, not twenty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub checked_at: u64,
    pub version: Version,
}

pub fn read_cache(path: &Path) -> Option<Cached> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(Cached {
        checked_at: value.get("checked_at")?.as_u64()?,
        version: Version::parse(value.get("version")?.as_str()?)?,
    })
}

pub fn write_cache(path: &Path, version: Version) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::json!({
        "checked_at": unix_now(),
        "version": version.to_string(),
    });
    // Through a temp file, so a concurrent launch never reads a half-written cache.
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        let text = serde_json::to_string_pretty(&body).unwrap_or_default();
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Freshness of a cached answer. `interval_hours == 0` means every launch checks.
pub fn cache_is_fresh(cache: &Cached, interval_hours: u64, now: u64) -> bool {
    if interval_hours == 0 {
        return false;
    }
    now.saturating_sub(cache.checked_at) < interval_hours.saturating_mul(3600)
}

// ── HTTP ─────────────────────────────────────────────────────────────────────────────

async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, UpdateError> {
    let response = client
        .get(url)
        // GitHub's API rejects requests that carry no User-Agent.
        .header(reqwest::header::USER_AGENT, "loom-updater")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.without_url().to_string()))?;
    let status = response.status();
    if !status.is_success() {
        // The body carries the explanation; it is untrusted, so it is bounded and only ever
        // surfaced as a detail string.
        let detail = response
            .text()
            .await
            .map(|t| t.trim().chars().take(200).collect::<String>())
            .unwrap_or_default();
        return Err(UpdateError::Status {
            status: status.as_u16(),
            detail: if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            },
        });
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| UpdateError::Network(e.without_url().to_string()))?;
    Ok(bytes.to_vec())
}

/// Parse one release object. Separate from the request so the parsing rules are testable
/// without a server.
pub fn parse_release(value: &serde_json::Value) -> Result<Release, UpdateError> {
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| UpdateError::Parse("the release has no tag_name".into()))?;
    let version = Version::parse(tag)
        .ok_or_else(|| UpdateError::Parse(format!("the release tag `{tag}` is not a version")))?;
    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(Asset {
                        name: row.get("name")?.as_str()?.to_string(),
                        url: row.get("browser_download_url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release {
        tag: tag.to_string(),
        version,
        prerelease: value
            .get("prerelease")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        assets,
    })
}

/// The newest acceptable release.
///
/// `/releases/latest` already excludes drafts and pre-releases, but a mirror or an enterprise
/// instance may not, so the flag is honoured here too rather than trusted from the endpoint.
/// When only pre-releases exist that route answers 404, so `include_prereleases` falls back to
/// the list.
async fn fetch_release(
    client: &reqwest::Client,
    settings: &Settings,
) -> Result<Release, UpdateError> {
    let body = match get(client, &settings.releases_url()).await {
        Ok(body) => body,
        Err(e) => return Err(e),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| UpdateError::Parse(e.to_string()))?;
    let mut releases: Vec<Release> = match &value {
        serde_json::Value::Array(rows) => {
            rows.iter().filter_map(|r| parse_release(r).ok()).collect()
        }
        other => vec![parse_release(other)?],
    };
    releases.retain(|r| settings.include_prereleases || !r.prerelease);
    releases.sort_by_key(|a| std::cmp::Reverse(a.version));
    releases
        .into_iter()
        .next()
        .ok_or_else(|| UpdateError::Parse("the release list is empty".into()))
}

/// Read the hex digest out of a published `sha256sum`-format checksum file.
pub fn parse_checksum(text: &str) -> Option<String> {
    let hex = text
        .split_whitespace()
        .next()?
        .trim_start_matches('*')
        .to_ascii_lowercase();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex)
    } else {
        None
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ── Install ─────────────────────────────────────────────────────────────────────────

/// Replace `target` with `bytes`, atomically, with the executable bit set.
///
/// The temp file is a sibling of the target because `rename` is only atomic within one
/// filesystem: staging in `/tmp` and renaming across a mount boundary would fail or silently
/// degrade into a copy. Any failure leaves the original untouched.
pub fn replace_binary(target: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| UpdateError::NotWritable {
            path: target.display().to_string(),
            reason: "the path has no parent directory".into(),
            hint: "install the binary into a normal directory".into(),
        })?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("loom");
    let tmp = dir.join(format!(".{file_name}.update-{}", std::process::id()));
    let mut cleanup = TempGuard(Some(tmp.clone()));
    {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| not_writable(target, "the directory is not writable", &e))?;
        file.write_all(bytes)
            .map_err(|e| UpdateError::Io(format!("cannot write {}: {e}", tmp.display())))?;
        // fsync before the rename: otherwise a crash can leave a correctly *named* but empty
        // binary, which is worse than not updating at all.
        file.sync_all()
            .map_err(|e| UpdateError::Io(format!("cannot flush {}: {e}", tmp.display())))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| {
            UpdateError::Io(format!("cannot set permissions on {}: {e}", tmp.display()))
        })?;
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(not_writable(target, "the directory is not writable", &e));
        }
        return Err(UpdateError::Io(format!(
            "cannot replace {}: {e}",
            target.display()
        )));
    }
    cleanup.0 = None;
    Ok(())
}

struct TempGuard(Option<PathBuf>);

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn not_writable(target: &Path, reason: &str, e: &std::io::Error) -> UpdateError {
    let dir = target
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".into());
    UpdateError::NotWritable {
        path: target.display().to_string(),
        reason: format!("{reason} by this user ({dir}: {e})"),
        hint: format!(
            "sudo install -m 0755 <downloaded-loom> {}",
            target.display()
        ),
    }
}

/// The file to replace: the real path of the running executable, symlinks resolved.
/// Replacing a symlink would leave the binary it points at untouched, so the update would
/// appear to succeed and change nothing.
pub fn install_path() -> Result<PathBuf, UpdateError> {
    let exe = std::env::current_exe()
        .map_err(|e| UpdateError::Io(format!("cannot locate the running binary: {e}")))?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Re-exec the updated binary with the original arguments.
///
/// On unix this replaces the process image, so there is no window with both versions alive
/// and the terminal has a single owner. Elsewhere there is no `exec`, so the caller is told
/// to restart rather than being handed a half-update.
pub fn relaunch(argv: &[String]) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let Ok(exe) = std::env::current_exe() else {
            eprintln!("loom: updated; rerun `loom` to use the new version");
            std::process::exit(0);
        };
        let mut command = std::process::Command::new(&exe);
        // argv[0] is the path; the rest are the user's arguments.
        command.args(argv.iter().skip(1));
        command.env(RELAUNCH_GUARD, env!("CARGO_PKG_VERSION"));
        let error = command.exec();
        eprintln!("loom: updated, but cannot restart ({error}); rerun `loom`");
        std::process::exit(0);
    }
    #[cfg(not(unix))]
    {
        let _ = argv;
        println!("loom was updated; restart it to use the new version");
        std::process::exit(0);
    }
}

// ── The check ───────────────────────────────────────────────────────────────────────

/// Look for, and optionally install, a newer release.
///
/// `force` bypasses the freshness cache (`loom update` always asks). A network failure comes
/// back as `Outcome::Unreachable` rather than an error: being offline is not a defect, and a
/// launch must not print a warning about it on every start.
pub async fn run(settings: &Settings, force: bool) -> Result<Outcome, UpdateError> {
    let current = Version::current();
    if !force {
        if let Some(cache) = settings.cache_file.as_deref().and_then(read_cache) {
            if cache_is_fresh(&cache, settings.interval_hours, unix_now()) {
                // A cached answer is only worth short-circuiting when it is *newer*; a cached
                // "nothing new" is still the honest answer for the rest of the TTL.
                if cache.version > current {
                    return Ok(Outcome::Cached {
                        latest: cache.version,
                        age: Duration::from_secs(unix_now().saturating_sub(cache.checked_at)),
                    });
                }
                return Ok(Outcome::UpToDate {
                    current,
                    latest: cache.version,
                });
            }
        }
    }
    check_now(settings).await
}

async fn check_now(settings: &Settings) -> Result<Outcome, UpdateError> {
    let current = Version::current();
    // A process that was relaunched after an update must not install and exec again; the
    // guard is checked here, at the single point where an install could start.
    let relaunched = std::env::var(RELAUNCH_GUARD).is_ok();
    let client = reqwest::Client::builder()
        .timeout(settings.timeout)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    let release = match fetch_release(&client, settings).await {
        Ok(release) => release,
        // Offline, rate limited, or a private/renamed repository: an answer, not a crash.
        Err(e @ UpdateError::Network(_)) | Err(e @ UpdateError::Status { .. }) => {
            return Ok(Outcome::Unreachable {
                reason: e.to_string(),
            })
        }
        Err(e) => return Err(e),
    };
    if let Some(cache) = settings.cache_file.as_deref() {
        let _ = write_cache(cache, release.version);
    }
    if release.version <= current {
        return Ok(Outcome::UpToDate {
            current,
            latest: release.version,
        });
    }
    if !settings.install || relaunched {
        return Ok(Outcome::Available { current, release });
    }
    let asset = release
        .binary_asset(&settings.target)
        .ok_or_else(|| UpdateError::NoAsset {
            tag: release.tag.clone(),
            target: settings.target.clone(),
            assets: release
                .assets
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
        })?
        .clone();
    // A release without a published digest is refused outright rather than installed on
    // trust: the whole point of downloading over TLS is undermined if the payload is
    // unverified.
    let checksum_asset = release
        .checksum_asset(&asset)
        .ok_or_else(|| UpdateError::MissingChecksum {
            tag: release.tag.clone(),
            asset: asset.name.clone(),
        })?
        .clone();
    let published = get(&client, &checksum_asset.url).await?;
    let expected = parse_checksum(&String::from_utf8_lossy(&published)).ok_or_else(|| {
        UpdateError::MissingChecksum {
            tag: release.tag.clone(),
            asset: asset.name.clone(),
        }
    })?;
    let bytes = get(&client, &asset.url).await?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(UpdateError::ChecksumMismatch {
            asset: asset.name,
            expected,
            actual,
        });
    }
    let path = match &settings.install_path {
        Some(path) => path.clone(),
        None => install_path()?,
    };
    replace_binary(&path, &bytes)?;
    Ok(Outcome::Installed {
        current,
        release,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(
            Version::parse("v1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(Version::parse("1.2").map(|v| v.patch), Some(0));
        // A pre-release compares as its release, and build metadata is ignored.
        assert_eq!(Version::parse("1.2.3-rc1"), Version::parse("1.2.3"));
        assert_eq!(Version::parse("1.2.3+build7"), Version::parse("1.2.3"));
        assert!(Version::parse("nightly").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(
            Version::parse("0.10.0").unwrap() > Version::parse("0.9.9").unwrap(),
            "ordering is numeric, not lexicographic"
        );
    }

    #[test]
    fn checksum_files_parse_in_sha256sum_format() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum(&format!("{digest}  loom-0.2.0\n")),
            Some(digest.clone())
        );
        assert_eq!(parse_checksum(&digest), Some(digest));
        assert_eq!(parse_checksum("not-a-digest  loom"), None);
        assert_eq!(parse_checksum(""), None);
    }

    #[test]
    fn a_release_picks_the_asset_for_this_target_and_its_checksum() {
        let release = Release {
            tag: "v0.3.0".into(),
            version: Version::parse("0.3.0").unwrap(),
            prerelease: false,
            assets: vec![
                Asset {
                    name: "loom-0.3.0-x86_64-unknown-linux-gnu".into(),
                    url: "u1".into(),
                },
                Asset {
                    name: "loom-0.3.0-x86_64-unknown-linux-gnu.sha256".into(),
                    url: "u2".into(),
                },
                Asset {
                    name: "loom-0.3.0-aarch64-apple-darwin".into(),
                    url: "u3".into(),
                },
            ],
        };
        let asset = release.binary_asset("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset.url, "u1");
        assert_eq!(
            release.checksum_asset(asset).unwrap().url,
            "u2",
            "the checksum is looked up next to the chosen asset"
        );
        assert!(
            release.binary_asset("aarch64-unknown-linux-gnu").is_none(),
            "another platform's binary is never a candidate"
        );
    }

    #[test]
    fn replacement_installs_atomically_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("loom");
        std::fs::write(&target, b"old").unwrap();
        replace_binary(&target, b"new binary").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the installed binary stays executable");
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "only the binary remains"
        );
    }

    #[test]
    fn cache_freshness_honours_the_interval() {
        let cache = Cached {
            checked_at: 1_000,
            version: Version {
                major: 9,
                minor: 0,
                patch: 0,
            },
        };
        assert!(
            !cache_is_fresh(&cache, 0, 1_000),
            "an interval of 0 always re-checks, so no cached answer is fresh"
        );
        assert!(cache_is_fresh(&cache, 1, 1_000 + 3599));
        assert!(!cache_is_fresh(&cache, 1, 1_000 + 3600));
        assert!(
            !cache_is_fresh(&cache, 24, 1_000 + 86_400),
            "24 hours later the TTL has expired"
        );
    }

    #[test]
    fn a_failed_replace_leaves_the_original_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("loom");
        std::fs::write(&target, b"old").unwrap();
        // A directory where the temp file must go makes creation fail deterministically,
        // without relying on permission bits (which root ignores).
        let blocker = dir
            .path()
            .join(format!(".loom.update-{}", std::process::id()));
        std::fs::create_dir_all(&blocker).unwrap();
        let error = replace_binary(&target, b"new").expect_err("the write cannot succeed");
        assert!(
            matches!(error, UpdateError::NotWritable { .. } | UpdateError::Io(_)),
            "got {error}"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old",
            "the working binary is never clobbered by a failed update"
        );
    }
}
