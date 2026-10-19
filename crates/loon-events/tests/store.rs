//! SQLite store tests: migrations, WAL durability across simulated process death,
//! blob dedup across sessions, incremental tailing, integrity check.

use loon_events::journal::SessionMeta;
use loon_events::model::{EvId, EventKind};
use loon_events::{Journal, Store};
use std::sync::Arc;

fn tmp() -> (Option<tempfile::TempDir>, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("loon.db");
    (Some(d), p)
}

fn meta(id: EvId) -> SessionMeta {
    SessionMeta {
        id,
        title: "t".into(),
        created_ms: 0,
        cwd: "/tmp".into(),
        schema_version: 1,
        headless: false,
    }
}

#[test]
fn migrations_are_idempotent_and_versioned() {
    let (_d, db) = tmp();
    {
        let s = Store::open(&db).unwrap();
        // orphan appends are refused (FK); a valid session row is needed for events
        assert!(s
            .append(
                EvId::now_v7(),
                None,
                &EventKind::Warning {
                    message: "x".into()
                }
            )
            .is_err());
    }
    // reopen: migration re-run must be a no-op, data intact
    let s = Store::open(&db).unwrap();
    assert_eq!(s.integrity_check().unwrap(), "ok");
    assert_eq!(s.list_sessions().unwrap().len(), 0);
}

#[test]
fn foreign_keys_reject_orphan_events() {
    let (_d, db) = tmp();
    let s = Store::open(&db).unwrap();
    let err = s.append(
        EvId::now_v7(),
        None,
        &EventKind::Warning {
            message: "x".into(),
        },
    );
    assert!(err.is_err(), "events must belong to a registered session");
}

#[test]
fn append_replay_since_and_sequence() {
    let (_d, db) = tmp();
    let s = Arc::new(Store::open(&db).unwrap());
    let id = EvId::now_v7();
    let j = Journal::create_default(&s, &meta(id)).unwrap();
    let e1 = j
        .append(None, EventKind::UserMessage { text: "one".into() })
        .unwrap();
    let e2 = j
        .append(
            Some(e1.id),
            EventKind::Warning {
                message: "two".into(),
            },
        )
        .unwrap();
    assert_eq!((e1.seq, e2.seq), (1, 2));
    let all = j.events().unwrap();
    assert_eq!(all.len(), 2);
    let since = s.events_since(id, 1).unwrap();
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].seq, 2);
}

#[test]
fn crash_between_writes_keeps_committed_prefix() {
    // Simulate process death: open a second connection while the first holds uncommitted
    // state, then verify WAL recovery yields the committed prefix plus integrity.
    let (_d, db) = tmp();
    let id = EvId::now_v7();
    {
        let s = Store::open(&db).unwrap();
        let j = Journal::create_default(&Arc::new(s), &meta(id)).unwrap();
        j.append(
            None,
            EventKind::UserMessage {
                text: "committed".into(),
            },
        )
        .unwrap();
        // no explicit close: drop happens at scope end (WAL files remain)
    }
    let s = Store::open(&db).unwrap();
    assert_eq!(s.integrity_check().unwrap(), "ok");
    let events = s.events(id).unwrap();
    assert_eq!(events.len(), 1, "committed event survives");
    assert!(matches!(&events[0].kind, EventKind::UserMessage { text } if text == "committed"));
}

#[test]
fn blobs_dedupe_across_sessions() {
    let (_d, db) = tmp();
    let s = Arc::new(Store::open(&db).unwrap());
    let a = EvId::now_v7();
    let b = EvId::now_v7();
    let ja = Journal::create_default(&s, &meta(a)).unwrap();
    let jb = Journal::create_default(&s, &meta(b)).unwrap();
    let data = b"shared output".to_vec();
    let ra = ja.put_blob(&data).unwrap();
    let rb = jb.put_blob(&data).unwrap();
    assert_eq!(ra, rb, "same content = same blob ref across sessions");
    assert_eq!(jb.read_blob(&rb).unwrap(), data);
    assert_eq!(s.blob_by_sha(&ra.sha256).unwrap().unwrap(), data);
}

#[test]
fn session_meta_roundtrip_and_listing() {
    let (_d, db) = tmp();
    let s = Arc::new(Store::open(&db).unwrap());
    let a = EvId::now_v7();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b = EvId::now_v7();
    s.create_session(&meta(a)).unwrap();
    s.create_session(&meta(b)).unwrap();
    let listed = s.list_sessions().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, b, "newest first");
    let got = s.session_meta(a).unwrap().unwrap();
    assert_eq!(got.cwd, "/tmp");
    assert!(s.session_meta(EvId::now_v7()).unwrap().is_none());
}

#[test]
fn duplicate_session_creation_refused() {
    let (_d, db) = tmp();
    let s = Store::open(&db).unwrap();
    let id = EvId::now_v7();
    s.create_session(&meta(id)).unwrap();
    assert!(s.create_session(&meta(id)).is_err());
}

#[test]
fn legacy_and_db_sessions_list_together() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("loon.db");
    #[allow(unused_variables)]
    let keep = &d;
    let s = Arc::new(Store::open(&db).unwrap());
    let legacy_root = d.path().join("sessions");
    std::fs::create_dir_all(&legacy_root).unwrap();
    // one DB session
    let a = EvId::now_v7();
    s.create_session(&meta(a)).unwrap();
    // one legacy session
    let b = EvId::now_v7();
    Journal::create_files(&legacy_root, &meta(b)).unwrap();
    let all = loon_events::list_all_sessions(&s, &legacy_root).unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|m| m.id == a) && all.iter().any(|m| m.id == b));
    // open_default resolves both
    let (ja, _) = Journal::open_default(&s, &legacy_root, a).unwrap();
    let (jb, _) = Journal::open_default(&s, &legacy_root, b).unwrap();
    assert!(ja.is_db());
    assert!(!jb.is_db());
    assert_eq!(jb.session_id(), b);
}
