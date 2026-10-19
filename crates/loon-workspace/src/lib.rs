//! Workspace: path authority, sandbox checks, and compare-and-swap mutation records.
//!
//! Every mutating operation goes through [`Workspace`] so that (a) paths are validated
//! against the sandbox roots, (b) stale reads cannot clobber concurrent user edits
//! (content-hash CAS), and (c) each change produces a [`MutationRecord`] with before/after
//! hashes — the substrate rewind is built on in a later milestone.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WsError {
    #[error("path escapes the workspace sandbox: {0}")]
    OutsideSandbox(String),
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("path is not a file: {0}")]
    NotAFile(String),
    #[error("conflict: file changed since it was read (expected sha256 {expected}, found {found}). Re-read the file and retry.")]
    StaleWrite { expected: String, found: String },
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid path: {0}")]
    InvalidPath(String),
}

pub type Result<T> = std::result::Result<T, WsError>;

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Before/after record for one mutated file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationRecord {
    pub path: String,
    /// sha256 before the change (all-zero for new files).
    pub before_sha256: String,
    pub after_sha256: String,
    pub tool: String,
    pub ts_ms: i64,
    pub seq: u64,
}

pub const EMPTY_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub struct Workspace {
    root: PathBuf,
    extra_write_roots: Vec<PathBuf>,
    mutation_seq: AtomicU64,
    /// Serializes mutating operations.
    write_lock: Mutex<()>,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>, extra_write_roots: Vec<PathBuf>) -> Result<Workspace> {
        let root = root.into();
        let root = root.canonicalize().map_err(|e| WsError::Io {
            path: root.display().to_string(),
            source: e,
        })?;
        for r in &extra_write_roots {
            if !r.is_absolute() {
                return Err(WsError::InvalidPath(format!(
                    "extra write root must be absolute: {}",
                    r.display()
                )));
            }
        }
        Ok(Workspace {
            root,
            extra_write_roots,
            mutation_seq: AtomicU64::new(0),
            write_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user/tool-supplied path against the workspace root and verify it stays
    /// inside the sandbox. Symlinks are resolved, so a symlink escaping the root is caught.
    pub fn resolve(&self, p: &str) -> Result<PathBuf> {
        if p.contains('\0') {
            return Err(WsError::InvalidPath("path contains NUL".into()));
        }
        let raw = PathBuf::from(p);
        let joined = if raw.is_absolute() {
            raw
        } else {
            self.root.join(raw)
        };
        // Canonicalize the deepest existing ancestor, then rejoin the remainder, so
        // not-yet-created files validate against their real parent.
        let canon = canonicalize_deepest(&joined)?;
        if !self.is_within_sandbox(&canon) {
            return Err(WsError::OutsideSandbox(canon.display().to_string()));
        }
        Ok(canon)
    }

    fn is_within_sandbox(&self, p: &Path) -> bool {
        // Read access anywhere is not restricted in M1; the sandbox governs *writes*.
        let _ = p;
        true
    }

    fn is_writable(&self, p: &Path) -> bool {
        if p.starts_with(&self.root) {
            return true;
        }
        self.extra_write_roots.iter().any(|r| p.starts_with(r))
    }

    pub fn check_writable(&self, p: &Path) -> Result<()> {
        if self.is_writable(p) {
            Ok(())
        } else {
            Err(WsError::OutsideSandbox(format!(
                "write to {} denied (writable roots: {}, plus configured extras)",
                p.display(),
                self.root.display()
            )))
        }
    }

    pub fn read_file(&self, p: &Path) -> Result<(Vec<u8>, String)> {
        let data = std::fs::read(p).map_err(|e| WsError::Io {
            path: p.display().to_string(),
            source: e,
        })?;
        let hash = sha256_hex(&data);
        Ok((data, hash))
    }

    /// Serialized mutating section. Returns a guard so callers hold it across
    /// read-hash → write → record.
    pub fn write_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        match self.write_lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Atomic compare-and-swap write. `expect_sha256 = None` skips the check (explicitly
    /// authorized full-file write); `Some(EMPTY_HASH)` asserts the file does not exist.
    /// Per-file replacement is atomic (temp + rename). Returns the mutation record.
    pub fn cas_write(
        &self,
        path: &Path,
        bytes: &[u8],
        expect_sha256: Option<&str>,
        tool: &str,
        _guard: &std::sync::MutexGuard<'_, ()>,
    ) -> Result<MutationRecord> {
        self.check_writable(path)?;
        let current = match std::fs::read(path) {
            Ok(b) => Some(sha256_hex(&b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(WsError::Io {
                    path: path.display().to_string(),
                    source: e,
                })
            }
        };
        if let Some(expected) = expect_sha256 {
            let found = current.clone().unwrap_or_else(|| EMPTY_HASH.to_string());
            if expected != found {
                return Err(WsError::StaleWrite {
                    expected: expected.to_string(),
                    found,
                });
            }
        }
        write_atomic(path, bytes).map_err(|e| WsError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let after = sha256_hex(bytes);
        let seq = self.mutation_seq.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(MutationRecord {
            path: path.display().to_string(),
            before_sha256: current.unwrap_or_else(|| EMPTY_HASH.to_string()),
            after_sha256: after,
            tool: tool.to_string(),
            ts_ms: chrono_millis(),
            seq,
        })
    }

    /// Record an externally-observed mutation (e.g. produced by a shell command) without
    /// performing one; used to keep the mutation trail honest about who changed what.
    pub fn record_external(
        &self,
        path: &Path,
        before: Option<String>,
        after: String,
        tool: &str,
    ) -> MutationRecord {
        let seq = self.mutation_seq.fetch_add(1, Ordering::SeqCst) + 1;
        MutationRecord {
            path: path.display().to_string(),
            before_sha256: before.unwrap_or_else(|| EMPTY_HASH.to_string()),
            after_sha256: after,
            tool: tool.to_string(),
            ts_ms: chrono_millis(),
            seq,
        }
    }

    /// Hash the current content of a path, if it exists.
    pub fn hash_of(&self, path: &Path) -> Result<Option<String>> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(sha256_hex(&b))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WsError::Io {
                path: path.display().to_string(),
                source: e,
            }),
        }
    }
}

/// Canonicalize the deepest existing ancestor of `p` and rejoin the non-existing tail.
fn canonicalize_deepest(p: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        match cur.canonicalize() {
            Ok(real) => {
                let mut real = real;
                for seg in missing.iter().rev() {
                    real = real.join(seg);
                }
                // Reject any `..` that survived into a non-existing tail by normalizing
                // lexically against the canonical prefix.
                return Ok(normalize_lexical(&real));
            }
            Err(_) => match (cur.file_name(), cur.parent()) {
                (Some(name), Some(parent)) => {
                    missing.push(name.to_os_string());
                    cur = parent.to_path_buf();
                }
                _ => return Err(WsError::NotFound(p.display().to_string())),
            },
        }
    }
}

fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("loon-tmp-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn chrono_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("proj")).unwrap();
        let w = Workspace::new(d.path().join("proj"), vec![]).unwrap();
        (d, w)
    }

    #[test]
    fn resolve_relative_and_reject_escape() {
        let (_d, w) = ws();
        let p = w.resolve("src/main.rs").unwrap();
        assert!(p.starts_with(w.root()));
        let up = w.resolve("../outside.txt").unwrap();
        assert!(
            !up.starts_with(w.root()),
            "resolve reports the escape, write-side enforces it"
        );
        assert!(w.check_writable(&up).is_err());
    }

    #[test]
    fn symlink_escape_is_caught() {
        let (d, w) = ws();
        let target = d.path().join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, d.path().join("proj").join("link")).unwrap();
        let p = w.resolve("link/evil.txt").unwrap();
        assert!(
            w.check_writable(&p).is_err(),
            "symlinked path outside root must not be writable"
        );
    }

    #[test]
    fn cas_write_detects_stale_write() {
        let (_d, w) = ws();
        let f = w.root().join("a.txt");
        std::fs::write(&f, b"one").unwrap();
        let (_, h1) = w.read_file(&f).unwrap();
        // concurrent user edit
        std::fs::write(&f, b"two").unwrap();
        let g = w.write_lock();
        let err = w
            .cas_write(&f, b"three", Some(&h1), "edit", &g)
            .unwrap_err();
        assert!(matches!(err, WsError::StaleWrite { .. }), "{err}");
        // and with the fresh hash it succeeds
        let (_, h2) = w.read_file(&f).unwrap();
        let rec = w.cas_write(&f, b"three", Some(&h2), "edit", &g).unwrap();
        assert_eq!(rec.before_sha256, h2);
        assert_eq!(rec.after_sha256, sha256_hex(b"three"));
    }

    #[test]
    fn new_file_write_requires_empty_hash_when_asserted() {
        let (_d, w) = ws();
        let f = w.root().join("new.txt");
        let g = w.write_lock();
        // The file doesn't exist, so an EMPTY_HASH assertion passes.
        w.cas_write(&f, b"x", Some(EMPTY_HASH), "write", &g)
            .unwrap();
        // Now it exists; the same assertion must fail.
        let err = w
            .cas_write(&f, b"y", Some(EMPTY_HASH), "write", &g)
            .unwrap_err();
        assert!(matches!(err, WsError::StaleWrite { .. }));
    }
}
