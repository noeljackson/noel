//! The storage conformance suite for noel's journal trait
//! (`JournalStorage`): what Durable's `storage-conformance.ts` checks that
//! applies to a journal (plan slice 2, audit item 10) —
//!
//! - an append is visible to a read and survives a reopen;
//! - the journal's bytes are exactly the records appended, newline-ended;
//! - a rewrite replaces the journal atomically: the next read sees the
//!   rewritten bytes and nothing of the old ones;
//! - appends from many handles of one process serialize (no interleaved
//!   or torn records);
//! - a second process cannot open the same journal (`Locked`), and
//!   handles of one process share the lock.

use std::sync::Arc;

use noel_core::storage::{JsonlFile, MemoryJournal, Storage};
use noel_core::SessionError;

use noel_durable::SqliteJournal;

fn record(text: &str) -> Vec<u8> {
    format!("{{\"text\":\"{text}\"}}").into_bytes()
}

fn reads_as(storage: &Storage) -> String {
    String::from_utf8(storage.read().expect("read")).expect("utf-8")
}

#[test]
fn appended_records_read_back_in_order_and_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!("noel-conformance-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("session.sqlite");
    {
        let storage: Storage = Arc::new(SqliteJournal::open(&path).expect("open"));
        storage.append(&record("first")).expect("append");
        storage.append(&record("second")).expect("append");
        assert_eq!(
            reads_as(&storage),
            "{\"text\":\"first\"}\n{\"text\":\"second\"}\n"
        );
    }
    let reopened: Storage = Arc::new(SqliteJournal::open(&path).expect("reopen"));
    assert_eq!(
        reads_as(&reopened),
        "{\"text\":\"first\"}\n{\"text\":\"second\"}\n",
        "records survive a reopen"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_backend_reads_the_same_journal() {
    let dir =
        std::env::temp_dir().join(format!("noel-conformance-backends-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let expected = "{\"text\":\"a\"}\n{\"text\":\"b\"}\n";

    let sqlite: Storage = Arc::new(SqliteJournal::open(dir.join("s.sqlite")).expect("open"));
    sqlite.append(&record("a")).expect("append");
    sqlite.append(&record("b")).expect("append");

    let file: Storage = Arc::new(JsonlFile::open(dir.join("s.jsonl")).expect("open"));
    file.append(&record("a")).expect("append");
    file.append(&record("b")).expect("append");

    let memory: Storage = Arc::new(MemoryJournal::new("memory"));
    memory.append(&record("a")).expect("append");
    memory.append(&record("b")).expect("append");

    for (name, storage) in [("sqlite", sqlite), ("file", file), ("memory", memory)] {
        assert_eq!(
            reads_as(&storage),
            expected,
            "{name} reads the same journal"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rewrite_replaces_the_whole_journal_atomically() {
    let dir = std::env::temp_dir().join(format!("noel-conformance-rewrite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let storage: Storage = Arc::new(SqliteJournal::open(dir.join("s.sqlite")).expect("open"));
    storage.append(&record("before-one")).expect("append");
    storage.append(&record("before-two")).expect("append");

    storage
        .rewrite(&mut |journal| {
            let kept: Vec<Vec<u8>> = journal
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty() && !String::from_utf8_lossy(line).contains("two"))
                .map(|line| line.to_vec())
                .collect();
            let mut rewritten: Vec<u8> = kept.join(&b"\n"[..]);
            rewritten.push(b'\n');
            rewritten.extend_from_slice(&record("after"));
            rewritten.push(b'\n');
            Ok(rewritten)
        })
        .expect("rewrite");

    assert_eq!(
        reads_as(&storage),
        "{\"text\":\"before-one\"}\n{\"text\":\"after\"}\n",
        "the rewrite replaced the journal"
    );
    // The rewrite is durable: a reopen reads the same.
    let reopened: Storage = Arc::new(SqliteJournal::open(dir.join("s.sqlite")).expect("reopen"));
    assert_eq!(
        reads_as(&reopened),
        "{\"text\":\"before-one\"}\n{\"text\":\"after\"}\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn appends_from_many_handles_of_one_process_do_not_interleave() {
    let dir = std::env::temp_dir().join(format!("noel-conformance-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("s.sqlite");
    let storage: Storage = Arc::new(SqliteJournal::open(&path).expect("open"));

    let handles = (0..8)
        .map(|worker| {
            let storage = storage.clone();
            std::thread::spawn(move || {
                for attempt in 0..50 {
                    storage
                        .append(&record(&format!("w{worker}a{attempt}")))
                        .expect("append");
                }
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().expect("worker");
    }

    let journal = reads_as(&storage);
    let lines: Vec<&str> = journal.lines().collect();
    assert_eq!(lines.len(), 8 * 50, "every append landed: {}", lines.len());
    for line in &lines {
        assert!(
            line.starts_with("{\"text\":\"w") && line.ends_with("\"}"),
            "no record torn or interleaved: {line}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_process_cannot_open_a_held_journal() {
    let dir = std::env::temp_dir().join(format!("noel-conformance-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    let path = dir.join("s.sqlite");
    let _held: Storage = Arc::new(SqliteJournal::open(&path).expect("open"));

    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("hold-the-lock-and-exit")
        .env("NOEL_CONFORMANCE_JOURNAL", &path)
        .status()
        .expect("run child");
    assert!(child.success(), "the child reports what it saw");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The child of the lock test: opening the held journal must fail with
/// `Locked` at once. Driven through the same test binary.
#[test]
fn hold_the_lock_and_exit() {
    let Ok(path) = std::env::var("NOEL_CONFORMANCE_JOURNAL") else {
        return;
    };
    match SqliteJournal::open(path) {
        Err(SessionError::Locked { .. }) => {}
        Err(other) => panic!("expected Locked, got {other:?}"),
        Ok(journal) => panic!("a second process opened a held journal: {journal:?}"),
    }
}
