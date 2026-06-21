//! Property test for the pure write-path linchpin `rewrite_cancelled_date`
//! (ADR-0013). This is the "never corrupts a line" contract for the
//! cancelled-date rewrite, mirroring `rewrite_done_date_proptest.rs`
//! byte-for-byte with `'✅'` (U+2705) → `'❌'` (U+274C).
//!
//! Property: for an arbitrary line and an arbitrary (valid-date-or-None)
//! `desired`, `rewrite_cancelled_date` **never panics** and:
//!   - if `Rewritten(s)` is returned, the result equals an INDEPENDENTLY-computed
//!     expected rewrite (byte-exact), the line has exactly the right number of
//!     `❌` (one after a stamp, zero after a clear), and
//!     `extract_cancelled_date(s)` reflects `desired` on a stamp;
//!   - if `Unchanged`, the call genuinely had nothing to do (no `❌` + None, or
//!     an already-matching date);
//!   - if `Unparseable`, the line carried a problematic `❌` (≥1 present but not
//!     a single clean token) — i.e. the function refused rather than guessed.

use proptest::prelude::*;

use taski_core::{RewriteResult, extract_cancelled_date, rewrite_cancelled_date};

/// Valid `YYYY-MM-DD` dates drawn from a fixed set of real anchors (incl. a leap
/// day and a div-by-400 leap day). Kept valid so the `Rewritten` postconditions
/// are meaningful; the malformed-`desired` path is covered by unit tests.
fn valid_date() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("2026-06-20".to_string()),
        Just("2024-02-29".to_string()), // leap day
        Just("2000-02-29".to_string()), // div-by-400 leap day
        Just("1999-12-31".to_string()),
        Just("2025-01-01".to_string()),
        Just("2031-07-04".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Independent byte-level helpers (cross-check the implementation)
// ---------------------------------------------------------------------------

/// Count raw `❌` (U+274C) occurrences, by bytes (independent of the impl's
/// char-based count).
fn cancelled_emoji_count(s: &str) -> usize {
    let needle = "\u{274C}".as_bytes();
    let bytes = s.as_bytes();
    let mut n = 0usize;
    let mut i = 0usize;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            n += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    n
}

/// Independently locate the byte range of the first parseable `❌` token
/// (`❌` + optional VS16 + ≥1 ASCII whitespace + strict `YYYY-MM-DD`), mirroring
/// the grammar but written from scratch so a bug in the shared helper is caught.
/// Returns `(emoji_start, date_end)` where the date occupies
/// `[date_end-10, date_end)`.
fn first_token_span(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let emoji = "\u{274C}".as_bytes();
    let vs16 = "\u{FE0F}".as_bytes();
    let mut i = 0usize;
    while i + emoji.len() <= bytes.len() {
        if &bytes[i..i + emoji.len()] != emoji {
            i += 1;
            continue;
        }
        let emoji_start = i;
        let mut pos = i + emoji.len();
        // Optional VS16.
        if pos + vs16.len() <= bytes.len() && &bytes[pos..pos + vs16.len()] == vs16 {
            pos += vs16.len();
        }
        // ≥1 ASCII whitespace.
        let ws_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos == ws_start {
            i += 1;
            continue;
        }
        // Strict YYYY-MM-DD at pos.
        if valid_date_at(bytes, pos) {
            return Some((emoji_start, pos + 10));
        }
        i += 1;
    }
    None
}

/// Independent strict `YYYY-MM-DD` check at `bytes[pos]`.
fn valid_date_at(bytes: &[u8], pos: usize) -> bool {
    if pos + 10 > bytes.len() {
        return false;
    }
    let s = &bytes[pos..pos + 10];
    let digit = |i: usize| s[i].is_ascii_digit();
    if !(digit(0) && digit(1) && digit(2) && digit(3)) {
        return false;
    }
    if s[4] != b'-' || s[7] != b'-' {
        return false;
    }
    if !(digit(5) && digit(6) && digit(8) && digit(9)) {
        return false;
    }
    let month = (s[5] - b'0') * 10 + (s[6] - b'0');
    let day = (s[8] - b'0') * 10 + (s[9] - b'0');
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Independently compute the expected `Rewritten` string, or `None` if the case
/// should NOT be a `Rewritten` result (it should be `Unchanged` or `Unparseable`
/// instead). Written from scratch as the oracle.
fn expected_rewrite(line: &str, desired: Option<&str>) -> Option<String> {
    let hc = cancelled_emoji_count(line);
    let token = first_token_span(line);
    match desired {
        None => match (hc, token) {
            (0, _) => None, // Unchanged
            (1, Some((start, end))) => {
                let bytes = line.as_bytes();
                let remove_start = if start > 0 && bytes[start - 1] == b' ' {
                    start - 1
                } else {
                    start
                };
                let mut out = String::with_capacity(line.len());
                out.push_str(&line[..remove_start]);
                out.push_str(&line[end..]);
                Some(out)
            }
            _ => None, // Unparseable
        },
        Some(date) => match (hc, token) {
            (0, None) => Some(format!("{line} \u{274C} {date}")),
            (1, Some((_, end))) => {
                let existing = &line[end - 10..end];
                if existing == date {
                    return None; // Unchanged
                }
                let mut out = String::with_capacity(line.len() + 10);
                out.push_str(&line[..end - 10]);
                out.push_str(date);
                out.push_str(&line[end..]);
                Some(out)
            }
            _ => None, // Unparseable
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn rewrite_cancelled_date_never_corrupts(
        line in ".*",
        desired in prop::option::of(valid_date()),
    ) {
        let result = rewrite_cancelled_date(&line, desired.as_deref());

        match &result {
            RewriteResult::Rewritten(new_line) => {
                // Byte-exact match against the independent oracle.
                let expected = expected_rewrite(&line, desired.as_deref())
                    .expect("Rewritten must correspond to a clean case the oracle handles");
                prop_assert_eq!(new_line, &expected, "rewritten line must equal the oracle output");

                // Cardinality + read-path invariants.
                match desired.as_deref() {
                    Some(d) => {
                        prop_assert_eq!(cancelled_emoji_count(new_line), 1, "stamp leaves exactly one ❌");
                        let parsed = extract_cancelled_date(new_line);
                        prop_assert_eq!(
                            parsed.as_deref(),
                            Some(d),
                            "the rewritten token parses back to the desired date"
                        );
                    }
                    None => {
                        prop_assert_eq!(cancelled_emoji_count(new_line), 0, "clear leaves no ❌");
                        prop_assert!(
                            extract_cancelled_date(new_line).is_none(),
                            "no cancelled date after clear"
                        );
                    }
                }
            }
            RewriteResult::Unchanged => {
                // Unchanged is only correct for the two genuine no-op cases.
                match desired.as_deref() {
                    Some(d) => {
                        let parsed = extract_cancelled_date(&line);
                        prop_assert_eq!(
                            parsed.as_deref(),
                            Some(d),
                            "Unchanged+Some requires the line to already carry that exact date"
                        );
                    }
                    None => prop_assert_eq!(
                        cancelled_emoji_count(&line),
                        0,
                        "Unchanged+None requires no ❌ on the line"
                    ),
                }
            }
            RewriteResult::Unparseable => {
                // Refusal is conservative: there must be at least one ❌ that we
                // declined to guess about. (A clean single token, or no ❌ at all,
                // would have been handled above.)
                prop_assert!(
                    cancelled_emoji_count(&line) >= 1,
                    "Unparseable requires ≥1 ❌ to refuse about; got line={:?}",
                    line
                );
            }
        }
    }
}
