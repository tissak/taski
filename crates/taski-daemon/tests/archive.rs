//! ADR-0021 archive write-back integration tests, on `tempfile::TempDir` fake vaults
//! (never a real vault): an `archive` action moves a note's completed (`[x]`/`[-]`)
//! flat task lines out of the source and into the archive note via copy-then-delete —
//! creating the archive on first use, appending to an existing archive, preserving
//! open/in-progress tasks and non-task lines, refusing (without touching either file)
//! on a concurrent source edit, and refusing an inconsistent payload.

use std::fs;

use taski_daemon::{ApplyOutcome, index_note, process_archive, scan_vault};
use taski_db as db;

/// Enqueue + process an archive of `lines` from `source_rel` into `archive_rel`,
/// returning the outcome. `anchor_id`/`anchor_line` identify a task in the source.
fn run_archive(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    source_rel: &str,
    anchor_id: i64,
    anchor_line: usize,
    archive_rel: &str,
    lines: &[usize],
) -> ApplyOutcome {
    db::enqueue_archive(conn, anchor_id, source_rel, anchor_line, archive_rel, lines)
        .expect("enqueue");
    let action = db::pending_actions(conn).expect("pending").pop().unwrap();
    process_archive(conn, root, &action).expect("process")
}

/// The (id, line) of the source's completed flat tasks, in line order — what the TUI
/// computes before enqueuing. Done (`[x]`) and cancelled (`[-]`) count; open and
/// in-progress do not.
fn completed_lines(conn: &rusqlite::Connection, source_rel: &str) -> Vec<(i64, usize)> {
    let mut v: Vec<(i64, usize)> = db::all_tasks(conn)
        .unwrap()
        .into_iter()
        .filter(|t| {
            t.note_path == source_rel
                && t.indent == 0
                && (matches!(t.status, taski_core::Status::Done) || t.raw_checkbox_char == "-")
        })
        .map(|t| (t.id, t.line_number))
        .collect();
    v.sort_by_key(|(_, ln)| *ln);
    v
}

#[test]
fn archive_creates_archive_and_removes_completed_from_source() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("a.db").to_string_lossy()).unwrap();

    let source = root.join("task-inbox.md");
    fs::write(
        &source,
        "# Inbox\n- [ ] open one\n- [x] done one\n- [/] in progress\n- [-] cancelled one\n",
    )
    .unwrap();
    scan_vault(&conn, root, &[]).unwrap();

    let completed = completed_lines(&conn, "task-inbox.md");
    assert_eq!(completed.len(), 2, "[x] done + [-] cancelled are completed");
    let lines: Vec<usize> = completed.iter().map(|(_, ln)| *ln).collect();

    assert_eq!(
        run_archive(
            &conn,
            root,
            "task-inbox.md",
            completed[0].0,
            completed[0].1,
            "task-archive.md",
            &lines,
        ),
        ApplyOutcome::Applied
    );

    // Source keeps the heading + open + in-progress tasks, in order; completed gone.
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "# Inbox\n- [ ] open one\n- [/] in progress\n"
    );
    // Archive was created (first-creation path) with the completed lines, verbatim.
    assert_eq!(
        fs::read_to_string(root.join("task-archive.md")).unwrap(),
        "- [x] done one\n- [-] cancelled one\n"
    );
}

#[test]
fn archive_appends_to_existing_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("a.db").to_string_lossy()).unwrap();

    let source = root.join("task-inbox.md");
    fs::write(&source, "- [ ] keep\n- [x] sweep me\n").unwrap();
    let archive = root.join("task-archive.md");
    fs::write(&archive, "- [x] older archived task\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();

    let completed = completed_lines(&conn, "task-inbox.md");
    let lines: Vec<usize> = completed.iter().map(|(_, ln)| *ln).collect();
    assert_eq!(
        run_archive(
            &conn,
            root,
            "task-inbox.md",
            completed[0].0,
            completed[0].1,
            "task-archive.md",
            &lines,
        ),
        ApplyOutcome::Applied
    );

    assert_eq!(fs::read_to_string(&source).unwrap(), "- [ ] keep\n");
    // The new line is appended below the existing archived content.
    assert_eq!(
        fs::read_to_string(&archive).unwrap(),
        "- [x] older archived task\n- [x] sweep me\n"
    );
}

#[test]
fn archive_refused_on_external_source_edit_leaves_both_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("a.db").to_string_lossy()).unwrap();

    let source = root.join("task-inbox.md");
    fs::write(&source, "- [ ] keep\n- [x] sweep me\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();
    let completed = completed_lines(&conn, "task-inbox.md");
    let lines: Vec<usize> = completed.iter().map(|(_, ln)| *ln).collect();

    // Simulate a concurrent Obsidian edit after the scan but before the archive.
    fs::write(
        &source,
        "- [ ] keep\n- [x] sweep me\n- [ ] added in obsidian\n",
    )
    .unwrap();

    assert_eq!(
        run_archive(
            &conn,
            root,
            "task-inbox.md",
            completed[0].0,
            completed[0].1,
            "task-archive.md",
            &lines,
        ),
        ApplyOutcome::ConflictNoteChanged
    );
    // The source hash check fires BEFORE Phase A, so the archive was never created
    // and the source is exactly as the external edit left it.
    assert!(
        !root.join("task-archive.md").exists(),
        "archive not created"
    );
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "- [ ] keep\n- [x] sweep me\n- [ ] added in obsidian\n"
    );
}

#[test]
fn archive_refused_when_payload_names_an_open_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("a.db").to_string_lossy()).unwrap();

    let source = root.join("task-inbox.md");
    fs::write(&source, "- [ ] open\n- [x] done\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();
    let done = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "done")
        .unwrap();

    // Line 1 is open, not completed — an inconsistent payload.
    assert_eq!(
        run_archive(
            &conn,
            root,
            "task-inbox.md",
            done.id,
            done.line_number,
            "task-archive.md",
            &[1, 2],
        ),
        ApplyOutcome::ArchiveInconsistent
    );
    // Nothing written: source unchanged, no archive.
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "- [ ] open\n- [x] done\n"
    );
    assert!(!root.join("task-archive.md").exists());
}

#[test]
fn archive_keeps_surviving_task_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let conn = db::open(&root.join("a.db").to_string_lossy()).unwrap();

    let source = root.join("task-inbox.md");
    fs::write(&source, "- [x] done first\n- [ ] survivor\n").unwrap();
    scan_vault(&conn, root, &[]).unwrap();

    let survivor_before = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "survivor")
        .unwrap();
    let completed = completed_lines(&conn, "task-inbox.md");
    let lines: Vec<usize> = completed.iter().map(|(_, ln)| *ln).collect();

    assert_eq!(
        run_archive(
            &conn,
            root,
            "task-inbox.md",
            completed[0].0,
            completed[0].1,
            "task-archive.md",
            &lines,
        ),
        ApplyOutcome::Applied
    );
    // The drain loop re-indexes the source after a successful write; do the same.
    index_note(&conn, &source, root).unwrap();

    // ADR-0005: the survivor keeps its surrogate id at its new (shifted-up) line.
    let survivor_after = db::all_tasks(&conn)
        .unwrap()
        .into_iter()
        .find(|t| t.text == "survivor")
        .unwrap();
    assert_eq!(survivor_after.id, survivor_before.id, "id follows content");
    assert_eq!(
        survivor_after.line_number, 1,
        "shifted from line 2 to line 1"
    );
}
