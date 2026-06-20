//! 🔑 The integrity guarantee for ADR-0009 Phase 2 metadata write-back (the
//! first non-checkbox vault write). Mirrors `writeback_proptest.rs`.
//!
//! Property: for any generated note containing a task line (with arbitrary
//! existing trailing content — maybe a `⏳`/`📅`/tags/none), any
//! `desired: Option<date>`, and any chance of a concurrent external edit,
//! `process_metadata_action` **never corrupts** the note. Concretely, after
//! processing:
//!   - the note still exists and is valid UTF-8;
//!   - it has the same number of lines as the appropriate baseline (no lines
//!     added or dropped by Taski);
//!   - **either** (no concurrent edit) the on-disk note equals the original with
//!     ONLY the target line replaced by `taski_core::rewrite_scheduled`'s output
//!     (or is byte-identical to the original when the oracle returned `Unchanged`
//!     or `Unparseable`), every byte outside the target line preserved;
//!   - **or** (concurrent edit) the on-disk file equals the post-edit content
//!     byte-for-byte (Taski refused with `ConflictNoteChanged`).
//!
//! `taski_core::rewrite_scheduled` is the ORACLE for the expected line content;
//! the byte-level splice + line-count + no-collateral-damage invariants are
//! checked independently here. Runs only against `tempfile::TempDir` fake vaults.

use proptest::prelude::*;

use taski_core::{RewriteResult, extract_scheduled_date, rewrite_scheduled};
use taski_daemon::{ApplyOutcome, process_metadata_action};
use taski_db as db;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Lines that may appear before the anchor (vary anchor position 1–4).
fn prefix_line() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("# A heading".to_string()),
        Just("some prose here".to_string()),
        Just(String::new()), // blank line
        Just("> a quoted line".to_string()),
    ]
}

/// A small alphabet of plausible Markdown lines that may follow the anchor.
fn any_line() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("- [ ] another open task".to_string()),
        Just("  - [x] nested done task".to_string()),
        Just("- [/] in progress task".to_string()),
        Just("# A heading".to_string()),
        Just("some prose here".to_string()),
        Just(String::new()),
        Just("- bullet without checkbox".to_string()),
        Just("> a quoted line".to_string()),
    ]
}

/// The anchor's checkbox char.
fn anchor_char() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just(" "), Just("/"), Just("x"), Just("✓")]
}

/// Arbitrary trailing content on the anchor line — exercises the append, replace,
/// and remove code paths of `rewrite_scheduled` as `desired` varies. Includes
/// VS16 (emoji-presentation selector) `⏳` variants so the replace/remove paths
/// are exercised deterministically, not just statistically.
fn anchor_trailing() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(""),                                 // no metadata → append path
        Just("⏳ 2026-06-19"),                    // existing scheduled → replace/remove
        Just("⏳ 2026-06-20"),                    // may match desired → idempotent
        Just("⏳\u{FE0F} 2026-06-19"),            // VS16-present scheduled → replace/remove
        Just("⏳\u{FE0F} 2026-06-20"),            // VS16-present, may match → idempotent
        Just("📅 2026-07-01"),                    // due date must be preserved
        Just("#tag @home"),                       // tags must be preserved
        Just("📅 2026-07-01 #tag ⏳ 2026-06-19"), // mixed content
        Just("⏳ 2026-06-19 trailing words"),     // token with trailing text
    ]
}

/// What the TUI asks to set the scheduled date to.
fn desired() -> impl Strategy<Value = Option<&'static str>> {
    prop_oneof![
        Just(Some("2026-06-20")),
        Just(Some("2024-02-29")),
        Just(Some("2031-12-31")),
        Just(None),
    ]
}

/// Kinds of concurrent external edits to simulate (mirrors writeback_proptest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    None,
    AppendLine,
    InsertBeforeAnchor,
    ModifyOtherLine,
    ModifyAnchorLine,
}

fn edit_kind() -> impl Strategy<Value = EditKind> {
    prop_oneof![
        Just(EditKind::None),
        Just(EditKind::AppendLine),
        Just(EditKind::InsertBeforeAnchor),
        Just(EditKind::ModifyOtherLine),
        Just(EditKind::ModifyAnchorLine),
    ]
}

// ---------------------------------------------------------------------------
// Independent byte-level helpers (cross-check the daemon splice)
// ---------------------------------------------------------------------------

/// Byte offset of the start of `line_number` (1-based), counting `\n` boundaries.
fn line_start(content: &[u8], line_number: usize) -> usize {
    let mut start = 0usize;
    for _ in 1..line_number {
        match content[start..].iter().position(|&b| b == b'\n') {
            Some(pos) => start += pos + 1,
            None => break,
        }
    }
    start
}

/// Build the edited content for a concurrent edit kind. Always produces valid
/// UTF-8 that differs from the original by at least one byte (so the hash changes).
fn apply_edit(original: &str, edit: EditKind, anchor_line: usize, sep: &str) -> String {
    match edit {
        EditKind::None => original.to_string(),
        EditKind::AppendLine => format!("{original}EDIT{sep}"),
        EditKind::InsertBeforeAnchor => {
            let off = line_start(original.as_bytes(), anchor_line);
            format!("{}EDIT{}{}", &original[..off], sep, &original[off..])
        }
        EditKind::ModifyOtherLine => {
            let bytes = original.as_bytes();
            let total_lines = original.lines().count();
            for lineno in 1..=total_lines {
                if lineno == anchor_line {
                    continue;
                }
                let ls = line_start(bytes, lineno);
                let le = bytes[ls..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |p| ls + p);
                if let Some(rel) = bytes[ls..le].iter().position(|&b| b.is_ascii_alphabetic()) {
                    let abs = ls + rel;
                    let mut out = bytes.to_vec();
                    out[abs] ^= 0x20; // toggle ASCII case bit
                    return String::from_utf8(out).expect("case flip keeps valid UTF-8");
                }
            }
            format!("{original}EDIT{sep}")
        }
        EditKind::ModifyAnchorLine => {
            let off = line_start(original.as_bytes(), anchor_line);
            format!("{}Z{}", &original[..off], &original[off..])
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn metadata_writeback_never_corrupts(
        prefix in prop::collection::vec(prefix_line(), 0..3),
        extra  in prop::collection::vec(any_line(), 0..6),
        anchor_ch in anchor_char(),
        trailing in anchor_trailing(),
        desired in desired(),
        use_crlf in any::<bool>(),
        // A note may or may not end in a final line terminator. When false, the
        // anchor (if last) has no terminator at all — a realistic shape the code
        // must handle (`line_byte_range`'s last-line `end = bytes.len()`).
        trailing_newline in any::<bool>(),
        edit in edit_kind(),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let note_rel = "note.md";
        let note_abs = root.join(note_rel);

        let sep: &str = if use_crlf { "\r\n" } else { "\n" };
        let anchor_line = prefix.len() + 1; // 1-based

        // Build the note: prefix lines, then the anchor (with trailing content),
        // then extra lines. Optionally append a final line terminator.
        let mut lines: Vec<String> = prefix;
        lines.push(format!("- [{anchor_ch}] first task{trailing}"));
        lines.extend(extra);
        let joined = lines.join(sep);
        let original = if trailing_newline {
            format!("{joined}{sep}")
        } else {
            joined
        };
        std::fs::write(&note_abs, &original).unwrap();

        let conn = db::open(&tmp.path().join("p.db").to_string_lossy()).unwrap();
        taski_daemon::scan_vault(&conn, root).unwrap();

        // Locate the anchor task by its (always-constant) body stem.
        let tasks = db::all_tasks(&conn).unwrap();
        let anchor = tasks
            .iter()
            .find(|t| t.note_path == note_rel && t.text.starts_with("first task"))
            .expect("anchor task must be indexed");
        prop_assert_eq!(anchor.line_number, anchor_line);

        // Apply the concurrent edit (if any) AFTER the scan so the on-disk hash
        // diverges from the hash captured at scan time.
        let edited = apply_edit(&original, edit, anchor_line, sep);
        if edited != original {
            std::fs::write(&note_abs, &edited).unwrap();
        }

        // Enqueue + process the set_scheduled action.
        let _id = db::enqueue_set_scheduled(
            &conn, anchor.id, note_rel, anchor_line, desired,
        )
        .unwrap();
        let action = db::pending_actions(&conn).unwrap()[0].clone();
        let outcome = process_metadata_action(&conn, root, &action).unwrap();

        // The file still exists and is valid UTF-8.
        let on_disk: Vec<u8> = std::fs::read(&note_abs).unwrap();
        std::str::from_utf8(&on_disk).expect("note must remain valid UTF-8");

        if edit == EditKind::None {
            // No concurrent edit: hash matches, so the outcome is decided by the
            // pure oracle over the ORIGINAL anchor line.
            let ls = line_start(original.as_bytes(), anchor_line);
            let le = original.as_bytes()[ls..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(original.len(), |p| ls + p);
            // Mirror the implementation's CR-trimming: `line_byte_range` includes a
            // trailing `\r` in the line, but the read path (`str::lines()`) treats it
            // as part of the terminator. The oracle must operate on the CR-trimmed
            // content too, else the byte comparison is tautological on the CRLF axis
            // (both sides agree on the wrong boundary).
            let content_end = if le > ls && original.as_bytes()[le - 1] == b'\r' {
                le - 1
            } else {
                le
            };
            let orig_anchor_line = &original[ls..content_end];

            match rewrite_scheduled(orig_anchor_line, desired) {
                RewriteResult::Unchanged => {
                    // Idempotent: Applied, file byte-identical to the original.
                    prop_assert_eq!(outcome, ApplyOutcome::Applied);
                    prop_assert_eq!(&on_disk[..], original.as_bytes(),
                        "Unchanged must not write — file must equal the original");
                }
                RewriteResult::Unparseable => {
                    // Refused: file byte-identical to the original.
                    prop_assert_eq!(outcome, ApplyOutcome::MetadataUnparseable);
                    prop_assert_eq!(&on_disk[..], original.as_bytes(),
                        "Unparseable must not write — file must equal the original");
                }
                RewriteResult::Rewritten(new_line) => {
                    // Applied: ONLY the anchor content replaced by the oracle output;
                    // every other byte (and all line endings — including the `\r` in a
                    // CRLF note) preserved, since they live in `bytes[content_end..]`.
                    prop_assert_eq!(outcome, ApplyOutcome::Applied);
                    let mut expected = Vec::with_capacity(original.len() + new_line.len());
                    expected.extend_from_slice(&original.as_bytes()[..ls]);
                    expected.extend_from_slice(new_line.as_bytes());
                    expected.extend_from_slice(&original.as_bytes()[content_end..]);
                    prop_assert_eq!(
                        &on_disk[..], &expected[..],
                        "on apply the file must equal the original with ONLY the \
                         anchor line replaced by rewrite_scheduled's output"
                    );
                    // Line count preserved (variable-length splice must not add/drop
                    // a line).
                    let orig_nl = original.bytes().filter(|&b| b == b'\n').count();
                    let disk_nl = on_disk.iter().filter(|&&b| b == b'\n').count();
                    prop_assert_eq!(disk_nl, orig_nl,
                        "metadata write must not change newline count");

                    // 🔑 Independent of the byte-splice oracle: re-split the on-disk
                    // note the SAME way the read path does (`str::lines()` strips a
                    // `\r` adjacent to `\n`) and assert the anchor line carries NO
                    // interior `\r` and resolves to the desired scheduled date. This
                    // catches the CRLF append bug — where the `⏳` is written between
                    // the CR and the LF, leaving a literal CR permanently inside the
                    // line — which the byte-comparison alone cannot (the oracle would
                    // share the same off-by-CR boundary). Without the daemon-side CR
                    // trim, this assertion FAILS on CRLF + append cases.
                    let disk_str = std::str::from_utf8(&on_disk).unwrap();
                    let anchor_on_disk = disk_str
                        .lines()
                        .find(|l| l.contains("first task"))
                        .expect("anchor line must be present after rewrite");
                    prop_assert!(
                        !anchor_on_disk.contains('\r'),
                        "the rewritten anchor line must contain NO interior CR \
                         (CRLF append bug): {anchor_on_disk:?}"
                    );
                    match desired {
                        Some(d) => prop_assert_eq!(
                            extract_scheduled_date(anchor_on_disk),
                            Some(d.to_string()),
                            "the anchor line's scheduled date must match desired \
                             after rewrite"
                        ),
                        None => prop_assert_eq!(
                            extract_scheduled_date(anchor_on_disk),
                            None,
                            "unmark must clear the scheduled date on the anchor line"
                        ),
                    }

                    // And the read path now sees the desired scheduled date (or none
                    // after an unmark).
                    let reparsed = taski_core::parse_tasks(
                        std::str::from_utf8(&on_disk).unwrap(),
                        note_rel,
                    );
                    let re_anchor = reparsed
                        .iter()
                        .find(|t| t.text.starts_with("first task"))
                        .expect("anchor still parses after rewrite");
                    match desired {
                        Some(d) => prop_assert_eq!(re_anchor.scheduled_date.as_deref(), Some(d)),
                        None => prop_assert!(re_anchor.scheduled_date.is_none(),
                            "unmark must clear the scheduled date"),
                    }
                }
            }
        } else {
            // Any concurrent edit must be refused — the file equals the post-edit
            // content byte-for-byte (Taski changed nothing).
            prop_assert_eq!(
                outcome, ApplyOutcome::ConflictNoteChanged,
                "any concurrent edit must be refused, not clobbered"
            );
            prop_assert_eq!(
                &on_disk[..], edited.as_bytes(),
                "on refusal the file must equal the externally-edited content \
                 byte-for-byte (Taski changed nothing)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ADR-0009 §6 R3: recurring-task fixture (edge table promised "add one fixture")
// ---------------------------------------------------------------------------

/// A recurring task carries a `🔁 every <freq>` recurrence token that `rewrite_scheduled`
/// must treat as ORDINARY trailing content — preserved byte-for-byte — while only the
/// `⏳ <date>` scheduled-date token is replaced or removed. The recurrence glyph is
/// never mistaken for a scheduled-date token, and never dropped. This is a focused
/// unit test (not proptest) honoring the ADR's edge-table commitment.
#[test]
fn rewrite_scheduled_preserves_recurring_token() {
    let line = "- [ ] build feature 🔁 every day ⏳ 2026-06-19";

    // Replace the date: 🔁 recurrence preserved, only the ⏳ date swaps.
    assert_eq!(
        rewrite_scheduled(line, Some("2026-07-01")),
        RewriteResult::Rewritten("- [ ] build feature 🔁 every day ⏳ 2026-07-01".to_string())
    );

    // Remove the scheduled date: 🔁 recurrence preserved, ⏳ token gone.
    assert_eq!(
        rewrite_scheduled(line, None),
        RewriteResult::Rewritten("- [ ] build feature 🔁 every day".to_string())
    );

    // Idempotent: asking for the date already present is a no-op.
    assert_eq!(
        rewrite_scheduled(line, Some("2026-06-19")),
        RewriteResult::Unchanged
    );

    // Append when no ⏳ is present: the recurrence token is preserved and the new
    // ⏳ is appended at the end of the content.
    let recurring_no_sched = "- [ ] build feature 🔁 every day";
    assert_eq!(
        rewrite_scheduled(recurring_no_sched, Some("2026-07-01")),
        RewriteResult::Rewritten("- [ ] build feature 🔁 every day ⏳ 2026-07-01".to_string())
    );
}
