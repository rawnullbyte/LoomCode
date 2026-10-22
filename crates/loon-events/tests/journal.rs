use loon_events::journal::{list_sessions, read_events, SessionMeta};
use loon_events::model::EvId;
use loon_events::model::{EventKind, Usage};
use std::io::Write;

fn tmp_root() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    (d, root)
}

fn meta(id: EvId) -> SessionMeta {
    SessionMeta::fresh(id, "test", 0, "/tmp", false)
}

#[test]
fn append_replay_and_sequence() {
    let (_d, root) = tmp_root();
    let id = EvId::now_v7();
    let j = loon_events::Journal::create_files(&root, &meta(id)).unwrap();
    let e1 = j
        .append(None, EventKind::UserMessage { text: "hi".into() })
        .unwrap();
    let e2 = j
        .append(
            Some(e1.id),
            EventKind::Warning {
                message: "w".into(),
            },
        )
        .unwrap();
    assert_eq!(e1.seq, 1);
    assert_eq!(e2.seq, 2);
    assert_eq!(e2.parent, Some(e1.id));
    let j2 = loon_events::FileJournal::open(&root, id).unwrap().0;
    let events =
        loon_events::journal::read_events(&root.join(id.to_string()).join("events.jsonl")).unwrap();
    assert_eq!(events.len(), 2);
    // continues sequence after reopen
    let e3 = j2
        .append(
            None,
            EventKind::Error {
                message: "e".into(),
            },
        )
        .unwrap();
    assert_eq!(e3.seq, 3);
}

#[test]
fn blob_roundtrip_is_content_addressed() {
    let (_d, root) = tmp_root();
    let j = loon_events::Journal::create_files(&root, &meta(EvId::now_v7())).unwrap();
    let data = b"large output".repeat(100);
    let r1 = j.put_blob(&data).unwrap();
    let r2 = j.put_blob(&data).unwrap();
    assert_eq!(r1, r2, "identical content dedupes");
    assert_eq!(j.read_blob(&r1).unwrap(), data);
    assert_eq!(r1.bytes, data.len() as u64);
}

#[test]
fn torn_final_line_is_tolerated_and_reported() {
    let (_d, root) = tmp_root();
    let id = EvId::now_v7();
    {
        let j = loon_events::Journal::create_files(&root, &meta(id)).unwrap();
        j.append(
            None,
            EventKind::UserMessage {
                text: "complete".into(),
            },
        )
        .unwrap();
    }
    // simulate a crash mid-write: append a partial line without newline
    let path = root.join(id.to_string()).join("events.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(f, "{{\"id\":\"trunc").unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1, "only the complete line is replayed");
}

#[test]
fn malformed_interior_line_is_an_error_not_silence() {
    let (_d, root) = tmp_root();
    let id = EvId::now_v7();
    {
        let j = loon_events::Journal::create_files(&root, &meta(id)).unwrap();
        j.append(None, EventKind::UserMessage { text: "one".into() })
            .unwrap();
    }
    let path = root.join(id.to_string()).join("events.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(f, "{{not json}}").unwrap();
    writeln!(f, "{{\"padding\":\"so this is not the last line\"}}").unwrap();
    let err = read_events(&path).unwrap_err();
    assert!(matches!(
        err,
        loon_events::EventsError::MalformedLine { line: 2, .. }
    ));
}

#[test]
fn session_listing_is_newest_first() {
    let (_d, root) = tmp_root();
    let a = EvId::now_v7();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b = EvId::now_v7();
    loon_events::Journal::create_files(&root, &meta(a)).unwrap();
    loon_events::Journal::create_files(&root, &meta(b)).unwrap();
    let listed = list_sessions(&root).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, b, "newest first");
}

#[test]
fn usage_serializes_none_as_absent_not_zero() {
    let u = Usage::default();
    let s = serde_json::to_string(&u).unwrap();
    assert!(!s.contains("0,"), "empty usage must not imply zeros: {s}");
    assert!(u.is_empty());
}
