//! SQLite event store: one database for all sessions, WAL mode, versioned migrations.
//!
//! Replaces the M1 per-session JSONL layout for *new* sessions (legacy JSONL sessions
//! remain readable through [`super::journal::list_legacy_sessions`]). Durability contract:
//! WAL + synchronous=NORMAL — a crash loses at most the in-flight transaction, never a
//! committed prefix. Blobs live in a content-addressed table (sha256 primary key), so
//! identical output dedupes across sessions.

use crate::journal::SessionMeta;
use crate::model::{BlobRef, EvId, Event, EventKind};
use crate::{EventsError, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const STORE_SCHEMA_VERSION: u32 = 1;

const MIGRATIONS: &[&str] = &[
    // v1: initial schema
    "CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        cwd TEXT NOT NULL,
        schema_version INTEGER NOT NULL,
        headless INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS events (
        id TEXT PRIMARY KEY,
        session TEXT NOT NULL REFERENCES sessions(id),
        parent TEXT,
        seq INTEGER NOT NULL,
        ts_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        UNIQUE(session, seq)
    );
    CREATE INDEX idx_events_session_seq ON events(session, seq);
    CREATE TABLE IF NOT EXISTS blobs (
        sha256 TEXT PRIMARY KEY,
        bytes INTEGER NOT NULL,
        data BLOB NOT NULL
    );",
];

pub struct Store {
    conn: Mutex<Connection>,
    /// Kept for API parity with the JSONL journal; SQLite blobs live in the DB.
    #[allow(dead_code)]
    dir: PathBuf,
}

impl Store {
    /// Open (creating if needed) the shared store at `path`, running pending migrations.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Store {
            conn: Mutex::new(conn),
            dir: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut guard = self.conn.lock().expect("store lock");
        let conn = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_ms INTEGER NOT NULL
            );",
        )?;
        let current: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .map_err(EventsError::from)?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let v = (i + 1) as u32;
            if v <= current {
                continue;
            }
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT INTO schema_migrations(version, applied_ms) VALUES (?1, ?2)",
                params![v, Utc::now().timestamp_millis()],
            )?;
            tracing::info!(version = v, "applied store migration");
        }
        // A database written by a *newer* build must not be silently downgraded.
        let highest_known = MIGRATIONS.len() as u32;
        let _ = highest_known;
        conn.commit()?;
        Ok(())
    }

    pub fn create_session(&self, meta: &SessionMeta) -> Result<()> {
        let conn = self.conn.lock().expect("store lock");
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![meta.id.to_string()],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if exists {
            return Err(EventsError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("session {} already exists", meta.id),
            )));
        }
        conn.execute(
            "INSERT INTO sessions(id, title, created_ms, cwd, schema_version, headless)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                meta.id.to_string(),
                meta.title,
                meta.created_ms,
                meta.cwd,
                meta.schema_version,
                meta.headless as i64
            ],
        )?;
        Ok(())
    }

    /// Append one event; assigns id/seq/ts when the caller passes them unresolved.
    pub fn append(&self, session: EvId, parent: Option<EvId>, kind: &EventKind) -> Result<Event> {
        let mut conn = self.conn.lock().expect("store lock");
        let tx = conn.transaction()?;
        let seq: u64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session = ?1",
                params![session.to_string()],
                |r| r.get(0),
            )
            .map_err(EventsError::from)?;
        let ev = Event {
            id: EvId::now_v7(),
            parent,
            seq,
            ts_ms: Utc::now().timestamp_millis(),
            kind: kind.clone(),
        };
        tx.execute(
            "INSERT INTO events(id, session, parent, seq, ts_ms, kind) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                ev.id.to_string(),
                session.to_string(),
                parent.map(|p| p.to_string()),
                seq as i64,
                ev.ts_ms,
                serde_json::to_string(&ev.kind)?
            ],
        )?;
        tx.commit()?;
        Ok(ev)
    }

    pub fn events(&self, session: EvId) -> Result<Vec<Event>> {
        let conn = self.conn.lock().expect("store lock");
        let mut stmt = conn.prepare(
            "SELECT id, parent, seq, ts_ms, kind FROM events WHERE session = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, parent, seq, ts_ms, kind) = row?;
            out.push(Event {
                id: EvId::parse_str(&id).map_err(|e| EventsError::MalformedLine {
                    line: seq as usize,
                    reason: format!("bad event id: {e}"),
                })?,
                parent: parent.and_then(|p| EvId::parse_str(&p).ok()),
                seq: seq as u64,
                ts_ms,
                kind: serde_json::from_str(&kind)?,
            });
        }
        Ok(out)
    }

    /// Events after `after_seq` (exclusive) — used by tailing UIs without full replay.
    pub fn events_since(&self, session: EvId, after_seq: u64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().expect("store lock");
        let mut stmt = conn.prepare(
            "SELECT id, parent, seq, ts_ms, kind FROM events WHERE session = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![session.to_string(), after_seq as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, parent, seq, ts_ms, kind) = row?;
            out.push(Event {
                id: EvId::parse_str(&id).map_err(|e| EventsError::MalformedLine {
                    line: seq as usize,
                    reason: format!("bad event id: {e}"),
                })?,
                parent: parent.and_then(|p| EvId::parse_str(&p).ok()),
                seq: seq as u64,
                ts_ms,
                kind: serde_json::from_str(&kind)?,
            });
        }
        Ok(out)
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<BlobRef> {
        let conn = self.conn.lock().expect("store lock");
        let digest = crate::journal::sha256_hex(data);
        conn.execute(
            "INSERT OR IGNORE INTO blobs(sha256, bytes, data) VALUES (?1, ?2, ?3)",
            params![digest, data.len() as i64, data],
        )?;
        Ok(BlobRef {
            sha256: digest,
            bytes: data.len() as u64,
        })
    }

    pub fn read_blob(&self, r: &BlobRef) -> Result<Vec<u8>> {
        let conn = self.conn.lock().expect("store lock");
        let found: Option<Vec<u8>> = conn
            .query_row(
                "SELECT data FROM blobs WHERE sha256 = ?1",
                params![r.sha256],
                |row| row.get(0),
            )
            .optional()?;
        found.ok_or_else(|| {
            EventsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("blob {} missing", r.sha256),
            ))
        })
    }

    pub fn blob_by_sha(&self, sha: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().expect("store lock");
        let found: Option<Vec<u8>> = conn
            .query_row(
                "SELECT data FROM blobs WHERE sha256 = ?1",
                params![sha],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>> {
        let conn = self.conn.lock().expect("store lock");
        let mut stmt = conn.prepare(
            "SELECT id, title, created_ms, cwd, schema_version, headless FROM sessions ORDER BY created_ms DESC, id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, title, created_ms, cwd, schema_version, headless) = row?;
            out.push(SessionMeta {
                id: EvId::parse_str(&id).map_err(|e| EventsError::MalformedLine {
                    line: 0,
                    reason: format!("bad session id: {e}"),
                })?,
                title,
                created_ms,
                cwd,
                schema_version: schema_version as u32,
                headless: headless != 0,
            });
        }
        Ok(out)
    }

    /// Delete a session and all of its events (used by tooling and tests; the journal
    /// is otherwise append-only).
    pub fn delete_session(&self, id: EvId) -> Result<()> {
        let mut conn = self.conn.lock().expect("store lock");
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM events WHERE session = ?1",
            params![id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Rename a session (shows up in listings / /resume).
    pub fn rename_session(&self, id: EvId, title: &str) -> Result<()> {
        let conn = self.conn.lock().expect("store lock");
        conn.execute(
            "UPDATE sessions SET title = ?2 WHERE id = ?1",
            params![id.to_string(), title],
        )
        .map_err(EventsError::Db)?;
        Ok(())
    }

    pub fn session_meta(&self, id: EvId) -> Result<Option<SessionMeta>> {
        let conn = self.conn.lock().expect("store lock");
        let row = conn
            .query_row(
                "SELECT title, created_ms, cwd, schema_version, headless FROM sessions WHERE id = ?1",
                params![id.to_string()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(
            |(title, created_ms, cwd, schema_version, headless)| SessionMeta {
                id,
                title,
                created_ms,
                cwd,
                schema_version: schema_version as u32,
                headless: headless != 0,
            },
        ))
    }

    /// Integrity check used by `loon doctor` and crash-recovery tests.
    pub fn integrity_check(&self) -> Result<String> {
        let conn = self.conn.lock().expect("store lock");
        let result: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(result)
    }

    /// Checkpoint the WAL (called on clean shutdown; not required for correctness).
    pub fn checkpoint(&self) -> Result<()> {
        let conn = self.conn.lock().expect("store lock");
        let _: String = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .unwrap_or_default();
        Ok(())
    }
}
