//! ADR-0020 task-reordering write-back integration tests, on `tempfile::TempDir`
//! fake vaults (never a real vault): a `reorder` action permutes the contents of a
//! note's checkbox-task lines among their existing positions in one atomic write,
//! preserving non-task lines; a concurrent external edit is refused without
//! mutating the file; and an inconsistent payload (a non-task line) is refused.

use std::fs;

use taski_daemon::{ApplyOutcome, index_note, process_reorder, scan_vault};
use taski_db as db;

/// Enqueue + process a reorder for `note_rel`, returning the outcome. `anchor_line`
/// is the moved task's line; `desired` is the involved lines' new top-to-bottom order.
fn run_reorder(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    note_rel: &str,
    anchor_id: i64,
    anchor_line: usize,
    desired: &[usize],
) -> ApplyOutcome {
    db::enqueue_reorder(conn, anchor_id, note_rel, anchor_line, desired).expect("enqueue");
    let action = db::pending_actions(conn).expect("pending").pop().unwrap();
    process_reorder(conn, root, &action).expect("process")
}

#[test]
fn reorder_bubbles_task_to_top() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("r.db").to_string_lossy()).unwrap();

    let note = root.join("inbox.md");
    fs::write(&note, "- [ ] a\n- [ ] b\n- [ ] c\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();

    // Move line 3 ("c") to the top: new order [3, 1, 2].
    let c = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "c")
        .unwrap();
    assert_eq!(
        run_reorder(&conn, root, "inbox.md", c.id, c.line_number, &[3, 1, 2]),
        ApplyOutcome::Applied
    );

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [ ] c\n- [ ] a\n- [ ] b\n"
    );
}

#[test]
fn reorder_preserves_non_task_lines_and_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("r.db").to_string_lossy()).unwrap();

    // A heading and a blank line sit between the tasks; only the task lines move.
    let note = root.join("inbox.md");
    fs::write(&note, "# Inbox\n\n- [ ] a\n- [ ] b\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();

    // Tasks are on lines 3 and 4; swap them.
    let a = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "a")
        .unwrap();
    assert_eq!(
        run_reorder(&conn, root, "inbox.md", a.id, a.line_number, &[4, 3]),
        ApplyOutcome::Applied
    );

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "# Inbox\n\n- [ ] b\n- [ ] a\n"
    );
}

#[test]
fn reorder_identity_follows_content() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("r.db").to_string_lossy()).unwrap();

    let note = root.join("inbox.md");
    fs::write(&note, "- [ ] a\n- [ ] b\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();

    let a_before = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "a")
        .unwrap();

    assert_eq!(
        run_reorder(
            &conn,
            root,
            "inbox.md",
            a_before.id,
            a_before.line_number,
            &[2, 1]
        ),
        ApplyOutcome::Applied
    );
    // The drain loop re-indexes after a successful write; do the same here.
    index_note(&conn, &note, root).unwrap();

    // ADR-0005: "a" keeps its surrogate id at its new line (content-hash match).
    let a_after = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "a")
        .unwrap();
    assert_eq!(
        a_after.id, a_before.id,
        "id follows content to the new line"
    );
    assert_eq!(a_after.line_number, 2, "a moved from line 1 to line 2");
}

#[test]
fn reorder_refused_on_external_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("r.db").to_string_lossy()).unwrap();

    let note = root.join("inbox.md");
    fs::write(&note, "- [ ] a\n- [ ] b\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();
    let a = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "a")
        .unwrap();

    // Simulate a concurrent Obsidian edit after the scan but before the reorder.
    fs::write(&note, "- [ ] a\n- [ ] b\n- [ ] c\n").unwrap();

    assert_eq!(
        run_reorder(&conn, root, "inbox.md", a.id, a.line_number, &[2, 1]),
        ApplyOutcome::ConflictNoteChanged
    );
    // The file is left exactly as the external edit left it — untouched.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [ ] a\n- [ ] b\n- [ ] c\n"
    );
}

#[test]
fn reorder_refused_when_payload_names_a_non_task_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("r.db").to_string_lossy()).unwrap();

    let note = root.join("inbox.md");
    fs::write(&note, "# Inbox\n- [ ] a\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();
    let a = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "a")
        .unwrap();

    // Line 1 is the heading, not a task — an inconsistent payload.
    assert_eq!(
        run_reorder(&conn, root, "inbox.md", a.id, a.line_number, &[1, 2]),
        ApplyOutcome::ReorderInconsistent
    );
    // Nothing written.
    assert_eq!(fs::read_to_string(&note).unwrap(), "# Inbox\n- [ ] a\n");
}
