//! Taski core domain types and the (Slice 0) line-based Markdown task parser.
//!
//! The parser is intentionally a thin, line-based implementation behind a single
//! public function ([`parse_tasks`]) so it can be swapped for a `pulldown-cmark`-based
//! implementation in Slice 1 without changing call sites.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

/// Checkbox-derived status of a task.
///
/// `Other` is a catch-all for any single checkbox character that is not one of the
/// recognised Obsidian states (e.g. `!`, `-`, `>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Open,
    Done,
    InProgress,
    Other(String),
}

impl Status {
    /// Map a raw checkbox character (e.g. `" "`, `"x"`, `"/"`) to a [`Status`].
    pub fn from_checkbox_char(ch: &str) -> Status {
        match ch {
            " " => Status::Open,
            "x" | "X" => Status::Done,
            "/" => Status::InProgress,
            other => Status::Other(other.to_string()),
        }
    }

    /// Render this status back to its canonical checkbox character.
    ///
    /// For `Other`, the original character is preserved. The canonical `Done` char
    /// emitted here is lowercase `x`.
    pub fn to_checkbox_char(&self) -> &str {
        match self {
            Status::Open => " ",
            Status::Done => "x",
            Status::InProgress => "/",
            Status::Other(ch) => ch.as_str(),
        }
    }
}

/// A single extracted task. See PRD §9 for the full field-by-field rationale.
#[derive(Debug, Clone)]
pub struct Task {
    /// DB-assigned surrogate identity (ADR-0005). The parser sets this to `0`; SQLite
    /// assigns the real `INTEGER PRIMARY KEY AUTOINCREMENT` rowid on INSERT. Once
    /// assigned, a task's id never changes and is never reused after deletion.
    pub id: i64,
    /// Source note (relative to vault root).
    pub note_path: String,
    /// 1-based line number within the note. Location, not identity.
    pub line_number: usize,
    /// Task body text (trimmed).
    pub text: String,
    /// Hash of the task text — the per-note reconciliation key (ADR-0005 §2). Two
    /// tasks with the same `text_hash` in the same note are considered the "same"
    /// task across re-scans (matched greedily in line order).
    pub text_hash: String,
    /// Checkbox-derived status.
    pub status: Status,
    /// Exact checkbox character as captured from the note.
    pub raw_checkbox_char: String,
    /// Note content hash captured at last scan (`None` in Slice 0).
    pub note_hash: Option<String>,
    /// Note mtime captured at last scan (`None` in Slice 0).
    pub note_mtime: Option<i64>,
    /// Parsed due date (`None` in Slice 0; Slice 1+ parses Tasks-plugin `📅`).
    pub due_date: Option<String>,
    /// Last-seen timestamp, unix seconds.
    pub updated_at: i64,
}

/// Parse Markdown into [`Task`]s for the given note path.
///
/// Slice 0: line-based recognition of `- [x] task body` style checkboxes. Task-like
/// lines that appear inside fenced code blocks are skipped. Identity/metadata fields
/// that require richer parsing (`note_hash`, `note_mtime`, `due_date`) are left as
/// `None` for Slice 0.
pub fn parse_tasks(markdown: &str, note_path: &str) -> Vec<Task> {
    let now = unix_now();
    let mut tasks = Vec::new();
    let mut in_fence = false;

    for (idx, raw_line) in markdown.lines().enumerate() {
        let line_number = idx + 1;
        let trimmed = raw_line.trim_start();
        if is_fence(trimmed) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(task) = parse_task_line(raw_line, note_path, line_number, now) {
            tasks.push(task);
        }
    }

    tasks
}

/// True if the (left-trimmed) line opens or closes a fenced code block.
fn is_fence(trimmed_line: &str) -> bool {
    trimmed_line.starts_with("```")
}

/// Try to interpret a single line as a task checkbox line.
fn parse_task_line(raw_line: &str, note_path: &str, line_number: usize, now: i64) -> Option<Task> {
    let (checkbox_char, body) = task_captures(raw_line)?;
    let body = body.trim();
    let status = Status::from_checkbox_char(checkbox_char);
    let text_hash = hash_str(body);

    Some(Task {
        id: 0, // placeholder — the DB assigns the surrogate rowid on INSERT (ADR-0005).
        note_path: note_path.to_string(),
        line_number,
        text: body.to_string(),
        text_hash,
        status,
        raw_checkbox_char: checkbox_char.to_string(),
        note_hash: None,
        note_mtime: None,
        due_date: extract_due_date(body),
        updated_at: now,
    })
}

/// Variation selector 16 (U+FE0F), optionally present after an emoji to request
/// emoji-style rendering.
const VS16: char = '\u{FE0F}';

/// Extract the first due date (`📅`/`📆`/`🗓` + optional VS16 + whitespace +
/// `YYYY-MM-DD`) from a task body, per the Obsidian Tasks emoji convention. Returns
/// the normalized `"YYYY-MM-DD"` string, or `None` if no valid due date is present.
///
/// Scans the body left-to-right for the first due-date emoji; at each, skips an
/// optional VS16, requires at least one whitespace char, then attempts to read a
/// strict `YYYY-MM-DD`. If the date is invalid (bad format or out-of-range month/day)
/// the scan continues to the next emoji. Trailing text after the date is allowed.
fn extract_due_date(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    for (emoji_off, ch) in body.char_indices() {
        if !matches!(ch, '📅' | '📆' | '🗓') {
            continue;
        }
        let mut pos = emoji_off + ch.len_utf8();

        // Optional variation selector 16.
        if let Some(rest) = body.get(pos..)
            && let Some(next) = rest.chars().next()
            && next == VS16
        {
            pos += VS16.len_utf8();
        }

        // Required whitespace (at least one).
        let ws_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos == ws_start {
            continue;
        }

        // Attempt YYYY-MM-DD at this position.
        if let Some(date) = parse_date_at(bytes, pos) {
            return Some(date);
        }
    }
    None
}

/// Read and validate a `YYYY-MM-DD` starting at `bytes[pos]`. Returns the normalized
/// date string if the format and ranges (month 1–12, day 1–31) are valid, else `None`.
fn parse_date_at(bytes: &[u8], pos: usize) -> Option<String> {
    if pos + 10 > bytes.len() {
        return None;
    }
    let s = &bytes[pos..pos + 10];

    // Format: dddd-dd-dd (ASCII digits and hyphens only).
    let digit = |i: usize| s[i].is_ascii_digit();
    if !(digit(0) && digit(1) && digit(2) && digit(3)) {
        return None;
    }
    if s[4] != b'-' || s[7] != b'-' {
        return None;
    }
    if !(digit(5) && digit(6) && digit(8) && digit(9)) {
        return None;
    }

    // Range validation (no calendar/leap-year logic — MVP).
    let month = (s[5] - b'0') * 10 + (s[6] - b'0');
    let day = (s[8] - b'0') * 10 + (s[9] - b'0');
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // All valid — build from verified-ASCII bytes.
    let mut date = String::with_capacity(10);
    for &b in s {
        date.push(b as char);
    }
    Some(date)
}

/// Match `^\s*[-*+]\s+\[(.)\]\s+(.+)$` for a single line, returning the two captured
/// slices (`checkbox_char`, `body`). Hand-rolled so we avoid a regex dependency in
/// Slice 0; safe because all indices land on UTF-8 char boundaries.
///
/// As a Slice 1 hardening, a leading run of blockquote markers (`>` optionally
/// followed by whitespace) is tolerated *before* the bullet — so `> - [ ] task` and
/// `> > - [x] nested` are recognised. Behaviour for ordinary lines is unchanged.
fn task_captures(line: &str) -> Option<(&str, &str)> {
    // Leading whitespace, then any run of `>` blockquote markers (each optionally
    // followed by whitespace). Leaves `i` at the first byte that is neither.
    let bytes = line.as_bytes();
    let mut i = 0;
    advance_past_leading_markers(bytes, &mut i);
    if i >= bytes.len() {
        return None;
    }

    // Bullet char: one of - * +.
    if !matches!(bytes[i], b'-' | b'*' | b'+') {
        return None;
    }
    i += 1;

    // Required whitespace (at least one) between bullet and checkbox.
    let ws_start = i;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i == ws_start || i >= bytes.len() {
        return None;
    }

    // `[` opener.
    if bytes[i] != b'[' {
        return None;
    }
    let char_start = i + 1;

    // Checkbox char = exactly one char.
    let rest = &line[char_start..];
    let checkbox = rest.chars().next()?;
    let char_len = checkbox.len_utf8();
    let close_idx = char_start + char_len;
    if line.as_bytes().get(close_idx) != Some(&b']') {
        return None;
    }

    // Required whitespace (at least one) after `]` before body.
    let mut j = close_idx + 1;
    let after_bracket = j;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j == after_bracket || j >= bytes.len() {
        return None;
    }

    let checkbox_str = &line[char_start..close_idx];
    let body = &line[j..];
    Some((checkbox_str, body))
}

/// Advance `i` past leading ASCII whitespace and any run of `>` blockquote markers
/// (each optionally followed by more whitespace). Stops at the first byte that is
/// neither whitespace nor part of a leading `>` run.
fn advance_past_leading_markers(bytes: &[u8], i: &mut usize) {
    loop {
        while *i < bytes.len() && bytes[*i].is_ascii_whitespace() {
            *i += 1;
        }
        if *i < bytes.len() && bytes[*i] == b'>' {
            *i += 1;
            continue;
        }
        break;
    }
}

fn hash_str(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_done_inprogress_and_skips_fenced() {
        let md = "\
# Daily

- [ ] First open task
- [x] Second, already done
- [/] Third, in progress

```text
- [ ] fake task inside a fence
```

some prose
";

        let tasks = parse_tasks(md, "Daily.md");
        assert_eq!(tasks.len(), 3, "should skip the fenced task");

        assert_eq!(tasks[0].line_number, 3);
        assert_eq!(tasks[0].status, Status::Open);
        assert_eq!(tasks[0].text, "First open task");
        assert_eq!(tasks[0].raw_checkbox_char, " ");
        assert_eq!(tasks[0].note_path, "Daily.md");
        assert!(tasks[0].note_hash.is_none());
        assert!(tasks[0].note_mtime.is_none());
        assert!(tasks[0].due_date.is_none());

        assert_eq!(tasks[1].status, Status::Done);
        assert_eq!(tasks[1].raw_checkbox_char, "x");
        assert_eq!(tasks[1].line_number, 4);

        assert_eq!(tasks[2].status, Status::InProgress);
        assert_eq!(tasks[2].raw_checkbox_char, "/");
        assert_eq!(tasks[2].line_number, 5);
    }

    #[test]
    fn status_round_trips_canonical_chars() {
        assert_eq!(Status::from_checkbox_char(" "), Status::Open);
        assert_eq!(Status::from_checkbox_char("x"), Status::Done);
        assert_eq!(Status::from_checkbox_char("X"), Status::Done);
        assert_eq!(Status::from_checkbox_char("/"), Status::InProgress);
        assert_eq!(
            Status::from_checkbox_char(">"),
            Status::Other(">".to_string())
        );

        assert_eq!(Status::Open.to_checkbox_char(), " ");
        assert_eq!(Status::Done.to_checkbox_char(), "x");
        assert_eq!(Status::InProgress.to_checkbox_char(), "/");
        assert_eq!(Status::Other(">".to_string()).to_checkbox_char(), ">");
    }

    #[test]
    fn ignores_non_task_lines_and_other_bullets() {
        let md = "\
plain text
- not a task (no checkbox)
* [ ]  star bullet works
+ [ ] plus bullet works
  - [ ] indented works
- [ ]no space after bracket is ignored
";
        let tasks = parse_tasks(md, "x.md");
        // star, plus, indented = 3 tasks; indented-with-no-space-after-`]` is rejected
        assert_eq!(tasks.len(), 3);
        assert!(tasks.iter().all(|t| t.status == Status::Open));
    }

    #[test]
    fn parsed_task_has_zero_id_placeholder() {
        let tasks_a = parse_tasks("- [ ] hello", "a.md");
        let tasks_b = parse_tasks("- [ ] hello", "a.md");
        assert_eq!(tasks_a.len(), 1);
        // The parser no longer generates identity — id is always 0; the DB assigns
        // the real surrogate rowid on INSERT (ADR-0005).
        assert_eq!(tasks_a[0].id, 0);
        assert_eq!(tasks_b[0].id, 0);
        // text_hash IS load-bearing — it's the reconciliation key.
        assert!(!tasks_a[0].text_hash.is_empty());
    }

    #[test]
    fn tolerates_leading_blockquote_markers() {
        let md = "\
> - [ ] quoted open
> > - [x] double-quoted done
- [ ] normal
>>> - [/] triple-quoted in progress
";

        let tasks = parse_tasks(md, "q.md");
        assert_eq!(
            tasks.len(),
            4,
            "blockquote-prefixed tasks should be recognised"
        );

        assert_eq!(tasks[0].status, Status::Open);
        assert_eq!(tasks[0].text, "quoted open");
        assert_eq!(tasks[1].status, Status::Done);
        assert_eq!(tasks[1].text, "double-quoted done");
        assert_eq!(tasks[2].status, Status::Open);
        assert_eq!(tasks[2].text, "normal");
        assert_eq!(tasks[3].status, Status::InProgress);
        assert_eq!(tasks[3].text, "triple-quoted in progress");
    }

    // --- extract_due_date / parse_date_at unit tests (Phase B) ---------------

    #[test]
    fn extract_due_date_plain_calendar_emoji() {
        assert_eq!(
            extract_due_date("ship it 📅 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_due_date_recognises_all_three_aliases() {
        assert_eq!(
            extract_due_date("📅 2025-01-01"),
            Some("2025-01-01".to_string())
        );
        assert_eq!(
            extract_due_date("📆 2025-01-02"),
            Some("2025-01-02".to_string())
        );
        assert_eq!(
            extract_due_date("🗓 2025-01-03"),
            Some("2025-01-03".to_string())
        );
    }

    #[test]
    fn extract_due_date_tolerates_variation_selector() {
        // VS16 (U+FE0F) immediately after the emoji is optional but accepted.
        assert_eq!(
            extract_due_date("task \u{1F4C5}\u{FE0F} 2025-06-15"),
            Some("2025-06-15".to_string())
        );
    }

    #[test]
    fn extract_due_date_allows_multiple_spaces() {
        assert_eq!(
            extract_due_date("📅   2025-07-04"),
            Some("2025-07-04".to_string())
        );
    }

    #[test]
    fn extract_due_date_none_when_no_emoji() {
        assert_eq!(extract_due_date("just a date 2025-01-01 here"), None);
    }

    #[test]
    fn extract_due_date_none_when_bad_format() {
        // Missing hyphens / wrong lengths are rejected; scan finds no valid date.
        assert_eq!(extract_due_date("📅 20250101"), None);
        assert_eq!(extract_due_date("📅 25-12-31"), None);
        assert_eq!(extract_due_date("📅 2025/12/31"), None);
    }

    #[test]
    fn extract_due_date_none_when_month_out_of_range() {
        assert_eq!(extract_due_date("📅 2025-13-01"), None);
        assert_eq!(extract_due_date("📅 2025-00-10"), None);
    }

    #[test]
    fn extract_due_date_none_when_day_out_of_range() {
        assert_eq!(extract_due_date("📅 2025-01-32"), None);
        assert_eq!(extract_due_date("📅 2025-01-00"), None);
    }

    #[test]
    fn extract_due_date_allows_trailing_text() {
        assert_eq!(
            extract_due_date("📅 2025-02-14 #high @home"),
            Some("2025-02-14".to_string())
        );
    }

    #[test]
    fn extract_due_date_takes_first_valid_emoji() {
        // First emoji has an invalid date; scan continues to the next valid one.
        assert_eq!(
            extract_due_date("📅 nope 📅 2025-03-17"),
            Some("2025-03-17".to_string())
        );
    }

    #[test]
    fn parse_task_line_wires_due_date_into_task() {
        let tasks = parse_tasks("- [ ] ship it 📅 2025-11-11\n", "d.md");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_date.as_deref(), Some("2025-11-11"));
    }
}
