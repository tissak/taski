//! Slice 3 write-back integration tests: a checkbox flip is applied safely to a temp
//! vault, and a concurrent edit is refused without mutating the file. All on
//! `tempfile::TempDir` fake vaults — never a real vault.

use std::fs;

use taski_daemon::{
    ApplyOutcome, index_note, process_action, process_action_at, process_bullet_action,
    process_metadata_action, process_pending_actions, scan_vault,
};
use taski_db as db;

#[test]
fn flip_open_to_done_applied_unchanged_elsewhere() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("w.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    let original = "# Day\n\n- [ ] task one\n- [x] task two\nsome prose\n";
    fs::write(&note, original).unwrap();

    scan_vault(&conn, root, &[]).expect("scan");
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let t1 = tasks
        .iter()
        .find(|t| t.text == "task one")
        .expect("task one indexed");

    // Enqueue open -> done, then apply. ADR-0012: the done-date stamp composes
    // into the same write as the flip, so ` ✅ 2026-06-21` is appended.
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
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // Exactly one line changed: the checkbox on line 3 went ` ` -> `x` AND a
    // ` ✅ 2026-06-21` stamp was appended. Everything else — including task two
    // and the prose — is byte-identical.
    let after = fs::read_to_string(&note).unwrap();
    assert_eq!(
        after,
        "# Day\n\n- [x] task one ✅ 2026-06-21\n- [x] task two\nsome prose\n"
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
    scan_vault(&conn, root, &[]).expect("scan");

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
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    // Enqueue with a wildly stale line_number (ADR-0005: action.line_number is now
    // audit-only). process_action targets the ROW's current line_number (1), not 99.
    db::enqueue_action(&conn, t.id, &t.note_path, 99, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::Applied,
        "process_action must use the row's line_number, not the stale action.line_number"
    );

    // The flip landed on line 1 (the task's actual location); ADR-0012 stamps `✅`.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] task one ✅ 2026-06-21\n"
    );
}

#[test]
fn flip_refused_when_task_gone_from_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("g.db").to_string_lossy()).expect("open db");
    fs::write(root.join("day.md"), "- [ ] task one\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");

    // An action for a task id that isn't (or is no longer) indexed, AND whose
    // recorded (note_path, line_number) also holds no task. The ADR-0012
    // location fallback (undo after a ✅ stamp) resolves a stale task_id by
    // (note_path, line_number); for "genuinely gone" to surface as
    // TaskNotFound, the recorded location must have no task. Line 99 has no
    // task, so both the id lookup and the fallback location lookup fail.
    db::enqueue_action(&conn, 99999, "day.md", 99, " ", "x").expect("enqueue");
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
    scan_vault(&conn, root, &[]).expect("scan");
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
    scan_vault(&conn, root, &[]).expect("scan");

    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let a = tasks.iter().find(|t| t.text == "a").expect("task a");
    let b = tasks.iter().find(|t| t.text == "b").expect("task b");

    // Enqueue BOTH flips before processing either. Without M1's post-apply re-index
    // the second would see a stale hash and be refused.
    db::enqueue_action(&conn, a.id, &a.note_path, a.line_number, " ", "x").expect("enqueue a");
    db::enqueue_action(&conn, b.id, &b.note_path, b.line_number, " ", "x").expect("enqueue b");

    process_pending_actions(&conn, root).expect("process pending");

    // ADR-0012: each `[ ]→[x]` flip also stamps `✅ <today>` (wall-clock). The M1
    // invariant under test — both flips apply despite sharing the note — is
    // orthogonal to the stamp; we just account for it in the expected bytes.
    let today = taski_core::ymd_from_unix(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    );
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        format!("- [x] a ✅ {today}\n- [x] b ✅ {today}\n"),
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
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    // First apply: open -> in-progress. ADR-0012: flips to Status::InProgress
    // skip the done-date oracle entirely (ambiguous; only the flip is written),
    // so the body text — and thus text_hash — is unchanged. This lets the task's
    // surrogate id survive the re-scan below, which is what the M2 idempotency
    // invariant requires. (A `[ ]→[x]` flip would stamp `✅`, churn text_hash,
    // and the old action.task_id would be TaskNotFound after re-scan — the
    // documented id-churn behavior, not an idempotency regression.)
    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "/").expect("enqueue");
    let action = db::pending_actions(&conn).expect("pending")[0].clone();
    let outcome = process_action_at(&conn, root, &action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(fs::read_to_string(&note).unwrap(), "- [/] task one\n");

    // Simulate crash+restart: the re-scan refreshes the stored note_hash to the
    // post-flip content. The unresolved action is then re-processed.
    scan_vault(&conn, root, &[]).expect("re-scan");

    let mtime_before = fs::metadata(&note).unwrap().modified().unwrap();
    let outcome2 = process_action_at(&conn, root, &action, "2026-06-21").expect("re-process");
    assert_eq!(
        outcome2,
        ApplyOutcome::Applied,
        "re-processing an already-applied flip must be idempotent"
    );
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [/] task one\n",
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

    let removed = taski_daemon::sweep_tmp_files(root, &[]).expect("sweep");
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
    scan_vault(&conn, root, &[]).expect("scan v1");
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
    scan_vault(&conn, root, &[]).expect("scan v2");

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
    let outcome = process_action_at(&conn, root, &action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // Line 1 (the heading) is untouched; line 2 (the task) is flipped to done
    // and stamped `✅ 2026-06-21` (ADR-0012).
    let after = fs::read_to_string(&note).unwrap();
    assert_eq!(after, "# New heading\n- [x] task one ✅ 2026-06-21\n");
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
    scan_vault(&conn, root, &[]).expect("scan v1");
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let id_a = tasks.iter().find(|t| t.text == "task a").unwrap().id;
    let id_b = tasks.iter().find(|t| t.text == "task b").unwrap().id;
    assert!(id_b > id_a, "AUTOINCREMENT assigns increasing ids");

    // Delete task a (remove its line, re-scan). Reconciliation deletes id_a's row
    // and keeps id_b (matched by text_hash, line_number updated to 1).
    fs::write(&note, "- [ ] task b\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan v2");
    let after = db::all_tasks(&conn).expect("all_tasks");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].text, "task b");
    assert!(
        !after.iter().any(|t| t.id == id_a),
        "deleted id must be gone"
    );

    // Add a new task: AUTOINCREMENT must assign a HIGHER id than any prior one.
    fs::write(&note, "- [ ] task b\n- [ ] task c\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan v3");
    let after2 = db::all_tasks(&conn).expect("all_tasks");
    let id_c = after2.iter().find(|t| t.text == "task c").unwrap().id;
    assert!(id_c > id_a, "must not reuse the deleted id");
    assert!(id_c > id_b, "new id must be higher than all prior ids");

    // A pending action referencing the deleted id → TaskNotFound (not some other
    // task that accidentally reused the id). The recorded line_number (99) has
    // no task: the ADR-0012 location fallback (undo after a ✅ stamp) resolves
    // a stale task_id by (note_path, line_number); for "genuinely deleted" to
    // surface as TaskNotFound, the recorded location must have no task. (The
    // AUTOINCREMENT-no-reuse invariant itself is already pinned by the
    // `id_c > id_a`/`id_c > id_b` assertions above.)
    db::enqueue_action(&conn, id_a, "day.md", 99, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action(&conn, root, action).expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::TaskNotFound,
        "deleted task id must never resolve to a different task"
    );
}

// ---------------------------------------------------------------------------
// ADR-0011: bullet toggle (the `b` key) converts `- [ ] task` <-> `- task`.
// Two write-back integration tests mirror the checkbox-flip pair above:
// applied (checkbox -> bullet, body preserved byte-for-byte) and refused on a
// concurrent edit (vault untouched).
// ---------------------------------------------------------------------------

#[test]
fn bullet_toggle_applied_converts_checkbox_to_bullet() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("bt.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    let original = "# Day\n\n- [ ] some task\n- [x] task two\nsome prose\n";
    fs::write(&note, original).unwrap();

    scan_vault(&conn, root, &[]).expect("scan");
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let t1 = tasks
        .iter()
        .find(|t| t.text == "some task")
        .expect("task one indexed");

    // Enqueue a bullet toggle for the open checkbox task, then apply.
    db::enqueue_bullet_toggle(&conn, t1.id, &t1.note_path, t1.line_number).expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_bullet_action(&conn, root, action).expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // The checkbox on line 3 was stripped to a plain bullet; the body is preserved
    // byte-for-byte. Task two and the prose are untouched.
    let after = fs::read_to_string(&note).unwrap();
    assert_eq!(after, "# Day\n\n- some task\n- [x] task two\nsome prose\n");

    // Resolving marks the action done and drops it from the pending view.
    db::resolve_action(&conn, action.id, "done", None).unwrap();
    assert!(db::pending_actions(&conn).unwrap().is_empty());
}

#[test]
fn bullet_toggle_refused_on_concurrent_edit_leaves_file_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("btc.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    fs::write(&note, "# Day\n\n- [ ] some task\n- [x] task two\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");

    // Snapshot the task after scan so the action carries the scanned hash's view.
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let t1 = tasks
        .iter()
        .find(|t| t.text == "some task")
        .expect("task one indexed")
        .clone();

    db::enqueue_bullet_toggle(&conn, t1.id, &t1.note_path, t1.line_number).expect("enqueue");

    // Simulate Obsidian editing the note AFTER the scan. The on-disk content now
    // differs from the hash captured at scan.
    let edited = "# Day\n\n- [ ] some task\n- [x] task two\nUSER EDITED ME\n";
    fs::write(&note, edited).unwrap();
    let before_process = fs::read(&note).unwrap();

    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_bullet_action(&conn, root, action).expect("process");
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

// ---------------------------------------------------------------------------
// ADR-0012: the `✅` done-date stamp composes into the same byte buffer as the
// checkbox flip. These tests pin the composed behaviour on every edge of the
// Decision table. All use `process_action_at` with the fixed date "2026-06-21"
// (the deterministic-date seam) except the end-to-end test which exercises the
// wall-clock `process_pending_actions` drain path.
// ---------------------------------------------------------------------------

/// 6.1 — flip `[ ]→[x]` on a bare task appends ` ✅ <today>` at logical line end.
#[test]
fn done_date_stamped_on_flip_open_to_done() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd1.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] task one ✅ 2026-06-21\n",
        "open→done flip must stamp ✅ <today> at line end"
    );
}

/// 6.2 — on a multi-line note, ONLY the target line changes; the `✅` is appended
/// AFTER any trailing tags.
#[test]
fn done_date_stamped_preserves_other_lines_and_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd2.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    let original = "# Day\n\n- [ ] ship 📅 2026-07-01 #urgent\n- [x] other task\nsome prose\n";
    fs::write(&note, original).unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn)
        .expect("all_tasks")
        .iter()
        .find(|t| t.text.starts_with("ship"))
        .cloned()
        .expect("ship task indexed");

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // Only line 3 changed: checkbox flipped + ` ✅ 2026-06-21` appended after #urgent.
    // The 📅 due date and #urgent tag are preserved; every other line is untouched.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "# Day\n\n- [x] ship 📅 2026-07-01 #urgent ✅ 2026-06-21\n- [x] other task\nsome prose\n",
        "only the target line changed; ✅ appended after trailing tags"
    );
}

/// 6.3 — re-done: an existing `✅ <other>` is REPLACED with today (canonical Tasks
/// behavior), not duplicated.
#[test]
fn done_date_replaced_on_redone_with_different_date() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd3.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] ship ✅ 2026-06-19\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] ship ✅ 2026-06-21\n",
        "existing ✅ date must be replaced, not duplicated"
    );
}

/// 6.4 — idempotent: if `✅ <today>` is already present, the oracle returns
/// `Unchanged` for the stamp; only the checkbox char flips.
#[test]
fn done_date_idempotent_when_already_today() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd4.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    // The task is open but already carries today's ✅ (e.g. user stamped it
    // manually and then un-checked the box). Flipping to done must not duplicate.
    fs::write(&note, "- [ ] ship ✅ 2026-06-21\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] ship ✅ 2026-06-21\n",
        "idempotent stamp — only the checkbox char changed, ✅ untouched"
    );
}

/// 6.5 — un-complete: flip `[x]→[ ]` REMOVES an existing `✅` (symmetry — an open
/// task cannot carry a done date).
#[test]
fn done_date_cleared_on_flip_done_to_open() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd5.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [x] ship ✅ 2026-06-20\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, "x", " ").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [ ] ship\n",
        "done→open flip must remove the ✅ token and its preceding space"
    );
}

/// 6.6 — ambiguous transition: flip `[x]→[/]` (in-progress) leaves `✅` UNTOUCHED.
/// Only the checkbox char flips (ADR-0012 edge table: "Flips involving
/// Status::InProgress (/) ... leave ✅ untouched").
#[test]
fn done_date_not_cleared_on_flip_to_in_progress() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd6.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [x] ship ✅ 2026-06-20\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, "x", "/").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [/] ship ✅ 2026-06-20\n",
        "flip to in-progress must leave ✅ untouched (ambiguous; do not guess)"
    );
}

/// 6.7 — refusal: a malformed existing `✅` refuses the WHOLE toggle (no flip, no
/// stamp). The vault is byte-identical to the original.
#[test]
fn done_date_unparseable_refuses_whole_toggle_no_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd7.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    let original = "- [ ] ship ✅ not-a-date\n";
    fs::write(&note, original).unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::DoneDateUnparseable,
        "malformed ✅ must refuse the whole toggle (no flip, no stamp)"
    );

    // The vault is untouched — no flip landed, no stamp landed.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        original,
        "DoneDateUnparseable must leave the vault byte-identical"
    );
}

/// 6.8 — concurrent-edit refusal prevents BOTH the flip AND the stamp from landing.
/// Clone of `flip_refused_on_concurrent_edit_leaves_file_unchanged`, on a
/// `[ ]→[x]` flip where the stamp would otherwise compose.
#[test]
fn done_date_toggle_refused_on_concurrent_edit_leaves_file_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd8.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "# Day\n\n- [ ] task one\n- [x] task two\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t1 = db::all_tasks(&conn)
        .expect("all_tasks")
        .iter()
        .find(|t| t.text == "task one")
        .cloned()
        .expect("task one indexed");

    db::enqueue_action(&conn, t1.id, &t1.note_path, t1.line_number, " ", "x").expect("enqueue");

    // Simulate Obsidian editing the note AFTER the scan.
    let edited = "# Day\n\n- [ ] task one\n- [x] task two\nUSER EDITED ME\n";
    fs::write(&note, edited).unwrap();
    let before_process = fs::read(&note).unwrap();

    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::ConflictNoteChanged,
        "concurrent edit must be refused — neither flip nor stamp lands"
    );

    let after = fs::read(&note).unwrap();
    assert_eq!(
        after, before_process,
        "on conflict the file must be untouched (no flip, no stamp)"
    );
}

/// 6.9 — 🔑 CRLF-hazard guard. The stamp must be spliced over `[line_range.start,
/// content_end)` where `content_end` EXCLUDES the trailing `\r`. Without the
/// CR-trim, `✅` would land BETWEEN the CR and LF, and the next `parse_tasks`
/// would fold the CR into the task body, permanently polluting `text_hash`.
#[test]
fn done_date_crlf_preserved_on_stamp() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd9.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] task\r\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // The ✅ is BEFORE the \r\n (NOT between \r and \n). The \r\n terminator is
    // preserved outside the spliced region.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] task ✅ 2026-06-21\r\n",
        "CRLF must be preserved; ✅ stamp goes BEFORE the \\r"
    );

    // Independent cross-check via `str::lines()` (the read path's line splitter,
    // which strips a `\r` adjacent to `\n`): the anchor line must contain NO
    // interior `\r`, and the done date must parse back to the stamped date.
    let on_disk = fs::read_to_string(&note).unwrap();
    let anchor = on_disk
        .lines()
        .find(|l| l.contains("task"))
        .expect("anchor");
    assert!(
        !anchor.contains('\r'),
        "anchor line must have NO interior CR (CRLF-hazard check): {anchor:?}"
    );
    assert_eq!(
        taski_core::extract_done_date(anchor).as_deref(),
        Some("2026-06-21"),
        "the stamped ✅ date must parse back correctly"
    );
}

/// 6.10 — end-to-end through the wall-clock `process_pending_actions` drain path.
/// Enqueue via `db::enqueue_action`, call `process_pending_actions`, assert the
/// `✅` date parses to today (the real wall-clock date, whatever it is).
#[test]
fn done_date_stamped_via_process_pending_actions_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("dd10.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] ship it\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, " ", "x").expect("enqueue");
    process_pending_actions(&conn, root).expect("drain");

    // The real wall-clock today (whatever date the test runs on).
    let today = taski_core::ymd_from_unix(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    );

    let on_disk = fs::read_to_string(&note).unwrap();
    assert!(
        on_disk.contains(&format!("✅ {today}")),
        "end-to-end drain must stamp ✅ <today>: got {on_disk:?}"
    );

    // The read path now sees the done date.
    let reparsed = taski_core::parse_tasks(&on_disk, &t.note_path);
    assert_eq!(reparsed.len(), 1);
    assert_eq!(reparsed[0].status, taski_core::Status::Done);
    assert_eq!(reparsed[0].done_date.as_deref(), Some(today.as_str()));
}

// ---------------------------------------------------------------------------
// ADR-0012 regression: the ✅ done-date stamp is appended to the task body,
// changing `text_hash`. Reconciliation then DELETEs the old row + INSERTs a
// new one with a new surrogate id. A pending action referencing the OLD id
// (e.g. an undo enqueued right after the forward flip) would fail with
// TaskNotFound. The fix: when `lookup_task_for_action(task_id)` returns None,
// fall back to the recorded `(note_path, line_number)` from the PendingAction
// row. For same-line metadata stamps the line_number is stable, so the
// location lookup finds the task at its new id.
//
// Symmetric coverage for the ⏳ write path in `process_metadata_action`
// (same id-churn hazard — `⏳` is also appended to the body).
// ---------------------------------------------------------------------------

/// Regression: undoing a `[ ]→[x]` flip after the drain loop has re-indexed the
/// note must NOT fail with `TaskNotFound`. The forward flip stamped `✅`, the
/// re-index churned the task's surrogate id, and the undo action still
/// references the stale id. The location fallback `(note_path, line_number)`
/// finds the task at its new id and the undo applies.
#[test]
fn undo_after_done_flip_uses_location_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("u.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t1 = db::all_tasks(&conn).expect("all_tasks")[0].clone();
    let stale_id = t1.id;
    assert_eq!(t1.line_number, 1);

    // Forward flip: [ ] -> [x] with the original task_id. ADR-0012 stamps ✅.
    db::enqueue_action(&conn, t1.id, &t1.note_path, t1.line_number, " ", "x").expect("enqueue fwd");
    let fwd = db::pending_actions(&conn).expect("pending")[0].clone();
    let outcome = process_action_at(&conn, root, &fwd, "2026-06-21").expect("process fwd");
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] task one ✅ 2026-06-21\n"
    );
    db::resolve_action(&conn, fwd.id, "done", None).unwrap();

    // Re-index — exactly what the drain loop does after every Applied. The ✅
    // stamp changed the body, so text_hash differs and reconcile_note DELETEs
    // the old row + INSERTs a new one (new surrogate id).
    index_note(&conn, &note, root).expect("re-index");

    // The stale task_id no longer resolves — this is the regression condition.
    let after = db::all_tasks(&conn).expect("all_tasks after re-index");
    assert_eq!(after.len(), 1);
    assert!(
        !after.iter().any(|t| t.id == stale_id),
        "stale id must be gone after the ✅ stamp churned text_hash"
    );
    let new_id = after[0].id;
    assert_ne!(
        new_id, stale_id,
        "reconciliation must have assigned a new id"
    );

    // Undo flip: [x] -> [ ] enqueued with the STALE task_id. Without the
    // location fallback this returns TaskNotFound; with it, it returns Applied.
    db::enqueue_action(&conn, stale_id, &t1.note_path, t1.line_number, "x", " ")
        .expect("enqueue undo");
    let undo = db::pending_actions(&conn).expect("pending")[0].clone();
    let outcome = process_action_at(&conn, root, &undo, "2026-06-21").expect("process undo");
    assert_eq!(
        outcome,
        ApplyOutcome::Applied,
        "undo with a stale task_id must succeed via the location fallback"
    );

    // ✅ cleared (un-complete symmetry), checkbox open. Byte-identical to the
    // original.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [ ] task one\n",
        "undo must clear the ✅ stamp and re-open the checkbox"
    );
}

/// Regression guard: the location fallback must STILL return `TaskNotFound`
/// when the task is genuinely gone (note deleted, row pruned). Otherwise the
/// fallback would mask real disappearances as apply-time id churn. Mirrors the
/// existing `flip_refused_when_task_gone_from_index` test, but exercises the
/// location-fallback code path explicitly.
#[test]
fn location_fallback_returns_tasknotfound_when_task_genuinely_deleted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("d.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t1 = db::all_tasks(&conn).expect("all_tasks")[0].clone();
    let stale_id = t1.id;

    // Genuinely delete the task: remove the note file AND prune the row (the
    // watch loop does both — `handle_debounced_event` calls
    // `db::delete_tasks_for_note` on file removal; `index_note` alone would
    // just no-op on a missing file).
    fs::remove_file(&note).unwrap();
    db::delete_tasks_for_note(&conn, &t1.note_path).expect("delete tasks for note");
    assert!(db::all_tasks(&conn).unwrap().is_empty());

    // Enqueue a flip with the stale id. Neither the id lookup nor the location
    // lookup finds anything → TaskNotFound (not Applied, not some other outcome).
    db::enqueue_action(&conn, stale_id, &t1.note_path, t1.line_number, " ", "x").expect("enqueue");
    let action = db::pending_actions(&conn).expect("pending")[0].clone();
    let outcome = process_action_at(&conn, root, &action, "2026-06-21").expect("process");
    assert_eq!(
        outcome,
        ApplyOutcome::TaskNotFound,
        "genuine deletion must surface as TaskNotFound even with the location fallback"
    );
}

/// Regression (metadata path): a second `set_scheduled` write — enqueued after
/// the first one's re-index churned the id — must succeed via the location
/// fallback. Symmetric to `undo_after_done_flip_uses_location_fallback` but
/// for the `⏳` stamp (ADR-0009) in `process_metadata_action`.
#[test]
fn double_set_scheduled_uses_location_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("ms.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t1 = db::all_tasks(&conn).expect("all_tasks")[0].clone();
    let stale_id = t1.id;

    // First ⏳ write: stamp `⏳ 2026-06-21`.
    db::enqueue_set_scheduled(
        &conn,
        t1.id,
        &t1.note_path,
        t1.line_number,
        Some("2026-06-21"),
    )
    .expect("enqueue mark");
    let mark = db::pending_actions(&conn).expect("pending")[0].clone();
    let outcome = process_metadata_action(&conn, root, &mark).expect("process mark");
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [ ] task one ⏳ 2026-06-21\n"
    );
    db::resolve_action(&conn, mark.id, "done", None).unwrap();

    // Re-index — the ⏳ stamp changed the body, so text_hash differs and the
    // surrogate id churns (DELETE + INSERT).
    index_note(&conn, &note, root).expect("re-index");
    let after = db::all_tasks(&conn).expect("all_tasks after re-index");
    assert_eq!(after.len(), 1);
    assert!(
        !after.iter().any(|t| t.id == stale_id),
        "stale id must be gone after the ⏳ stamp churned text_hash"
    );

    // Second ⏳ write: unmark (`payload = None`) enqueued with the STALE task_id.
    // Without the location fallback this returns TaskNotFound; with it, Applied.
    db::enqueue_set_scheduled(&conn, stale_id, &t1.note_path, t1.line_number, None)
        .expect("enqueue unmark");
    let unmark = db::pending_actions(&conn).expect("pending")[0].clone();
    let outcome = process_metadata_action(&conn, root, &unmark).expect("process unmark");
    assert_eq!(
        outcome,
        ApplyOutcome::Applied,
        "second ⏳ write with a stale task_id must succeed via the location fallback"
    );

    // The ⏳ token (and its preceding space) is gone — byte-identical to the
    // original.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [ ] task one\n",
        "unmark must remove the ⏳ token"
    );
}

#[test]
fn undo_bullet_toggle_uses_note_contents_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("btf.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");

    // Capture task identity before the forward toggle deletes the row.
    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let t1 = tasks[0].clone();

    // Forward bullet toggle: - [ ] task one → - task one
    db::enqueue_bullet_toggle(&conn, t1.id, &t1.note_path, t1.line_number).expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_bullet_action(&conn, root, action).expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(fs::read_to_string(&note).unwrap(), "- task one\n");

    // Re-index: the checkbox task is gone from the DB (it's a bullet now).
    db::resolve_action(&conn, action.id, "done", None).unwrap();
    index_note(&conn, &note, root).expect("re-index");
    assert!(
        db::all_tasks(&conn).unwrap().is_empty(),
        "task row should be gone after bullet toggle + re-index"
    );

    // Undo bullet toggle: - task one → - [ ] task one
    // The action references stale task_id, which no longer exists.
    // Location lookup also finds nothing (no task at that line — it's a bullet).
    // The note_contents cache provides the note_hash for conflict detection.
    db::enqueue_bullet_toggle(&conn, t1.id, &t1.note_path, t1.line_number).expect("enqueue undo");
    let undo = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_bullet_action(&conn, root, undo).expect("process undo");
    assert_eq!(
        outcome,
        ApplyOutcome::Applied,
        "undo must succeed via note_contents fallback"
    );

    // The checkbox is restored.
    assert_eq!(fs::read_to_string(&note).unwrap(), "- [ ] task one\n");
}

#[test]
fn bullet_undo_refused_on_concurrent_edit_via_note_contents_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("btc2.db").to_string_lossy()).expect("open db");

    let note = root.join("day.md");
    fs::write(&note, "- [ ] task one\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");

    let tasks = db::all_tasks(&conn).expect("all_tasks");
    let t1 = tasks[0].clone();

    // Forward bullet toggle.
    db::enqueue_bullet_toggle(&conn, t1.id, &t1.note_path, t1.line_number).expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    process_bullet_action(&conn, root, action).expect("process");
    db::resolve_action(&conn, action.id, "done", None).unwrap();
    index_note(&conn, &note, root).expect("re-index");

    // Simulate Obsidian editing the note AFTER the re-index. The note_contents
    // cache has the pre-edit hash; the file now differs.
    fs::write(&note, "- task one EDITED\n").unwrap();

    // Undo bullet toggle: note_contents hash ≠ file hash → ConflictNoteChanged.
    db::enqueue_bullet_toggle(&conn, t1.id, &t1.note_path, t1.line_number).expect("enqueue undo");
    let undo = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_bullet_action(&conn, root, undo).expect("process undo");
    assert_eq!(
        outcome,
        ApplyOutcome::ConflictNoteChanged,
        "concurrent edit must be detected even via note_contents fallback"
    );

    // File is untouched (still the edited version).
    assert_eq!(fs::read_to_string(&note).unwrap(), "- task one EDITED\n");
}

// ---------------------------------------------------------------------------
// ADR-0013: cross-state transitions of the three-state stamp decision.
// `cancelled_date_writeback_proptest` only exercises ` `→`-`; these deterministic
// tests cover the remaining cross-state cells (`x`→`-`, `-`→`x`, `-`→` `) where
// BOTH oracles fire (one stamps, the other clears). Each uses the fixed-date seam
// `process_action_at(.., "2026-06-21")` and cross-checks via the pure extractors.
// ---------------------------------------------------------------------------

/// 7.1 — done→cancelled (`x`→`-`): the `✅` done-date is CLEARED and the `❌`
/// cancelled-date is STAMPED with today (a task cannot be both done and cancelled).
/// Both oracles fire on this transition.
#[test]
fn cancelled_date_stamped_and_done_cleared_on_flip_done_to_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("cd1.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [x] ship ✅ 2026-06-19\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, "x", "-").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [-] ship ❌ 2026-06-21\n",
        "done→cancelled flip must clear ✅ and stamp ❌ <today>"
    );

    // Independent cross-check via the pure extractors (the read path's view).
    let on_disk = fs::read_to_string(&note).unwrap();
    let anchor = on_disk.lines().next().expect("anchor");
    assert_eq!(
        taski_core::extract_cancelled_date(anchor).as_deref(),
        Some("2026-06-21"),
        "cancelled date must parse back to today"
    );
    assert_eq!(
        taski_core::extract_done_date(anchor),
        None,
        "done date must be gone after the cancel transition"
    );
}

/// 7.2 — cancelled→done (`-`→`x`): the `❌` cancelled-date is CLEARED and the
/// `✅` done-date is STAMPED with today. The mirror of 7.1; both oracles fire.
#[test]
fn cancelled_date_cleared_and_done_stamped_on_flip_cancelled_to_done() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("cd2.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [-] ship ❌ 2026-06-19\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, "-", "x").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [x] ship ✅ 2026-06-21\n",
        "cancelled→done flip must clear ❌ and stamp ✅ <today>"
    );

    let on_disk = fs::read_to_string(&note).unwrap();
    let anchor = on_disk.lines().next().expect("anchor");
    assert_eq!(
        taski_core::extract_done_date(anchor).as_deref(),
        Some("2026-06-21"),
        "done date must parse back to today"
    );
    assert_eq!(
        taski_core::extract_cancelled_date(anchor),
        None,
        "cancelled date must be gone after the done transition"
    );
}

/// 7.3 — cancelled→open (`-`→` `): the `❌` cancelled-date is CLEARED (an open
/// task has neither a done nor a cancelled date). Only the ❌ oracle fires (the
/// ✅ oracle is a no-op: there is no ✅ to clear).
#[test]
fn cancelled_date_cleared_on_flip_cancelled_to_open() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("cd3.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [-] ship ❌ 2026-06-19\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, "-", " ").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [ ] ship\n",
        "cancelled→open flip must remove the ❌ token and its preceding space"
    );

    let on_disk = fs::read_to_string(&note).unwrap();
    let anchor = on_disk.lines().next().expect("anchor");
    assert_eq!(
        taski_core::extract_cancelled_date(anchor),
        None,
        "cancelled date must be gone after un-cancel"
    );
}

/// 7.4 — CRLF guard for the done→cancelled cross-state transition: the `❌` stamp
/// is written BEFORE the `\r\n` (not between `\r` and `\n`), and the cleared `✅`
/// leaves no interior CR. Guards the ADR-0012/0013 CRLF discipline on a transition
/// where both oracles fire (one clears, one appends).
#[test]
fn cancelled_date_crlf_preserved_on_done_to_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&tmp.path().join("cd4.db").to_string_lossy()).expect("open db");
    let note = root.join("day.md");
    fs::write(&note, "- [x] ship ✅ 2026-06-19\r\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let t = db::all_tasks(&conn).expect("all_tasks")[0].clone();

    db::enqueue_action(&conn, t.id, &t.note_path, t.line_number, "x", "-").expect("enqueue");
    let action = &db::pending_actions(&conn).expect("pending")[0];
    let outcome = process_action_at(&conn, root, action, "2026-06-21").expect("process");
    assert_eq!(outcome, ApplyOutcome::Applied);

    // The ❌ is BEFORE the \r\n (NOT between \r and \n); the ✅ is gone. The
    // \r\n terminator is preserved outside the spliced region.
    assert_eq!(
        fs::read_to_string(&note).unwrap(),
        "- [-] ship ❌ 2026-06-21\r\n",
        "CRLF must be preserved; ❌ stamp goes BEFORE the \\r; ✅ cleared"
    );

    // Independent cross-check via `str::lines()` (strips a `\r` adjacent to `\n`):
    // the anchor line must contain NO interior `\r`, and the cancelled date must
    // parse back to the stamped date.
    let on_disk = fs::read_to_string(&note).unwrap();
    let anchor = on_disk
        .lines()
        .find(|l| l.contains("ship"))
        .expect("anchor");
    assert!(
        !anchor.contains('\r'),
        "anchor line must have NO interior CR (CRLF-hazard check): {anchor:?}"
    );
    assert_eq!(
        taski_core::extract_cancelled_date(anchor).as_deref(),
        Some("2026-06-21"),
        "the stamped ❌ date must parse back correctly"
    );
    assert_eq!(
        taski_core::extract_done_date(anchor),
        None,
        "the ✅ must be gone after the cancel transition"
    );
}
