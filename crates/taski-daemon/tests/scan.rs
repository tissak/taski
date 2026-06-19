//! Deterministic integration tests for the daemon's scan logic.
//!
//! These exercise `scan_vault` and `index_note` against a throwaway fake vault under
//! a `tempfile::TempDir`. The live watch loop is intentionally NOT tested here — it
//! is timing-flaky — per the Slice 1 spec.

use std::fs;

use taski_daemon::{index_note, scan_vault};
use taski_db as db;
use taski_db::Status;

#[test]
fn scan_indexes_md_notes_and_skips_hidden_dirs_and_non_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("test.db").to_string_lossy()).expect("open db");

    // root/note1.md — two open/done tasks.
    fs::write(root.join("note1.md"), "- [ ] root task\n- [x] done task\n").unwrap();
    // root/sub/note2.md — one in-progress task (nested dir).
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/note2.md"), "- [/] nested task\n").unwrap();
    // root/.hidden/secret.md — inside a hidden dir; MUST be skipped entirely.
    fs::create_dir_all(root.join(".hidden")).unwrap();
    fs::write(root.join(".hidden/secret.md"), "- [ ] should be skipped\n").unwrap();
    // root/sub/.obsidian/config.md — a hidden dir *inside* a normal one; skipped.
    fs::create_dir_all(root.join("sub/.obsidian")).unwrap();
    fs::write(root.join("sub/.obsidian/config.md"), "- [ ] also skipped\n").unwrap();
    // root/notes.txt — not markdown; MUST be skipped.
    fs::write(root.join("notes.txt"), "- [ ] not a markdown task\n").unwrap();
    // root/README.MD — uppercase extension is still markdown.
    fs::write(root.join("README.MD"), "- [ ] uppercase ext\n").unwrap();

    let total = scan_vault(&conn, root).expect("scan_vault");
    // note1(2) + sub/note2(1) + README.MD(1) = 4
    assert_eq!(total, 4, "hidden-dir and non-md files must be skipped");

    let tasks = db::all_tasks(&conn).expect("all_tasks");
    assert_eq!(tasks.len(), 4);

    // Nothing from a hidden directory or a .txt file made it in.
    assert!(
        tasks.iter().all(|t| !t.note_path.contains(".hidden")),
        "no tasks from .hidden: {:?}",
        tasks
    );
    assert!(
        tasks.iter().all(|t| !t.note_path.contains(".obsidian")),
        "no tasks from .obsidian: {:?}",
        tasks
    );
    assert!(
        tasks.iter().all(|t| !t.note_path.ends_with(".txt")),
        "no tasks from .txt: {:?}",
        tasks
    );

    // note_path uses forward slashes and is relative to the vault root.
    let note_paths: Vec<&str> = tasks.iter().map(|t| t.note_path.as_str()).collect();
    assert!(note_paths.contains(&"note1.md"), "{:?}", note_paths);
    assert!(note_paths.contains(&"sub/note2.md"), "{:?}", note_paths);
    assert!(note_paths.contains(&"README.MD"), "{:?}", note_paths);

    // Statuses round-tripped correctly.
    assert!(
        tasks
            .iter()
            .any(|t| t.status == Status::Open && t.text == "root task"),
        "{:?}",
        tasks
    );
    assert!(
        tasks
            .iter()
            .any(|t| t.status == Status::Done && t.text == "done task"),
        "{:?}",
        tasks
    );
    assert!(
        tasks
            .iter()
            .any(|t| t.status == Status::InProgress && t.text == "nested task"),
        "{:?}",
        tasks
    );
}

#[test]
fn index_note_replaces_old_tasks_on_rescan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("t.db").to_string_lossy()).expect("open db");
    let note = root.join("a.md");

    // First version: two open tasks.
    fs::write(&note, "- [ ] one\n- [ ] two\n").unwrap();
    assert_eq!(index_note(&conn, &note, root).expect("index v1"), 2);
    let after_v1 = db::all_tasks(&conn).expect("all_tasks");
    assert_eq!(after_v1.len(), 2);

    // Second version: only one task, done. The two old rows MUST be removed (not 3).
    fs::write(&note, "- [x] only\n").unwrap();
    assert_eq!(index_note(&conn, &note, root).expect("index v2"), 1);
    let after_v2 = db::all_tasks(&conn).expect("all_tasks");
    assert_eq!(
        after_v2.len(),
        1,
        "old tasks must be deleted before re-insert"
    );
    assert_eq!(after_v2[0].text, "only");
    assert_eq!(after_v2[0].status, Status::Done);
    assert_eq!(after_v2[0].note_path, "a.md");
}

#[test]
fn index_note_skips_non_utf8_without_bailing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("t.db").to_string_lossy()).expect("open db");
    let note = root.join("bad.md");
    // Invalid UTF-8 (a lone continuation byte).
    fs::write(&note, b"- [ ] before\xff after\n").unwrap();

    let n = index_note(&conn, &note, root).expect("non-utf8 must not error");
    assert_eq!(n, 0, "non-UTF8 note is skipped, not indexed");
    assert!(db::all_tasks(&conn).expect("all_tasks").is_empty());
}

/// Slice 3: indexing must populate `note_hash` + `note_mtime` (the anchor for the
/// write-back conflict check), and the hash must be stable for unchanged bytes but
/// change when the bytes change.
#[test]
fn index_note_populates_note_hash_and_mtime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("h.db").to_string_lossy()).expect("open db");
    let note = root.join("a.md");
    fs::write(&note, "- [ ] do a thing\n").unwrap();

    index_note(&conn, &note, root).expect("index");
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].note_hash.is_some(), "note_hash must be populated");
    assert!(
        tasks[0].note_mtime.is_some(),
        "note_mtime must be populated"
    );

    // Stable for unchanged bytes.
    let h1 = tasks[0].note_hash.clone().unwrap();
    index_note(&conn, &note, root).expect("re-index");
    let h2 = db::all_tasks(&conn).expect("all_tasks")[0]
        .note_hash
        .clone()
        .unwrap();
    assert_eq!(h1, h2, "content hash must be stable for unchanged bytes");

    // Different when bytes change.
    fs::write(&note, "- [ ] do a DIFFERENT thing\n").unwrap();
    index_note(&conn, &note, root).expect("re-index changed");
    let h3 = db::all_tasks(&conn).expect("all_tasks")[0]
        .note_hash
        .clone()
        .unwrap();
    assert_ne!(h3, h1, "content hash must change when bytes change");
}
