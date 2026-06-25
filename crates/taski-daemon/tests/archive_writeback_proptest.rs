//! 🔑 The integrity guarantee for ADR-0021 archive write-back (the copy-then-delete
//! move of completed tasks). Mirrors `quick_add_writeback_proptest.rs`.
//!
//! Property: for any generated flat source note (mixing open / done / in-progress /
//! cancelled tasks and non-task lines) and any archive shape (absent or pre-existing),
//! `process_archive` **never loses a task** and is exactly:
//!   - source after  == `remove_lines(source_before, completed)`   (Phase B oracle),
//!   - archive after  == archive_before ⧺ `extract_lines(source_before, completed)`
//!     appended verbatim, each on its own LF line                  (Phase A).
//!
//! `taski_core::remove_lines` / `extract_lines` are the ORACLES (proven never-corrupts
//! in `taski-core`); this test proves the daemon wires them correctly across two files
//! under the hash gate, including the first-creation path for an absent archive. Runs
//! only against `tempfile::TempDir` fake vaults.
//!
//! The TOCTOU guard (`atomic_write`'s content-hash re-check) is already proven by
//! `writeback_proptest` — the same code path. Single-threaded, so no concurrent edit
//! is injected here; the `archive.rs` integration tests cover the conflict-refusal.

use proptest::prelude::*;

use taski_core::{Status, extract_lines, remove_lines};
use taski_daemon::{ApplyOutcome, process_archive, scan_vault};
use taski_db as db;

/// One flat source line: a checkbox task in one of the four statuses, or a non-task
/// line. Bodies are distinct enough to keep the round-trip unambiguous.
fn source_line() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("- [ ] open a".to_string()),
        Just("- [x] done b".to_string()),
        Just("- [/] wip c".to_string()),
        Just("- [-] cancelled d".to_string()),
        Just("# heading".to_string()),
        Just("plain prose".to_string()),
    ]
}

/// Whether the archive pre-exists, and its trailing-newline shape.
#[derive(Debug, Clone)]
enum ArchiveShape {
    Absent,
    Present(String),
}

fn archive_shape() -> impl Strategy<Value = ArchiveShape> {
    prop_oneof![
        Just(ArchiveShape::Absent),
        Just(ArchiveShape::Present("- [x] older archived\n".to_string())),
        Just(ArchiveShape::Present("- [x] no trailing nl".to_string())),
    ]
}

/// Replicates the daemon's `append_archived_block`: append each block line on its own
/// LF-terminated line, prepending a `\n` if the existing content lacks one.
fn expected_archive(orig: &str, block: &[String]) -> String {
    let mut out = orig.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for l in block {
        out.push_str(l);
        out.push('\n');
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn archive_never_loses_a_task(
        lines in prop::collection::vec(source_line(), 1..7),
        source_trailing_nl in any::<bool>(),
        arch in archive_shape(),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let source_rel = "task-inbox.md";
        let archive_rel = "task-archive.md";
        let source_abs = root.join(source_rel);
        let archive_abs = root.join(archive_rel);

        // Build the source note.
        let joined = lines.join("\n");
        let source_before = if source_trailing_nl { format!("{joined}\n") } else { joined };
        std::fs::write(&source_abs, &source_before).unwrap();

        // Build the archive (if pre-existing).
        let archive_before = match &arch {
            ArchiveShape::Absent => None,
            ArchiveShape::Present(c) => {
                std::fs::write(&archive_abs, c).unwrap();
                Some(c.clone())
            }
        };

        // Index so the source tasks carry their cached note hash (ADR-0006).
        let conn = db::open(&root.join("p.db").to_string_lossy()).unwrap();
        scan_vault(&conn, root, &[]).unwrap();

        // The completed flat task lines, as the TUI would compute them.
        let mut completed: Vec<(i64, usize)> = db::all_tasks(&conn)
            .unwrap()
            .into_iter()
            .filter(|t| {
                t.note_path == source_rel
                    && t.indent == 0
                    && (matches!(t.status, Status::Done) || t.raw_checkbox_char == "-")
            })
            .map(|t| (t.id, t.line_number))
            .collect();
        completed.sort_by_key(|(_, ln)| *ln);
        // The TUI only enqueues when there is something to archive.
        prop_assume!(!completed.is_empty());

        let lines_to_move: Vec<usize> = completed.iter().map(|(_, ln)| *ln).collect();
        db::enqueue_archive(
            &conn,
            completed[0].0,
            source_rel,
            completed[0].1,
            archive_rel,
            &lines_to_move,
        )
        .unwrap();
        let action = db::pending_actions(&conn).unwrap().pop().unwrap();
        let outcome = process_archive(&conn, root, &action).unwrap();
        prop_assert_eq!(outcome, ApplyOutcome::Applied);

        // Phase B: the source equals the pure deletion oracle.
        let source_after = std::fs::read_to_string(&source_abs).unwrap();
        prop_assert_eq!(
            &source_after,
            &remove_lines(&source_before, &lines_to_move),
            "source must equal remove_lines(source_before, completed)"
        );

        // Phase A: the archive equals existing content ⧺ the extracted block, verbatim.
        let block = extract_lines(&source_before, &lines_to_move);
        let archive_after = std::fs::read_to_string(&archive_abs).unwrap();
        let expected = expected_archive(archive_before.as_deref().unwrap_or(""), &block);
        prop_assert_eq!(&archive_after, &expected, "archive must equal prior ⧺ block");

        // No loss: every moved line's content is present in the archive, and the
        // count of surviving source lines + moved lines == original line count.
        prop_assert_eq!(
            source_after.lines().count() + lines_to_move.len(),
            source_before.lines().count(),
            "removed-count is exact; nothing dropped silently"
        );
    }
}
