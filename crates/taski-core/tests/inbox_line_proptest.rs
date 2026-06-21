//! Property test for the pure ADR-0014 quick-add construction oracle
//! `inbox_line_for`. Simpler than the rewrite-oracle proptests — this is a
//! construction function (build a line), not a rewrite (edit an existing line).
//!
//! Property: for an arbitrary `text` and a valid `YYYY-MM-DD` `today`,
//! `inbox_line_for(text, today)` **never panics** and:
//!   - the result starts with `- [ ] `;
//!   - the result ends with ` ➕ <today>`;
//!   - the `➕` created date parses back to `today` via `extract_created_date`;
//!   - the result is a single line (no embedded `\n`/`\r`), even if `text`
//!     contained them;
//!   - the result contains the cleaned `text` verbatim (text containing
//!     `✅`/`❌`/`📅`/`⏳` is preserved).

use proptest::prelude::*;

use taski_core::{extract_created_date, inbox_line_for};

/// Valid `YYYY-MM-DD` dates drawn from a fixed set of real anchors.
fn valid_date() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("2026-06-21".to_string()),
        Just("2024-02-29".to_string()), // leap day
        Just("2000-02-29".to_string()), // div-by-400 leap day
        Just("1999-12-31".to_string()),
        Just("2025-01-01".to_string()),
        Just("2031-07-04".to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn inbox_line_for_produces_canonical_task_line(
        text in ".*",
        today in valid_date(),
    ) {
        let line = inbox_line_for(&text, &today);

        // 1. Canonical prefix: open checkbox.
        prop_assert!(
            line.starts_with("- [ ] "),
            "must start with `- [ ] `: {line:?}"
        );

        // 2. Canonical suffix: ` ➕ <today>`.
        let suffix = format!(" \u{2795} {today}");
        prop_assert!(
            line.ends_with(&suffix),
            "must end with ` ➕ {today}`: {line:?}"
        );

        // 3. The created date parses back to `today` (read-path invariant).
        prop_assert_eq!(
            extract_created_date(&line),
            Some(today.clone()),
            "the ➕ token must parse back to today"
        );

        // 4. Single-line: no embedded newlines survive the strip (the modal is
        //    single-line only).
        prop_assert!(
            !line.contains('\n') && !line.contains('\r'),
            "result must be a single line (embedded newlines stripped): {line:?}"
        );

        // 5. The cleaned text appears verbatim between the prefix and suffix.
        let cleaned: String = text.chars().filter(|&c| c != '\n' && c != '\r').collect();
        let expected = format!("- [ ] {cleaned} \u{2795} {today}");
        prop_assert_eq!(&line, &expected, "byte-exact match against the expected line");
    }

    /// Empty text still produces valid syntax (the user can fill in the body in
    /// Obsidian). The created date must still parse.
    #[test]
    fn inbox_line_for_empty_text_is_valid(
        today in valid_date(),
    ) {
        let line = inbox_line_for("", &today);
        prop_assert_eq!(&line, &format!("- [ ]  \u{2795} {today}"));
        prop_assert_eq!(
            extract_created_date(&line),
            Some(today.clone()),
            "empty-text line still has a parseable ➕ date"
        );
    }
}
