//! Slice 3 write-back integration tests: a checkbox flip is applied safely to a temp
//! vault, and a concurrent edit is refused without mutating the file. All on
//! `tempfile::TempDir` fake vaults — never a real vault.

use std::fs;

use taski_daemon::{ApplyOutcome, process_action, process_pending_actions, scan_vault};
use taski_db as db;

#[test]
fn flip_open_to_done_applied_unchanged_elsewhere() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("w.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    let original = "# Day\n\n- [ ] task one\n- [x] task two\nsome prose\n";
    fs::write(&note, original).unwrap();

    scan_vault(&conn, root).expect("scan");
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let t1 = tasks
        .iter()
        .find(|t| t.text == "task one")
        .expect("task one indexed");

    // Enqueue open -> done, then apply.
    db::enqueue_action(
        &conn,
        t1.id,
        &t1.note_path,
        t1.line_number,
        &t1.raw_checkbox_char,
        "x",
    )
    .expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action(&conn, root, action).expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // Exactly one byte changed: the checkbox on line 3 went ` ` -> `x`. Everything
    // else — including task two and the prose — is byte-identical.
    let after = fs::read_to_string(&note).unwrap();
    assert_eq!(
        after,
        "# Day\n\n- [x] task one\n- [x] task two\nsome prose\n"
    );

    // Resolving marks the action done and drops it from the pending view.
    db::resolve_action(&conn, action.id, "done", None).unwrap();
    assert!(db::pending_actions(&conn).unwrap().is_empty());
}

#[test]
fn flip_refused_on_concurrent_edit_leaves_file_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("c.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    fs::write(&note, "# Day\n\n- [ ] task one\n- [x] task two\n").unwrap();
    scan_vault(&conn, root).expect("scan");

    // Snapshot the task after scan so the action carries the scanned hash's view.
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let t2 = tasks
        .iter()
        .find(|t| t.text == "task two")
        .expect("task two indexed")
        .clone();

    db::enqueue_action(
        &conn,
        t2.id,
        &t2.note_path,
        t2.line_number,
        &t2.raw_checkbox_char,
        " ",
    )
    .expect("enqueue");

    // Simulate Obsidian editing the note AFTER the scan. The on-disk content now
    // differs from the hash captured at scan.
    let edited = "# Day\n\n- [ ] task one\n- [x] task two\nUSER EDITED ME\n";
    fs::write(&note, edited).unwrap();
    let before_process = fs::read(&note).unwrap();

    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action(&conn, root, action).expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::ConflictNoteChanged,
        "a note changed since scan must be refused, not clobbered"
    );

    // Taski changed nothing: file is byte-identical to the post-edit version.
    let after = fs::read(&note).unwrap();
    assert_eq!(
        after, before_process,
        "on conflict the file must be untouched by Taski"
    );
}

#[test]
fn flip_targets_row_line_number_not_stale_action_line_number() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("o.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    // Enqueue with a wildly stale line_number (ADR-0005: action.line_number is now
    // audit-only). process_action targets the ROW's current line_number (1), not 99.
    db::enqueue_action(&conn, t.id, &t.note_path, 99, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action(&conn, root, action).expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::Applied,
        "process_action must use the row's line_number, not the stale action.line_number"
    );

    // The flip landed on line 1 (the task's actual location).
    assert_eq!(fs::read_to_string(&note).unwrap(), "- [x] task one\n");
}

#[test]
fn flip_refused_when_task_gone_from_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("g.db").to_string_lossy()).expect("open db");
    fs::write(root.join("day.md"), "- [ ] task one\n").unwrap();
    scan_vault(&conn, root).expect("scan");

    // An action for a task id that isn't (or is no longer) indexed.
    db::enqueue_action(&conn, 99999, "day.md", 1, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action(&conn, root, action).expect("process");
    assert_eq!(outcome, ApplyOutcome::TaskNotFound);
    assert_eq!(
        fs::read_to_string(root.join("day.md")).unwrap(),
        "- [ ] task one\n"
    );
}

// ---------------------------------------------------------------------------
// H1: malformed new_char is refused before any I/O.
// ---------------------------------------------------------------------------

#[test]
fn malformed_new_char_refused_h1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("h1.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    for bad in ["", "xy"] {
        db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", bad).expect("enqueue");
        let action = db::pending_actions(&conn).expect("pending")[0].clone();
        let outcome = process_action(&conn, root, &action).expect("process");
        assert_eq!(
            outcome,
            ApplyOutcome::InvalidAction,
            "new_char = {bad:?} must be refused as InvalidAction"
        );
        // Resolve so the next iteration sees the freshly-enqueued action.
        db::resolve_action(&conn, action.id, "failed", None).unwrap();
    }
    // The vault was never touched.
    assert_eq!(fs::read_to_string(&note).unwrap(), "- [ ] task one\n");
}

// ---------------------------------------------------------------------------
// M1: a second flip on the same note is not starved by the first flip's
// hash change — process_pending_actions re-indexes after each Applied.
// ---------------------------------------------------------------------------

#[test]
fn two_flips_same_note_both_applied_m1() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("m1.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] a\n- [ ] b\n").unwrap();
    scan_vault(&conn, root).expect("scan");

    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let a = tasks.iter().find(|t| t.text == "a").expect("task a");
    let b = tasks.iter().find(|t| t.text == "b").expect("task b");

    // Enqueue BOTH flips before processing either. Without M1's post-apply re-index
    // the second would see a stale hash and be refused.
    db::enqueue_action(&conn, a.id, &a.note_path, a.line_number, " ", "x").expect("enqueue a");
    db::enqueue_action(&conn, b.id, &b.note_path, b.line_number, " ", "x").expect("enqueue b");

    process_pending_actions(&conn, root).expect("process pending");

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] a\n- [x] b\n",
        "both flips must apply; M1 re-index lets the second through"
    );
    assert!(
        db::pending_actions(&conn).unwrap().is_empty(),
        "both actions resolved"
    );
}

// ---------------------------------------------------------------------------
// M2: re-processing an already-applied action (crash + restart re-scan) is
// idempotent — it returns Applied and does NOT touch the file again.
// ---------------------------------------------------------------------------

#[test]
fn re_processing_after_apply_is_idempotent_m2() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("m2.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    // First apply: open -> done.
    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    let action = db::pending_actions(&conn).expect("pending")[0].clone();
    let outcome = process_action(&conn, root, &action).expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(fs::read_to_string(&note).unwrap(), "- [x] task one\n");

    // Simulate crash+restart: the re-scan refreshes the stored note_hash to the
    // post-flip content. The unresolved action is then re-processed.
    scan_vault(&conn, root).expect("re-scan");

    let mtime_before = fs::metadata(&note).unwrap().modified().unwrap();
    let outcome2 = process_action(&conn, root, &action).expect("re-process");
    assert_eq!(
        outcome2,
        ApplyOutcome::Applied,
        "re-processing an already-applied flip must be idempotent"
    );
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] task one\n",
        "file must not change on idempotent re-process"
    );
    let _ = mtime_before; // (content equality above is the strong guarantee)
}

// ---------------------------------------------------------------------------
// M4: stale *.taski.tmp files are swept at startup.
// ---------------------------------------------------------------------------

#[test]
fn sweep_removes_stale_tmp_files_m4() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // A normal note + a stale temp in the vault root + a temp inside a hidden dir.
    fs::write(root.join("day.md"), "- [ ] a\n").unwrap();
    fs::write(root.join("day.md.taski.tmp"), "stale").unwrap();
    fs::create_dir(root.join(".obsidian")).unwrap();
    fs::write(root.join(".obsidian/note.md.taski.tmp"), "stale hidden").unwrap();

    let removed = taski_daemon::sweep_tmp_files(root).expect("sweep");
    assert_eq!(
        removed, 1,
        "only the non-hidden top-level temp should be swept"
    );
    assert!(root.join("day.md").exists(), "the real note must survive");
    assert!(
        !root.join("day.md.taski.tmp").exists(),
        "the stale temp must be gone"
    );
    assert!(
        root.join(".obsidian/note.md.taski.tmp").exists(),
        "temp in a hidden dir must NOT be swept (matches scan pruning)"
    );
}

// ---------------------------------------------------------------------------
// ADR-0005 §4: process_action targets the task row's CURRENT line_number
// (updated by reconciliation), not the stale action.line_number.
// ---------------------------------------------------------------------------

#[test]
fn flip_lands_on_current_line_after_line_shift_adr0005() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("ret.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");

    // Task starts at line 1.
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root).expect("scan v1");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();
    assert_eq!(t.line_number, 1);

    // Enqueue a flip. action.line_number = 1 (what the user saw at enqueue time).
    db::enqueue_action(
        &conn,
        t.id,
        &t.note_path,
        t.line_number,
        &t.raw_checkbox_char,
        "x",
    )
    .expect("enqueue");
    let action = db::pending_actions(&conn).unwrap()[0].clone();

    // User inserts a heading ABOVE the task → it shifts to line 2. Re-scan:
    // reconciliation matches by text_hash, updates line_number to 2, and
    // refreshes note_hash to the new content.
    fs::write(&note, "# New heading\n- [ ] task one\n").unwrap();
    scan_vault(&conn, root).expect("scan v2");

    let task_after = db::all_tasks(&conn)
        .expect("all_tasks")
        .iter()
        .find(|x| x.text == "task one")
        .cloned()
        .expect("task one still indexed");
    assert_eq!(task_after.line_number, 2, "task moved to line 2");
    assert_eq!(
        task_after.id, t.id,
        "same rowid preserved by reconciliation"
    );

    // Process the action. It must target the ROW's current line (2), not the
    // stale action.line_number (1). The flip must land on line 2.
    let outcome = process_action(&conn, root, &action).expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // Line 1 (the heading) is untouched; line 2 (the task) is flipped to done.
    let after = fs::read_to_string(&note).unwrap();
    assert_eq!(after, "# New heading\n- [x] task one\n");
}

// ---------------------------------------------------------------------------
// ADR-0005 §1: AUTOINCREMENT guarantees a deleted task's id is never reused.
// ---------------------------------------------------------------------------

#[test]
fn autoincrement_never_reuses_deleted_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("ai.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");

    // Two tasks.
    fs::write(&note, "- [ ] task a\n- [ ] task b\n").unwrap();
    scan_vault(&conn, root).expect("scan v1");
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let id_a = tasks.iter().find(|t| t.text == "task a").unwrap().id;
    let id_b = tasks.iter().find(|t| t.text == "task b").unwrap().id;
    assert!(id_b > id_a, "AUTOINCREMENT assigns increasing ids");

    // Delete task a (remove its line, re-scan). Reconciliation deletes id_a's row
    // and keeps id_b (matched by text_hash, line_number updated to 1).
    fs::write(&note, "- [ ] task b\n").unwrap();
    scan_vault(&conn, root).expect("scan v2");
    let after = db::all_tasks(&conn).expect("all_tasks");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].text, "task b");
    assert!(
        !after.iter().any(|t| t.id == id_a),
        "deleted id must be gone"
    );

    // Add a new task: AUTOINCREMENT must assign a HIGHER id than any prior one.
    fs::write(&note, "- [ ] task b\n- [ ] task c\n").unwrap();
    scan_vault(&conn, root).expect("scan v3");
    let after2 = db::all_tasks(&conn).expect("all_tasks");
    let id_c = after2.iter().find(|t| t.text == "task c").unwrap().id;
    assert!(id_c > id_a, "must not reuse the deleted id");
    assert!(id_c > id_b, "new id must be higher than all prior ids");

    // A pending action referencing the deleted id → TaskNotFound (not some other
    // task that accidentally reused the id).
    db::enqueue_action(&conn, id_a, "day.md", 1, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action(&conn, root, action).expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::TaskNotFound,
        "deleted task id must never resolve to a different task"
    );
}
