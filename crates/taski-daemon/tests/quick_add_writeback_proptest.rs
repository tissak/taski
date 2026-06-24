//! 🔑 The integrity guarantee for ADR-0014 quick-add write-back (the inbox
//! creation feature). Mirrors `writeback_proptest.rs` / `metadata_writeback_proptest.rs`.
//!
//! Property: for any generated pre-existing inbox content (or no inbox at all)
//! and any typed text, `process_quick_add_at` **never corrupts** the inbox and
//! always produces the canonical `- [ ] <text> ➕ <today>` task line.
//! Concretely, after processing:
//!   - the inbox exists and is valid UTF-8;
//!   - **either** (inbox existed) the on-disk inbox equals the original with
//!     exactly `- [ ] <text> ➕ <today>\n` appended at the end, every original
//!     byte preserved;
//!   - **or** (inbox absent) the inbox is created with exactly
//!     `- [ ] <text> ➕ <today>\n` (one line, one terminator).
//!
//! `taski_core::inbox_line_for` is the ORACLE for the expected appended line;
//! the byte-level splice + created-date round-trip invariants are checked
//! independently here. Runs only against `tempfile::TempDir` fake vaults.
//!
//! The TOCTOU guard (`atomic_write`'s content-hash re-check) is already proven
//! by `writeback_proptest` — it's the SAME code path. Quick_add reads the file
//! fresh and hashes it at processing time (no stale scan hash), so a pre-applied
//! "edit" is indistinguishable from "different starting content" in a
//! single-threaded test. The companion `quick_add_undo_*` tests verify the undo
//! path's integrity, including a genuine last-line-mismatch refusal.

use proptest::prelude::*;

use taski_core::{extract_created_date, inbox_line_for, parse_tasks};
use taski_daemon::{ApplyOutcome, process_quick_add_at, process_quick_add_undo_at};
use taski_db as db;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// A small alphabet of plausible Markdown lines that may pre-exist in the inbox.
/// Exercises empty file, single-line, multi-line, with/without trailing newline.
fn inbox_line() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("# Inbox".to_string()),
        Just("- [ ] existing open task".to_string()),
        Just("- [x] done task".to_string()),
        Just("some prose here".to_string()),
        Just(String::new()), // blank line
        Just("> a quoted line".to_string()),
        Just("- bullet without checkbox".to_string()),
    ]
}

/// Text the user types into the quick-add modal. Includes ASCII, Unicode,
/// emoji, tags, and empty (after stripping). Strips newlines to mirror
/// `inbox_line_for`'s single-line discipline.
fn typed_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("buy milk".to_string()),
        Just("call mom #family".to_string()),
        Just("fix bug in parser @work".to_string()),
        Just("résumé review".to_string()), // non-ASCII
        Just("review PR 🎉".to_string()),  // emoji
        Just("2026-06-21 due today 📅 2026-06-21".to_string()), // contains a date token
        Just("   ".to_string()),           // whitespace-only → trimmed to empty
        Just("".to_string()),              // empty
    ]
}

/// Whether the inbox pre-exists, and if so its trailing-newline shape.
fn inbox_shape() -> impl Strategy<Value = InboxShape> {
    prop_oneof![
        Just(InboxShape::Absent),
        Just(InboxShape::Present {
            trailing_newline: true
        }),
        Just(InboxShape::Present {
            trailing_newline: false
        }),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboxShape {
    Absent,
    Present { trailing_newline: bool },
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn quick_add_never_corrupts(
        pre_lines in prop::collection::vec(inbox_line(), 0..6),
        shape  in inbox_shape(),
        text   in typed_text(),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inbox_rel = "task-inbox.md";
        let inbox_abs = root.join(inbox_rel);
        let today = "2026-06-21";

        // Build the pre-existing inbox (if any).
        let original: String = match shape {
            InboxShape::Absent => String::new(), // marker: file does not exist
            InboxShape::Present { trailing_newline } => {
                let joined = pre_lines.join("\n");
                if trailing_newline {
                    format!("{joined}\n")
                } else {
                    joined
                }
            }
        };
        let inbox_existed = matches!(shape, InboxShape::Present { .. });
        if inbox_existed {
            std::fs::write(&inbox_abs, &original).unwrap();
        }

        // Enqueue + process the quick_add action. We bypass scan entirely
        // (quick_add carries its own inbox_path + payload, no task_id needed).
        let conn = db::open(&tmp.path().join("p.db").to_string_lossy()).unwrap();
        let _id = db::enqueue_quick_add(&conn, inbox_rel, &text).unwrap();
        let action = db::pending_actions(&conn).unwrap()[0].clone();
        let outcome = process_quick_add_at(root, &action, today).unwrap();

        // The file always exists now (either it pre-existed, or it was just
        // created).
        let on_disk: Vec<u8> = std::fs::read(&inbox_abs).unwrap();
        std::str::from_utf8(&on_disk).expect("inbox must remain valid UTF-8");

        // The expected line (oracle). The daemon passes `action.payload` (the
        // raw enqueued text) directly to `inbox_line_for`; trimming happens in
        // the TUI before enqueue, so the daemon sees whatever was stored.
        let expected_line = inbox_line_for(&text, today);

        if inbox_existed {
            // Existing inbox → appended with the full TOCTOU guard.
            prop_assert_eq!(outcome, ApplyOutcome::Applied);
            let mut expected_bytes = original.clone().into_bytes();
            // The implementation prepends a newline if the file has content but
            // no trailing newline; an empty file gets the line directly.
            if !original.is_empty() && !original.ends_with('\n') {
                expected_bytes.push(b'\n');
            }
            expected_bytes.extend_from_slice(expected_line.as_bytes());
            expected_bytes.push(b'\n');
            prop_assert_eq!(
                &on_disk[..], &expected_bytes[..],
                "on apply the inbox must equal the original with exactly \
                 `- [ ] <text> ➕ <today>\\n` appended; every original byte preserved"
            );
        } else {
            // Absent inbox → first-creation via atomic_create. No TOCTOU (by
            // construction nothing exists to conflict with).
            prop_assert_eq!(outcome, ApplyOutcome::Applied);
            let mut expected_bytes = Vec::new();
            expected_bytes.extend_from_slice(expected_line.as_bytes());
            expected_bytes.push(b'\n');
            prop_assert_eq!(
                &on_disk[..], &expected_bytes[..],
                "first-creation must write exactly `- [ ] <text> ➕ <today>\\n`"
            );
        }

        // 🔑 Cross-check: the on-disk file parses and the LAST task carries the
        // expected created date. Empty/whitespace text yields a `- [ ]  ➕ <date>`
        // line with empty/spaced body — still valid Markdown, still parses.
        let disk_str = std::str::from_utf8(&on_disk).unwrap();
        let tasks = parse_tasks(disk_str, inbox_rel);
        let last = tasks.last().expect("at least one task must parse");
        prop_assert_eq!(
            extract_created_date(&inbox_line_for(&text, today)),
            Some(today.to_string()),
            "oracle sanity: the expected line's created date round-trips"
        );
        prop_assert_eq!(
            last.created_date.as_deref(),
            Some(today),
            "the appended task's created_date must equal today"
        );
    }
}

// ---------------------------------------------------------------------------
// Undo: focused unit-style tests (deterministic, not proptest)
// ---------------------------------------------------------------------------

/// A successful undo removes EXACTLY the last line — the original content is
/// restored byte-for-byte (the canonical happy path).
#[test]
fn quick_add_undo_removes_exactly_the_last_line() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let inbox_rel = "task-inbox.md";
    let inbox_abs = root.join(inbox_rel);
    let today = "2026-06-21";
    let original = "# Inbox\n- [ ] existing\n";
    std::fs::write(&inbox_abs, original).unwrap();

    let conn = db::open(&tmp.path().join("p.db").to_string_lossy()).unwrap();

    // 1. Apply a quick_add.
    let _id = db::enqueue_quick_add(&conn, inbox_rel, "buy milk").unwrap();
    let action = db::pending_actions(&conn).unwrap()[0].clone();
    let outcome = process_quick_add_at(root, &action, today).unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);

    // 2. Apply the corresponding undo.
    let _undo_id = db::enqueue_quick_add_undo(&conn, inbox_rel, "buy milk").unwrap();
    let undo_action = db::pending_actions(&conn).unwrap()[0].clone();
    let undo_outcome = process_quick_add_undo_at(root, &undo_action, today).unwrap();
    assert_eq!(undo_outcome, ApplyOutcome::Applied);

    // 3. The file is byte-identical to the original.
    let on_disk = std::fs::read(&inbox_abs).unwrap();
    assert_eq!(
        &on_disk[..],
        original.as_bytes(),
        "undo must restore the original content byte-for-byte"
    );
}

/// A concurrent edit between add and undo is refused with `ConflictNoteChanged`;
/// the file equals the post-edit content (Taski changed nothing on undo). This is
/// a GENUINE last-line-mismatch refusal — the undo expected `- [ ] buy milk ➕ ...`
/// as the last line, but the last line was externally tampered.
#[test]
fn quick_add_undo_refused_on_last_line_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let inbox_rel = "task-inbox.md";
    let inbox_abs = root.join(inbox_rel);
    let today = "2026-06-21";
    std::fs::write(&inbox_abs, "# Inbox\n").unwrap();

    let conn = db::open(&tmp.path().join("p.db").to_string_lossy()).unwrap();

    // 1. Apply a quick_add.
    let _id = db::enqueue_quick_add(&conn, inbox_rel, "buy milk").unwrap();
    let action = db::pending_actions(&conn).unwrap()[0].clone();
    process_quick_add_at(root, &action, today).unwrap();

    // 2. Concurrent external edit replaces the appended line with something else.
    let edited = "# Inbox\n- [ ] TAMPERED LINE\n";
    std::fs::write(&inbox_abs, edited).unwrap();

    // 3. The undo is refused because the last line doesn't match the expected
    //    `- [ ] buy milk ➕ 2026-06-21`.
    let _undo_id = db::enqueue_quick_add_undo(&conn, inbox_rel, "buy milk").unwrap();
    let undo_action = db::pending_actions(&conn).unwrap()[0].clone();
    let undo_outcome = process_quick_add_undo_at(root, &undo_action, today).unwrap();
    assert_eq!(undo_outcome, ApplyOutcome::ConflictNoteChanged);

    // The file equals the externally-edited content byte-for-byte.
    let on_disk = std::fs::read(&inbox_abs).unwrap();
    assert_eq!(
        &on_disk[..],
        edited.as_bytes(),
        "on refused undo the file must equal the externally-edited content"
    );
}

/// Undo on a non-existent inbox returns `TaskNotFound` (nothing to undo).
#[test]
fn quick_add_undo_task_not_found_when_inbox_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let inbox_rel = "task-inbox.md";
    let today = "2026-06-21";

    let conn = db::open(&tmp.path().join("p.db").to_string_lossy()).unwrap();
    let _undo_id = db::enqueue_quick_add_undo(&conn, inbox_rel, "buy milk").unwrap();
    let undo_action = db::pending_actions(&conn).unwrap()[0].clone();
    let outcome = process_quick_add_undo_at(root, &undo_action, today).unwrap();
    assert_eq!(outcome, ApplyOutcome::TaskNotFound);
}

/// Regression: when the inbox carries a `## task-notes` section (ADR-0019), a new
/// quick-add task line joins the task list ABOVE the section — it must never be
/// appended under a note. The blank-line separator before the section is kept.
#[test]
fn quick_add_inserts_above_task_notes_section() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let inbox_rel = "task-inbox.md";
    let inbox_abs = root.join(inbox_rel);
    let today = "2026-06-21";
    // An inbox with one task that already has a note grouped under `## task-notes`.
    let original = "# Inbox\n- [ ] existing [[#notes-1|Notes]]\n\n## task-notes\n\n### notes-1\n- a note\n";
    std::fs::write(&inbox_abs, original).unwrap();

    let conn = db::open(&tmp.path().join("p.db").to_string_lossy()).unwrap();
    let _id = db::enqueue_quick_add(&conn, inbox_rel, "buy milk").unwrap();
    let action = db::pending_actions(&conn).unwrap()[0].clone();
    let outcome = process_quick_add_at(root, &action, today).unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);

    let on_disk = std::fs::read_to_string(&inbox_abs).unwrap();
    let expected = format!(
        "# Inbox\n- [ ] existing [[#notes-1|Notes]]\n{}\n\n## task-notes\n\n### notes-1\n- a note\n",
        inbox_line_for("buy milk", today)
    );
    assert_eq!(
        on_disk, expected,
        "new task must land in the task list above `## task-notes`, not under the note"
    );

    // The note section is unchanged: still exactly one `### notes-` heading and
    // the new task line is not parsed as a note bullet.
    let tasks = parse_tasks(&on_disk, inbox_rel);
    assert_eq!(tasks.len(), 2, "exactly the two checkbox tasks are parsed");

    // Undo must remove exactly the line it inserted (above the section), not the
    // file's true last line — restoring the original byte-for-byte.
    let _undo_id = db::enqueue_quick_add_undo(&conn, inbox_rel, "buy milk").unwrap();
    let undo_action = db::pending_actions(&conn).unwrap()[0].clone();
    let undo_outcome = process_quick_add_undo_at(root, &undo_action, today).unwrap();
    assert_eq!(undo_outcome, ApplyOutcome::Applied);
    let after_undo = std::fs::read_to_string(&inbox_abs).unwrap();
    assert_eq!(
        after_undo, original,
        "undo of a task-list-aware insert must restore the original content"
    );
}
