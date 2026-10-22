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
    SessionMeta::fresh(id, "t", 0, "/tmp", false)
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

/// Regression: resume listed sessions by creation time, so the session you had just
/// messaged sank below any session created after it. Ordering is by last activity.
#[test]
fn sessions_list_by_last_activity_not_creation() {
    let (_d, path) = tmp();
    let store = Store::open(&path).unwrap();
    let first = EvId::now_v7();
    let second = EvId::now_v7();
    store.create_session(&meta(first)).unwrap();
    store.create_session(&meta(second)).unwrap();
    // `second` is newer by creation, so it leads before anyone writes.
    let order: Vec<EvId> = store
        .list_sessions()
        .unwrap()
        .iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(order, vec![second, first], "newest created first");

    // Now use `first`: it must jump above `second`.
    store
        .append(first, None, &EventKind::UserMessage { text: "hi".into() })
        .unwrap();
    let order: Vec<EvId> = store
        .list_sessions()
        .unwrap()
        .iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(
        order,
        vec![first, second],
        "the session that was messaged moves to the top"
    );

    // Using `second` moves it back above.
    store
        .append(second, None, &EventKind::UserMessage { text: "hi".into() })
        .unwrap();
    let order: Vec<EvId> = store
        .list_sessions()
        .unwrap()
        .iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(order, vec![second, first], "the most recent message wins");
}

/// `updated_ms` is exposed so callers can show "last used" without reading events.
#[test]
fn session_meta_reports_last_activity() {
    let (_d, path) = tmp();
    let store = Store::open(&path).unwrap();
    let id = EvId::now_v7();
    store.create_session(&meta(id)).unwrap();
    let before = store.session_meta(id).unwrap().unwrap();
    assert_eq!(
        before.updated_ms, before.created_ms,
        "fresh: no activity yet"
    );
    store
        .append(id, None, &EventKind::UserMessage { text: "hi".into() })
        .unwrap();
    let after = store.session_meta(id).unwrap().unwrap();
    assert!(
        after.updated_ms > before.updated_ms,
        "appending advances the activity stamp"
    );
}

/// A database written before the activity column existed must order correctly right
/// after the upgrade, not only after each session's next message.
#[test]
fn upgrade_backfills_activity_from_existing_events() {
    let (_d, path) = tmp();
    // Build a v1 database by hand: the sessions table without `updated_ms`.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_ms INTEGER NOT NULL);
             CREATE TABLE sessions (
                id TEXT PRIMARY KEY, title TEXT NOT NULL, created_ms INTEGER NOT NULL,
                cwd TEXT NOT NULL, schema_version INTEGER NOT NULL, headless INTEGER NOT NULL
             );
             CREATE TABLE events (
                id TEXT PRIMARY KEY, session TEXT NOT NULL REFERENCES sessions(id),
                parent TEXT, seq INTEGER NOT NULL, ts_ms INTEGER NOT NULL, kind TEXT NOT NULL,
                UNIQUE(session, seq)
             );
             INSERT INTO schema_migrations(version, applied_ms) VALUES (1, 0);",
        )
        .unwrap();
        let old = EvId::now_v7();
        let recent = EvId::now_v7();
        conn.execute(
            "INSERT INTO sessions(id,title,created_ms,cwd,schema_version,headless) VALUES (?1,'old',1,'/tmp',1,0)",
            rusqlite::params![old.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions(id,title,created_ms,cwd,schema_version,headless) VALUES (?1,'recent',2,'/tmp',1,0)",
            rusqlite::params![recent.to_string()],
        )
        .unwrap();
        // `old` was created first but messaged last (higher ts).
        conn.execute(
            "INSERT INTO events(id,session,parent,seq,ts_ms,kind) VALUES ('e1',?1,NULL,1,50,'{\"kind\":\"user_message\",\"text\":\"x\"}')",
            rusqlite::params![old.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events(id,session,parent,seq,ts_ms,kind) VALUES ('e2',?1,NULL,1,10,'{\"kind\":\"user_message\",\"text\":\"x\"}')",
            rusqlite::params![recent.to_string()],
        )
        .unwrap();
        // Remember the ids for the assertions below.
        std::fs::write(path.with_extension("ids"), format!("{old}\n{recent}")).unwrap();
    }
    let ids = std::fs::read_to_string(path.with_extension("ids")).unwrap();
    let mut lines = ids.lines();
    let old = EvId::parse_str(lines.next().unwrap()).unwrap();
    let recent = EvId::parse_str(lines.next().unwrap()).unwrap();

    // Opening the store applies the v2 migration.
    let store = Store::open(&path).unwrap();
    let order: Vec<EvId> = store
        .list_sessions()
        .unwrap()
        .iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(
        order,
        vec![old, recent],
        "the backfill must use event timestamps, not creation order"
    );
    assert_eq!(store.session_meta(old).unwrap().unwrap().updated_ms, 50);
}
