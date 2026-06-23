//! ADR-0019 task-notes write-back integration tests, on `tempfile::TempDir` fake
//! vaults (never a real vault): the first note creates a `## task-notes` section
//! with a per-task `### notes-<id>` heading and inserts an aliased in-page link
//! into the task line; a second note appends under the same heading with no new
//! link; and a concurrent edit is refused without mutating the file.

use std::fs;

use taski_daemon::{ApplyOutcome, index_note, process_add_note, scan_vault};
use taski_db as db;

/// The `### notes-<id>` heading the task's link points at, parsed from the task
/// line so tests don't hard-code the write-time-generated id.
fn link_id(note_line: &str) -> String {
    taski_core::notes_link_id(note_line).expect("task line carries a notes link")
}

#[test]
fn first_note_creates_section_and_links_task() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&root.join("n.db").to_string_lossy()).expect("open db");

    let note = root.join("projects.md");
    fs::write(&note, "# Projects\n\n- [ ] Redesign page ⏳ 2026-06-25\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let task = db::all_tasks(&conn).expect("all_tasks").remove(0);

    db::enqueue_add_note(
        &conn,
        task.id,
        &task.note_path,
        task.line_number,
        "went with hero",
    )
    .expect("enqueue");
    let action = db::pending_actions(&conn).expect("pending").remove(0);
    assert_eq!(
        process_add_note(&conn, root, &action).expect("process"),
        ApplyOutcome::Applied
    );

    let after = fs::read_to_string(&note).unwrap();
    let lines: Vec<&str> = after.lines().collect();
    let task_line = lines.iter().find(|l| l.contains("Redesign page")).unwrap();
    let id = link_id(task_line);

    // The link is in the description, before the ⏳ metadata.
    assert_eq!(
        *task_line,
        format!("- [ ] Redesign page [[#notes-{id}|Notes]] ⏳ 2026-06-25")
    );
    // The section + heading + bullet were appended.
    assert!(after.contains("## task-notes"));
    assert!(after.contains(&format!("### notes-{id}")));
    assert!(after.contains("- went with hero"));
    // The pre-existing prose is untouched.
    assert!(after.starts_with("# Projects\n"));
}

#[test]
fn second_note_appends_under_same_heading_without_a_new_link() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&root.join("n.db").to_string_lossy()).expect("open db");

    let note = root.join("projects.md");
    fs::write(&note, "- [ ] Redesign page ⏳ 2026-06-25\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");

    // First note.
    let task = db::all_tasks(&conn).expect("all_tasks").remove(0);
    db::enqueue_add_note(
        &conn,
        task.id,
        &task.note_path,
        task.line_number,
        "went with hero",
    )
    .expect("enqueue 1");
    let a1 = db::pending_actions(&conn).expect("pending").remove(0);
    assert_eq!(
        process_add_note(&conn, root, &a1).unwrap(),
        ApplyOutcome::Applied
    );
    db::resolve_action(&conn, a1.id, "done", None).unwrap();
    // The link edit changes the task body → re-index so the cached note_hash and
    // the (new) surrogate id reflect the written bytes (the drain loop does this).
    index_note(&conn, &note, root).expect("re-index");

    // Second note: re-fetch the task (its surrogate id changed with the body).
    let task = db::all_tasks(&conn).expect("all_tasks").remove(0);
    db::enqueue_add_note(
        &conn,
        task.id,
        &task.note_path,
        task.line_number,
        "approved by design",
    )
    .expect("enqueue 2");
    let a2 = db::pending_actions(&conn).expect("pending").remove(0);
    assert_eq!(
        process_add_note(&conn, root, &a2).unwrap(),
        ApplyOutcome::Applied
    );

    let after = fs::read_to_string(&note).unwrap();
    // Exactly ONE link on the task line (no second link accrued).
    assert_eq!(after.matches("[[#notes-").count(), 1);
    // Exactly ONE section + ONE heading.
    assert_eq!(after.matches("## task-notes").count(), 1);
    assert_eq!(after.matches("### notes-").count(), 1);
    // Both notes are present, grouped under the heading.
    assert!(after.contains("- went with hero"));
    assert!(after.contains("- approved by design"));
}

#[test]
fn refused_on_concurrent_edit_leaves_file_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let conn = db::open(&root.join("n.db").to_string_lossy()).expect("open db");

    let note = root.join("projects.md");
    fs::write(&note, "- [ ] Redesign page\n").unwrap();
    scan_vault(&conn, root, &[]).expect("scan");
    let task = db::all_tasks(&conn).expect("all_tasks").remove(0);
    db::enqueue_add_note(&conn, task.id, &task.note_path, task.line_number, "a note")
        .expect("enqueue");
    let action = db::pending_actions(&conn).expect("pending").remove(0);

    // Obsidian edits the note after the snapshot but before the daemon runs.
    let edited = "- [ ] Redesign page\nuser added this line\n";
    fs::write(&note, edited).unwrap();

    assert_eq!(
        process_add_note(&conn, root, &action).unwrap(),
        ApplyOutcome::ConflictNoteChanged
    );
    // The file is byte-for-byte the user's edit — never clobbered.
    assert_eq!(fs::read_to_string(&note).unwrap(), edited);
}
