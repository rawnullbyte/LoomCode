//! Append-only JSONL journal + content-addressed blob store for one session.
//!
//! Durability contract: `append` writes the serialized line and flushes before returning,
//! so a crash loses at most the in-flight line, never a prefix. Blobs are written
//! atomically (temp + rename) and addressed by sha256.

use crate::model::{BlobRef, EvId, Event, EventKind};
use crate::{EventsError, Result};
use serde::{Deserialize, Serialize};

/// A persisted session header. Stored as `session.json` beside the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: EvId,
    pub title: String,
    pub created_ms: i64,
    pub cwd: String,
    pub schema_version: u32,
    pub headless: bool,
}
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const SCHEMA_VERSION: u32 = 1;

pub struct FileJournal {
    dir: PathBuf,
    inner: Mutex<JournalInner>,
}

struct JournalInner {
    file: File,
    seq: u64,
}

impl FileJournal {
    /// Create a brand-new session directory. Fails if it already exists.
    pub fn create(sessions_root: &Path, meta: &SessionMeta) -> Result<FileJournal> {
        let dir = session_dir(sessions_root, meta.id);
        fs::create_dir_all(dir.join("blobs"))?;
        let meta_path = dir.join("session.json");
        if meta_path.exists() {
            return Err(EventsError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("session {} already exists", meta.id),
            )));
        }
        write_atomic(&meta_path, &serde_json::to_vec_pretty(meta)?)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        Ok(FileJournal {
            dir,
            inner: Mutex::new(JournalInner { file, seq: 0 }),
        })
    }

    /// Open an existing session for appending, replaying its events.
    pub fn open(sessions_root: &Path, id: EvId) -> Result<(FileJournal, Vec<Event>)> {
        let dir = session_dir(sessions_root, id);
        let events = read_events(&dir.join("events.jsonl"))?;
        let seq = events.last().map(|e| e.seq).unwrap_or(0);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))?;
        Ok((
            FileJournal {
                dir,
                inner: Mutex::new(JournalInner { file, seq }),
            },
            events,
        ))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one event: assigns id/seq/ts and persists with a flush.
    pub fn append(&self, parent: Option<EvId>, kind: EventKind) -> Result<Event> {
        let mut inner = self.inner.lock().expect("journal lock poisoned");
        inner.seq += 1;
        let ev = Event {
            id: EvId::now_v7(),
            parent,
            seq: inner.seq,
            ts_ms: Utc::now().timestamp_millis(),
            kind,
        };
        let mut line = serde_json::to_vec(&ev)?;
        line.push(b'\n');
        inner.file.write_all(&line)?;
        inner.file.flush()?;
        Ok(ev)
    }

    /// Store bytes as a content-addressed blob and return its reference.
    pub fn put_blob(&self, data: &[u8]) -> Result<BlobRef> {
        let digest: String = hex::encode(Sha256::digest(data));
        let path = self.dir.join("blobs").join(&digest);
        if !path.exists() {
            write_atomic(&path, data)?;
        }
        Ok(BlobRef {
            sha256: digest,
            bytes: data.len() as u64,
        })
    }

    pub fn read_blob(&self, r: &BlobRef) -> Result<Vec<u8>> {
        let p = self.dir.join("blobs").join(&r.sha256);
        fs::read(&p).map_err(|e| {
            EventsError::Io(std::io::Error::new(
                e.kind(),
                format!("blob {} missing: {e}", r.sha256),
            ))
        })
    }
}

pub fn session_dir(sessions_root: &Path, id: EvId) -> PathBuf {
    sessions_root.join(id.to_string())
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

/// Alias kept explicit at call sites that specifically want the pre-SQLite layout.
pub fn list_legacy_sessions(sessions_root: &Path) -> Result<Vec<SessionMeta>> {
    list_sessions(sessions_root)
}

/// Read and validate a journal file. Malformed lines are reported with line numbers —
/// a torn final line from a crash is tolerated and reported as such.
pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let f = File::open(path)?;
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(&line) {
            Ok(ev) => out.push(ev),
            Err(e) => {
                let last = i + 1 == line_count(path);
                if last {
                    tracing::warn!(path = %path.display(), error = %e, "torn final journal line (crash during write); prefix replayed");
                    break;
                }
                return Err(EventsError::MalformedLine {
                    line: i + 1,
                    reason: e.to_string(),
                });
            }
        }
    }
    Ok(out)
}

fn line_count(path: &Path) -> usize {
    File::open(path)
        .map(|f| BufReader::new(f).lines().count())
        .unwrap_or(0)
}

/// List sessions sorted newest-first by session id (UUIDv7 ⇒ time-ordered).
pub fn list_sessions(sessions_root: &Path) -> Result<Vec<SessionMeta>> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(sessions_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in rd {
        let entry = entry?;
        let p = entry.path().join("session.json");
        if let Ok(bytes) = fs::read(&p) {
            if let Ok(meta) = serde_json::from_slice::<SessionMeta>(&bytes) {
                out.push(meta);
            }
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.id));
    Ok(out)
}

/// Write bytes atomically: temp file in the same directory, fsync, rename over target.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}
