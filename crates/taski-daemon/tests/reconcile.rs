//! ADR-0005 §3: content-hash reconciliation tests. Each test exercises
//! `reconcile_note` directly on a `:memory:` DB (the function is DB-level; no vault
//! I/O is needed) and verifies that the surrogate rowid is preserved across re-scans
//! when `text_hash` matches, and deleted/inserted when it doesn't.

use taski_core::parse_tasks;
use taski_db as db;
use taski_db::{ReconcileSummary, reconcile_note};

/// Re-scan identical content → same rowids preserved; attrs refreshed.
#[test]
fn reconcile_unchanged_preserves_rowid() {
    let conn = db::open(":memory:").unwrap();
    let tasks = parse_tasks("- [ ] task a\n- [ ] task b\n", "note.md");
    let s1 = reconcile_note(&conn, "note.md", &tasks, Some("hash1"), Some(100)).unwrap();
    assert_eq!(
        s1,
        ReconcileSummary {
            kept: 0,
            inserted: 2,
            deleted: 0
        }
    );

    let after_v1 = db::all_tasks(&conn).unwrap();
    let id_a = after_v1.iter().find(|t| t.text == "task a").unwrap().id;
    let id_b = after_v1.iter().find(|t| t.text == "task b").unwrap().id;

    // Re-scan identical tasks (new note_hash/mtime to simulate a fresh scan).
    let s2 = reconcile_note(&conn, "note.md", &tasks, Some("hash2"), Some(200)).unwrap();
    assert_eq!(
        s2,
        ReconcileSummary {
            kept: 2,
            inserted: 0,
            deleted: 0
        }
    );

    let after_v2 = db::all_tasks(&conn).unwrap();
    // Same rowids.
    assert_eq!(
        after_v2.iter().find(|t| t.text == "task a").unwrap().id,
        id_a
    );
    assert_eq!(
        after_v2.iter().find(|t| t.text == "task b").unwrap().id,
        id_b
    );
    // Attributes refreshed.
    let a = after_v2.iter().find(|t| t.text == "task a").unwrap();
    assert_eq!(a.note_hash.as_deref(), Some("hash2"));
    assert_eq!(a.note_mtime, Some(200));
}

/// Insert a line above a task → same rowid, `line_number` updated.
#[test]
fn reconcile_moved_updates_line_number_preserves_rowid() {
    let conn = db::open(":memory:").unwrap();
    let tasks_v1 = parse_tasks("- [ ] task a\n", "note.md");
    reconcile_note(&conn, "note.md", &tasks_v1, Some("h1"), None).unwrap();
    let id_a = db::all_tasks(&conn).unwrap()[0].id;
    assert_eq!(db::all_tasks(&conn).unwrap()[0].line_number, 1);

    // A heading is inserted above: task a shifts from line 1 to line 2.
    let tasks_v2 = parse_tasks("# heading\n- [ ] task a\n", "note.md");
    let s = reconcile_note(&conn, "note.md", &tasks_v2, Some("h2"), None).unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            kept: 1,
            inserted: 0,
            deleted: 0
        }
    );

    let after = db::all_tasks(&conn).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, id_a, "same rowid preserved across line shift");
    assert_eq!(
        after[0].line_number, 2,
        "line_number updated to current position"
    );
}

/// Change a task's body text → old rowid deleted, new rowid inserted.
#[test]
fn reconcile_edited_deletes_old_inserts_new() {
    let conn = db::open(":memory:").unwrap();
    let tasks_v1 = parse_tasks("- [ ] original text\n", "note.md");
    reconcile_note(&conn, "note.md", &tasks_v1, Some("h1"), None).unwrap();
    let old_id = db::all_tasks(&conn).unwrap()[0].id;

    // Edit the body: different text_hash → no match → delete + insert.
    let tasks_v2 = parse_tasks("- [ ] edited text\n", "note.md");
    let s = reconcile_note(&conn, "note.md", &tasks_v2, Some("h2"), None).unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            kept: 0,
            inserted: 1,
            deleted: 1
        }
    );

    let after = db::all_tasks(&conn).unwrap();
    assert_eq!(after.len(), 1);
    assert_ne!(after[0].id, old_id, "old rowid gone, new one assigned");
    assert_eq!(after[0].text, "edited text");
}

/// Remove a task line → its row gone.
#[test]
fn reconcile_deleted_removes_row() {
    let conn = db::open(":memory:").unwrap();
    let tasks_v1 = parse_tasks("- [ ] task a\n- [ ] task b\n", "note.md");
    reconcile_note(&conn, "note.md", &tasks_v1, Some("h1"), None).unwrap();
    assert_eq!(db::all_tasks(&conn).unwrap().len(), 2);

    // Task b removed.
    let tasks_v2 = parse_tasks("- [ ] task a\n", "note.md");
    let s = reconcile_note(&conn, "note.md", &tasks_v2, Some("h2"), None).unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            kept: 1,
            inserted: 0,
            deleted: 1
        }
    );

    let after = db::all_tasks(&conn).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].text, "task a");
}

/// Add a task line → new rowid assigned.
#[test]
fn reconcile_new_inserts_row() {
    let conn = db::open(":memory:").unwrap();
    let tasks_v1 = parse_tasks("- [ ] task a\n", "note.md");
    reconcile_note(&conn, "note.md", &tasks_v1, Some("h1"), None).unwrap();
    let id_a = db::all_tasks(&conn).unwrap()[0].id;

    // Add task b below.
    let tasks_v2 = parse_tasks("- [ ] task a\n- [ ] task b\n", "note.md");
    let s = reconcile_note(&conn, "note.md", &tasks_v2, Some("h2"), None).unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            kept: 1,
            inserted: 1,
            deleted: 0
        }
    );

    let after = db::all_tasks(&conn).unwrap();
    assert_eq!(after.len(), 2);
    // task a's rowid preserved.
    assert_eq!(after.iter().find(|t| t.text == "task a").unwrap().id, id_a);
    // task b is new with a distinct id.
    let b = after.iter().find(|t| t.text == "task b").unwrap();
    assert_ne!(b.id, id_a);
}

/// Two identical-text tasks in one note: both matched in FIFO order, both rowids
/// preserved across re-scan.
#[test]
fn reconcile_duplicates_matched_in_order() {
    let conn = db::open(":memory:").unwrap();
    let tasks_v1 = parse_tasks("- [ ] buy milk\n- [ ] buy milk\n", "note.md");
    reconcile_note(&conn, "note.md", &tasks_v1, Some("h1"), None).unwrap();
    let after_v1 = db::all_tasks(&conn).unwrap();
    assert_eq!(after_v1.len(), 2);
    let id_first = after_v1[0].id; // line 1
    let id_second = after_v1[1].id; // line 2
    assert_ne!(id_first, id_second);

    // Re-scan: both should match in FIFO order (first new → first old, etc.).
    let tasks_v2 = parse_tasks("- [x] buy milk\n- [x] buy milk\n", "note.md");
    let s = reconcile_note(&conn, "note.md", &tasks_v2, Some("h2"), None).unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            kept: 2,
            inserted: 0,
            deleted: 0
        }
    );

    let after_v2 = db::all_tasks(&conn).unwrap();
    assert_eq!(after_v2.len(), 2);
    // Both rowids preserved.
    let ids: Vec<i64> = after_v2.iter().map(|t| t.id).collect();
    assert!(ids.contains(&id_first), "first duplicate's rowid preserved");
    assert!(
        ids.contains(&id_second),
        "second duplicate's rowid preserved"
    );
    // Checkbox states refreshed.
    assert!(
        after_v2.iter().all(|t| t.raw_checkbox_char == "x"),
        "both duplicates updated to done"
    );
}

/// ADR-0005 §3 + Phase B: when a task's `text_hash` is unchanged but its `due_date`
/// field changes (e.g. the parser is upgraded to recognise a new emoji), the
/// reconciliation UPDATE-in-place path must refresh `due_date` while preserving the
/// surrogate rowid.
///
/// We can't simulate this purely via `parse_tasks` (adding/removing a `📅` marker
/// changes the body text and thus `text_hash`, which would delete+insert). Instead we
/// construct two `Task`s with identical `text_hash`/`text` but different `due_date`
/// fields — exactly the situation the UPDATE branch exists to handle.
#[test]
fn reconcile_refreshes_due_date_via_update_preserving_rowid() {
    let conn = db::open(":memory:").unwrap();

    // v1: a task with no due date.
    let mut tasks_v1 = parse_tasks("- [ ] task a\n", "note.md");
    let id_before = {
        reconcile_note(&conn, "note.md", &tasks_v1, Some("h1"), None).unwrap();
        let row = &db::all_tasks(&conn).unwrap()[0];
        assert_eq!(row.due_date, None, "v1 should have no due_date");
        row.id
    };

    // v2: same text_hash + same text, but now the parser would attach a due_date.
    // Mutate the in-memory task in place to keep text_hash identical.
    tasks_v1[0].due_date = Some("2025-12-31".to_string());
    let s = reconcile_note(&conn, "note.md", &tasks_v1, Some("h2"), None).unwrap();
    assert_eq!(
        s,
        ReconcileSummary {
            kept: 1,
            inserted: 0,
            deleted: 0
        },
        "matching text_hash should UPDATE-in-place, not delete+insert"
    );

    let after_v2 = db::all_tasks(&conn).unwrap();
    assert_eq!(after_v2.len(), 1);
    assert_eq!(
        after_v2[0].id, id_before,
        "rowid preserved when only due_date changed"
    );
    assert_eq!(
        after_v2[0].due_date.as_deref(),
        Some("2025-12-31"),
        "due_date refreshed via UPDATE path"
    );

    // v3: clear the due_date again — still same text_hash, so another UPDATE.
    let mut tasks_v3 = parse_tasks("- [ ] task a\n", "note.md");
    tasks_v3[0].due_date = None;
    // Force the same text_hash as v1/v2 (parse_tasks already matches since text is
    // identical), and reconcile.
    let _ = tasks_v3[0].text_hash.clone(); // no-op; kept for clarity.
    reconcile_note(&conn, "note.md", &tasks_v3, Some("h3"), None).unwrap();
    let after_v3 = db::all_tasks(&conn).unwrap();
    assert_eq!(after_v3[0].id, id_before, "rowid still preserved");
    assert_eq!(
        after_v3[0].due_date, None,
        "due_date cleared via UPDATE path"
    );
}
