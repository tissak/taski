//! 🔑 The integrity guarantee for Taski write-back (ADR-0004 consequence),
//! amended for ADR-0012 (composed `✅` done-date stamp on Done flips).
//!
//! Property: for any generated note and any chance of a concurrent external edit,
//! `process_action_at` **never corrupts** the note. Concretely, after processing:
//!   - the note is still valid UTF-8 and still exists;
//!   - it has the same number of lines as the appropriate baseline (no lines added
//!     or dropped by Taski);
//!   - **either** (unchanged note) the file equals the original with the anchor
//!     checkbox char flipped AND, when the target is a Done char (`x`/`X`), a
//!     ` ✅ <fixed-date>` stamp appended to the anchor line (the composed
//!     done-date stamp from ADR-0012, in the same byte buffer as the flip),
//!     nothing else; **or** (concurrent edit) the file equals the post-edit
//!     content byte-for-byte (Taski refused).
//!
//! Cases exercised (safety-review hardening):
//!   - **L3** — the anchor task may sit on line 1–4 (0–3 prefix lines prepended);
//!   - **L4** — CRLF (`\r\n`) line endings, and a multi-byte checkbox char (`✓`,
//!     U+2713, 3 bytes UTF-8) to stress the byte-level surgery;
//!   - **L5** — realistic concurrent edits: append, insert-before-anchor,
//!     modify-other-line, modify-anchor-line, or none.
//!
//! This is the single most important test in the project. It runs only against
//! `tempfile::TempDir` fake vaults.

use proptest::prelude::*;

use taski_daemon::{ApplyOutcome, process_action_at, scan_vault};
use taski_db as db;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Lines that may appear before the anchor (L3: vary anchor position 1–4). None are
/// task-checkbox lines, so the anchor is always the only "first task" in the note.
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
        Just("- [ ] task with 📅 emoji body".to_string()),
    ]
}

/// The anchor's starting checkbox char (L4: include a multi-byte char).
fn anchor_char() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(" "), // 1 byte — the common open state
        Just("/"), // 1 byte — in-progress
        Just("✓"), // 3 bytes UTF-8 (U+2713) — multi-byte surgery
    ]
}

/// What the TUI flips the anchor to.
fn flip_target() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just("x"), Just(" ")]
}

/// Kinds of concurrent external edits to simulate (L5).
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
// Independent byte-level helpers (cross-check the daemon)
// ---------------------------------------------------------------------------

/// Byte offset of the start of `line_number` (1-based), counting `\n` boundaries —
/// the same notion as the daemon's `line_byte_range`.
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

/// Independently compute the byte range `(start, end)` of the single checkbox char on
/// `line_number`, or `None` if that line doesn't hold a `[<char>]` pattern.
fn anchor_checkbox_range(content: &[u8], line_number: usize) -> Option<(usize, usize)> {
    let start = line_start(content, line_number);
    let end = content[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(content.len(), |p| start + p);
    let line = &content[start..end];
    let bracket = line.iter().position(|&b| b == b'[')?;
    let char_start = start + bracket + 1;
    let rest = std::str::from_utf8(&content[char_start..end]).ok()?;
    let ch = rest.chars().next()?;
    let char_end = char_start + ch.len_utf8();
    (content.get(char_end) == Some(&b']')).then_some((char_start, char_end))
}

/// Build the edited content for a concurrent edit kind. Always produces valid UTF-8
/// that differs from the original by at least one byte (so the content hash changes).
fn apply_edit(original: &str, edit: EditKind, anchor_line: usize, sep: &str) -> String {
    match edit {
        EditKind::None => original.to_string(),
        EditKind::AppendLine => format!("{original}EDIT{sep}"),
        EditKind::InsertBeforeAnchor => {
            let off = line_start(original.as_bytes(), anchor_line);
            format!("{}EDIT{}{}", &original[..off], sep, &original[off..])
        }
        EditKind::ModifyOtherLine => {
            // Flip the case of the first ASCII letter on a non-anchor line.
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
            // Fallback: no suitable non-anchor line — append.
            format!("{original}EDIT{sep}")
        }
        EditKind::ModifyAnchorLine => {
            // Insert a byte at the start of the anchor line.
            let off = line_start(original.as_bytes(), anchor_line);
            format!("{}Z{}", &original[..off], &original[off..])
        }
    }
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn writeback_never_corrupts(
        prefix in prop::collection::vec(prefix_line(), 0..3),  // L3
        extra  in prop::collection::vec(any_line(), 0..6),
        anchor_ch in anchor_char(),                              // L4
        target_ch in flip_target(),
        use_crlf in any::<bool>(),                               // L4
        edit in edit_kind(),                                     // L5
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let note_rel = "note.md";
        let note_abs = root.join(note_rel);

        let sep: &str = if use_crlf { "\r\n" } else { "\n" };
        let anchor_line = prefix.len() + 1; // 1-based

        // Build the note: prefix lines, then the anchor, then extra lines.
        let mut lines: Vec<String> = prefix;
        lines.push(format!("- [{anchor_ch}] first task"));
        lines.extend(extra);
        let original = format!("{}{}", lines.join(sep), sep);
        std::fs::write(&note_abs, &original).unwrap();

        let conn = db::open(&tmp.path().join("p.db").to_string_lossy()).unwrap();
        scan_vault(&conn, root, &[]).unwrap();

        // Locate the anchor task by its (always-constant) body text.
        let tasks = db::all_tasks(&conn).unwrap();
        let anchor = tasks
            .iter()
            .find(|t| t.note_path == note_rel && t.text == "first task")
            .expect("anchor task must be indexed");
        prop_assert_eq!(anchor.line_number, anchor_line);
        prop_assert_eq!(&anchor.raw_checkbox_char, anchor_ch);

        // Apply the concurrent edit (if any) AFTER the scan so the on-disk hash
        // diverges from the hash captured at scan time.
        let edited = apply_edit(&original, edit, anchor_line, sep);
        if edited != original {
            std::fs::write(&note_abs, &edited).unwrap();
        }

        // Enqueue + process the flip. ADR-0012: process_action_at composes the
        // done-date stamp into the same splice as the flip when target is a Done
        // char (x/X). The fixed date keeps the byte-exact assertion deterministic.
        db::enqueue_action(
            &conn, anchor.id, note_rel, anchor_line, anchor_ch, target_ch,
        )
        .unwrap();
        let action = db::pending_actions(&conn).unwrap()[0].clone();
        let outcome = process_action_at(&conn, root, &action, "2026-06-20").unwrap();

        // The file still exists and is valid UTF-8.
        let on_disk: Vec<u8> = std::fs::read(&note_abs).unwrap();
        std::str::from_utf8(&on_disk).expect("note must remain valid UTF-8");

        if edit == EditKind::None {
            // MUST apply: file = original with the anchor checkbox char → target_ch
            // AND, when target_ch is a Done char, ` ✅ 2026-06-20` appended to the
            // anchor line (the composed done-date stamp, ADR-0012). Mirror
            // process_action_at's CR-trim + oracle discipline exactly.
            let ls = line_start(original.as_bytes(), anchor_line);
            let le = original.as_bytes()[ls..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(original.len(), |p| ls + p);
            // CR-trim: `line_byte_range` includes a trailing `\r`; the stamp goes
            // BEFORE the `\r\n`. Same discipline as process_action_at.
            let content_end = if le > ls && original.as_bytes()[le - 1] == b'\r' {
                le - 1
            } else {
                le
            };
            let orig_line = &original[ls..content_end];

            // Flip the checkbox char in the decoded line.
            let (cs, ce) = anchor_checkbox_range(original.as_bytes(), anchor_line)
                .expect("anchor must have a checkbox in the original");
            let target_c = target_ch.chars().next().unwrap();
            let rel_cs = cs - ls;
            let rel_ce = ce - ls;
            let mut flipped = String::with_capacity(orig_line.len() + 4);
            flipped.push_str(&orig_line[..rel_cs]);
            flipped.push(target_c);
            flipped.push_str(&orig_line[rel_ce..]);

            // Apply the done-date oracle exactly as process_action_at does. The
            // generated anchor never carries a ✅ (anchor_trailing has none), so
            // Unparseable is impossible here — Unchanged on clear, Rewritten on stamp.
            let final_line = if matches!(target_c, 'x' | 'X') || target_c == ' ' {
                let desired: Option<&str> = if matches!(target_c, 'x' | 'X') {
                    Some("2026-06-20")
                } else {
                    None
                };
                match taski_core::rewrite_done_date(&flipped, desired) {
                    taski_core::RewriteResult::Rewritten(s) => s,
                    taski_core::RewriteResult::Unchanged => flipped,
                    taski_core::RewriteResult::Unparseable => unreachable!(
                        "clean anchor (no ✅) is never Unparseable"
                    ),
                }
            } else {
                // Non-done/non-open target (not generated here, but kept for parity).
                flipped
            };

            let mut expected = Vec::with_capacity(original.len() + final_line.len());
            expected.extend_from_slice(&original.as_bytes()[..ls]);
            expected.extend_from_slice(final_line.as_bytes());
            expected.extend_from_slice(&original.as_bytes()[content_end..]);

            prop_assert_eq!(outcome, ApplyOutcome::Applied);
            prop_assert_eq!(
                &on_disk[..], &expected[..],
                "on apply the file must equal the original with the flip + composed \
                 done-date stamp (when target is Done)"
            );
            // Newline count preserved (the splice touches only the anchor line content).
            let orig_nl = original.bytes().filter(|&b| b == b'\n').count();
            let disk_nl = on_disk.iter().filter(|&&b| b == b'\n').count();
            prop_assert_eq!(disk_nl, orig_nl, "apply must not change newline count");
        } else {
            // MUST refuse: file equals the post-edit content byte-for-byte.
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
