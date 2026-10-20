//! Unified session journal facade: SQLite-backed for new sessions, legacy file backend
//! for pre-SQLite sessions. The rest of the codebase programs against this enum.

use crate::journal::FileJournal;
use crate::journal::SessionMeta;
use crate::model::{BlobRef, EvId, Event, EventKind};
use crate::store::Store;
use crate::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub enum Journal {
    Files(Arc<FileJournal>),
    Db { store: Arc<Store>, session: EvId },
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

    /// Create a legacy per-directory JSONL session (kept for tests and migration tools).
    pub fn create_files(sessions_root: &Path, meta: &SessionMeta) -> Result<Journal> {
        Ok(Journal::Files(Arc::new(FileJournal::create(
            sessions_root,
            meta,
        )?)))
    }

    /// Open a session from the store, falling back to the legacy layout.
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
        let (fj, events) = FileJournal::open(sessions_root, id)?;
        Ok((Journal::Files(Arc::new(fj)), events))
    }

    /// Rename this session (SQLite-backed journals only; legacy files report
    /// unsupported honestly).
    pub fn rename_session(&self, title: &str) -> Result<()> {
        match self {
            Journal::Db { store, session } => store.rename_session(*session, title),
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
        }
    }

    pub fn append(&self, parent: Option<EvId>, kind: EventKind) -> Result<Event> {
        match self {
            Journal::Files(f) => f.append(parent, kind),
            Journal::Db { store, session } => store.append(*session, parent, &kind),
        }
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<BlobRef> {
        match self {
            Journal::Files(f) => f.put_blob(data),
            Journal::Db { store, .. } => store.put_blob(data),
        }
    }

    pub fn read_blob(&self, r: &BlobRef) -> Result<Vec<u8>> {
        match self {
            Journal::Files(f) => f.read_blob(r),
            Journal::Db { store, .. } => store.read_blob(r),
        }
    }

    pub fn blob_by_sha(&self, sha: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Journal::Files(f) => Ok(std::fs::read(f.dir().join("blobs").join(sha)).ok()),
            Journal::Db { store, .. } => store.blob_by_sha(sha),
        }
    }

    pub fn events(&self) -> Result<Vec<Event>> {
        match self {
            Journal::Files(f) => crate::journal::read_events(&f.dir().join("events.jsonl")),
            Journal::Db { store, session } => store.events(*session),
        }
    }

    pub fn dir(&self) -> PathBuf {
        match self {
            Journal::Files(f) => f.dir().to_path_buf(),
            Journal::Db { .. } => PathBuf::new(), // blobs live in the store
        }
    }

    pub fn is_db(&self) -> bool {
        matches!(self, Journal::Db { .. })
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
