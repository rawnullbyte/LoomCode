//! Unified session journal facade: SQLite-backed for new sessions, legacy file backend
//! for pre-SQLite sessions. The rest of the codebase programs against this enum.
//!
//! Sessions are *lazily materialized*: [`Journal::create_pending`] reserves a session id
//! and header but writes nothing to the store. The row, the header event and the first
//! real event all land together on the first [`Journal::append`]. A session that never
//! receives content therefore never reaches the store, which is what keeps "open the app,
//! look around, quit" from leaving an empty session behind in `/resume`.

use crate::journal::FileJournal;
use crate::journal::SessionMeta;
use crate::model::{BlobRef, EvId, Event, EventKind};
use crate::store::Store;
use crate::Result;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub enum Journal {
    Files(Arc<FileJournal>),
    Db {
        store: Arc<Store>,
        session: EvId,
    },
    /// Reserved but not yet written. Materializes on the first append (or rename).
    Pending(Arc<PendingSession>),
}

/// A session that exists only in memory. The first write creates the store row, appends
/// the header (typically `SessionStarted`) and then the triggering event, in that order,
/// so a replay always sees how the session began.
pub struct PendingSession {
    store: Arc<Store>,
    id: EvId,
    meta: Mutex<Option<SessionMeta>>,
    header: Option<EventKind>,
}

impl PendingSession {
    /// Write the session row and header if they are not written yet.
    fn materialize(&self) -> Result<Journal> {
        let mut guard = self.meta.lock().expect("journal state");
        if let Some(meta) = guard.take() {
            self.store.create_session(&meta)?;
            if let Some(header) = &self.header {
                self.store.append(self.id, None, header)?;
            }
        }
        Ok(Journal::Db {
            store: self.store.clone(),
            session: self.id,
        })
    }
}

impl Journal {
    /// Create a session in the shared SQLite store (the default since M2).
    pub fn create_default(store: &Arc<Store>, meta: &SessionMeta) -> Result<Journal> {
        store.create_session(meta)?;
        Ok(Journal::Db {
            store: store.clone(),
            session: meta.id,
        })
    }

    /// Reserve a session without persisting it. `header` is appended immediately before
    /// whatever event first materializes the session.
    pub fn create_pending(
        store: &Arc<Store>,
        meta: &SessionMeta,
        header: Option<EventKind>,
    ) -> Journal {
        Journal::Pending(Arc::new(PendingSession {
            store: store.clone(),
            id: meta.id,
            meta: Mutex::new(Some(meta.clone())),
            header,
        }))
    }

    /// Create a legacy per-directory JSONL session (kept for tests and migration tools).
    pub fn create_files(sessions_root: &Path, meta: &SessionMeta) -> Result<Journal> {
        Ok(Journal::Files(Arc::new(FileJournal::create(
            sessions_root,
            meta,
        )?)))
    }

    /// Open a session from the store, falling back to the legacy layout. An id that
    /// exists in neither backend is an error naming the id, not a file-system detail.
    pub fn open_default(
        store: &Arc<Store>,
        sessions_root: &Path,
        id: EvId,
    ) -> Result<(Journal, Vec<Event>)> {
        if store.session_meta(id)?.is_some() {
            let events = store.events(id)?;
            return Ok((
                Journal::Db {
                    store: store.clone(),
                    session: id,
                },
                events,
            ));
        }
        let legacy = crate::journal::session_dir(sessions_root, id).join("session.json");
        if !legacy.exists() {
            return Err(crate::EventsError::SessionNotFound { id: id.to_string() });
        }
        let (fj, events) = FileJournal::open(sessions_root, id)?;
        Ok((Journal::Files(Arc::new(fj)), events))
    }

    /// Rename this session. Renaming a pending session materializes it: naming a session
    /// is a deliberate act and the name must survive a restart.
    pub fn rename_session(&self, title: &str) -> Result<()> {
        match self {
            Journal::Db { store, session } => store.rename_session(*session, title),
            Journal::Pending(p) => p.materialize()?.rename_session(title),
            Journal::Files(_) => Err(crate::EventsError::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "legacy file journals do not support renaming",
            ))),
        }
    }

    pub fn session_id(&self) -> EvId {
        match self {
            Journal::Files(f) => {
                // derive from dir name
                let name = f
                    .dir()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                EvId::parse_str(name).unwrap_or_else(|_| EvId::nil())
            }
            Journal::Db { session, .. } => *session,
            Journal::Pending(p) => p.id,
        }
    }

    /// False only while this session is reserved but unwritten.
    pub fn is_materialized(&self) -> bool {
        match self {
            Journal::Files(_) | Journal::Db { .. } => true,
            Journal::Pending(p) => p.meta.lock().expect("journal state").is_none(),
        }
    }

    /// True while this session exists only in memory. The first append (or rename)
    /// materializes it into the store.
    pub fn is_pending(&self) -> bool {
        !self.is_materialized()
    }

    pub fn append(&self, parent: Option<EvId>, kind: EventKind) -> Result<Event> {
        match self {
            Journal::Files(f) => f.append(parent, kind),
            Journal::Db { store, session } => store.append(*session, parent, &kind),
            Journal::Pending(p) => p.materialize()?.append(parent, kind),
        }
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<BlobRef> {
        match self {
            Journal::Files(f) => f.put_blob(data),
            // Blobs are store-global (content addressed), so this needs no session row.
            Journal::Db { store, .. } => store.put_blob(data),
            Journal::Pending(p) => p.store.put_blob(data),
        }
    }

    pub fn read_blob(&self, r: &BlobRef) -> Result<Vec<u8>> {
        match self {
            Journal::Files(f) => f.read_blob(r),
            Journal::Db { store, .. } => store.read_blob(r),
            Journal::Pending(p) => p.store.read_blob(r),
        }
    }

    pub fn blob_by_sha(&self, sha: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Journal::Files(f) => Ok(std::fs::read(f.dir().join("blobs").join(sha)).ok()),
            Journal::Db { store, .. } => store.blob_by_sha(sha),
            Journal::Pending(p) => p.store.blob_by_sha(sha),
        }
    }

    pub fn events(&self) -> Result<Vec<Event>> {
        match self {
            Journal::Files(f) => crate::journal::read_events(&f.dir().join("events.jsonl")),
            Journal::Db { store, session } => store.events(*session),
            Journal::Pending(p) => {
                if p.meta.lock().expect("journal state").is_some() {
                    Ok(Vec::new())
                } else {
                    p.store.events(p.id)
                }
            }
        }
    }

    pub fn dir(&self) -> PathBuf {
        match self {
            Journal::Files(f) => f.dir().to_path_buf(),
            // Blobs live in the store, not in a per-session directory.
            Journal::Db { .. } | Journal::Pending(_) => PathBuf::new(),
        }
    }

    pub fn is_db(&self) -> bool {
        !matches!(self, Journal::Files(_))
    }
}

/// List sessions across both backends, newest first, deduplicated by id.
pub fn list_all_sessions(store: &Arc<Store>, sessions_root: &Path) -> Result<Vec<SessionMeta>> {
    let mut out = store.list_sessions()?;
    for m in crate::journal::list_legacy_sessions(sessions_root)? {
        if !out.iter().any(|x| x.id == m.id) {
            out.push(m);
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.id));
    Ok(out)
}
