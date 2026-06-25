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

/// Obsidian Tasks-plugin priority (Tier 1 read path). The five variants map 1:1
/// to the bare priority emojis; see [`Priority::from_emoji`] for the canonical
/// mapping (source-verified against `Priority.ts` + `DefaultTaskSerializer.ts`
/// in the Obsidian Tasks plugin).
///
/// `Other` is reserved for future round-trip safety (an unknown glyph the user
/// *did* mean as priority). It is **never produced by [`extract_priority`]** —
/// there is no reliable way to know an unknown bare emoji was meant as a
/// priority marker, so the extractor returns `None` rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Priority {
    /// `🔺` (U+1F53A) — highest priority.
    Highest,
    /// `⏫` (U+23EB) — high priority. (NOT Highest — common mix-up source.)
    High,
    /// `🔼` (U+1F53C) — medium priority.
    Medium,
    /// `🔽` (U+1F53D) — low priority.
    Low,
    /// `⏬` (U+23EC) — lowest priority.
    Lowest,
    /// Reserved for future round-trip safety: an unknown priority glyph. Not
    /// produced by [`extract_priority`]; kept here so future code can model
    /// glyphs Taski doesn't yet understand without redefining the enum.
    Other(String),
}

/// Canonical emoji → [`Priority`] lookup table. The single source of truth for
/// the mapping; the extractor and the DB read/write paths both go through the
/// helpers below.
///
/// ⚠️ `⏫` is **High**, not Highest. `🔺` is Highest. This was verified against
/// `Priority.ts` + `DefaultTaskSerializer.ts` in the Obsidian Tasks plugin.
const PRIORITY_EMOJIS: &[(char, Priority)] = &[
    ('\u{1F53A}', Priority::Highest), // 🔺
    ('\u{23EB}', Priority::High),     // ⏫
    ('\u{1F53C}', Priority::Medium),  // 🔼
    ('\u{1F53D}', Priority::Low),     // 🔽
    ('\u{23EC}', Priority::Lowest),   // ⏬
];

impl Priority {
    /// Map a single char to a known [`Priority`] variant. Returns `None` for any
    /// char that is not one of the five canonical priority emojis; the caller
    /// (extractor, DB read path) decides whether to treat `None` as
    /// "no priority" or to wrap as [`Priority::Other`]. The extractor does the
    /// former; `Other` is reserved for future use.
    pub fn from_emoji(ch: char) -> Option<Priority> {
        PRIORITY_EMOJIS
            .iter()
            .find(|(c, _)| *c == ch)
            .map(|(_, p)| p.clone())
    }

    /// Render this priority back to its canonical emoji char, or `None` for
    /// [`Priority::Other`] (unknown glyph has no canonical form to emit). Used
    /// by the DB write path to store the emoji glyph itself.
    pub fn to_emoji(&self) -> Option<&'static str> {
        match self {
            Priority::Highest => Some("\u{1F53A}"), // 🔺
            Priority::High => Some("\u{23EB}"),     // ⏫
            Priority::Medium => Some("\u{1F53C}"),  // 🔼
            Priority::Low => Some("\u{1F53D}"),     // 🔽
            Priority::Lowest => Some("\u{23EC}"),   // ⏬
            Priority::Other(_) => None,
        }
    }

    /// Lenient reverse of [`Priority::to_emoji`] for the DB read path. Accepts
    /// the canonical emoji string for each variant; returns `None` for NULL,
    /// empty, unrecognised, or `Other`-preserved strings. The read path never
    /// reconstructs `Other` — it stays lenient (unknown glyph → `None`).
    pub fn from_emoji_str(s: &str) -> Option<Priority> {
        // A single char is the common case; `s` is exactly one emoji glyph (or
        // empty / NULL on read). Use `chars().next()` to accept the first char
        // and ignore any stray variation selector.
        match s.chars().next() {
            Some(ch) => Priority::from_emoji(ch),
            None => None,
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
    /// Leading whitespace column of the source line (spaces 1:1, tabs expanded to
    /// 4-column tab stops). Captures subtask nesting depth for visual indentation in
    /// the TUI. Zero for top-level tasks. Blockquote markers (`>`) are not counted.
    pub indent: usize,
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
    /// Parsed scheduled date (Tasks-plugin `⏳` U+23F3, ADR-0009). `None` when the
    /// task body carries no valid scheduled date. Independent of `due_date`.
    pub scheduled_date: Option<String>,
    /// Parsed inline tags (`#foo`, with the `#` stripped). Empty when the body
    /// has no tags; deduped, first-seen order preserved. Tier 1 read-only.
    pub tags: Vec<String>,
    /// Parsed priority (single bare priority emoji `🔺`/`⏫`/`🔼`/`🔽`/`⏬`).
    /// `None` when no known priority emoji is present on the line.
    pub priority: Option<Priority>,
    /// Parsed start date (`🛫` + whitespace + `YYYY-MM-DD`). `None` when absent.
    pub start_date: Option<String>,
    /// Parsed created date (`➕` + whitespace + `YYYY-MM-DD`). `None` when absent.
    pub created_date: Option<String>,
    /// Parsed done date (`✅` + whitespace + `YYYY-MM-DD`). `None` when absent.
    pub done_date: Option<String>,
    /// Parsed cancelled date (`❌` + whitespace + `YYYY-MM-DD`). `None` when absent.
    pub cancelled_date: Option<String>,
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

/// True if the (left-trimmed) line opens or closes a fenced code block. Recognises
/// both CommonMark fence forms: backtick (```` ``` ````) and tilde (`~~~`).
fn is_fence(trimmed_line: &str) -> bool {
    trimmed_line.starts_with("```") || trimmed_line.starts_with("~~~")
}

/// True iff the note's YAML frontmatter carries a top-level `taski-skip: true` flag.
///
/// Only the first frontmatter block is honored: the note's very first line must be `---`
/// (trailing whitespace/`\r` trimmed), and only lines up to the first closing `---` are
/// examined. A `---` elsewhere in the body is not frontmatter. If no closing `---` is
/// found, the note is treated as having no frontmatter at all (the flag is absent, even
/// if the key appears below the opener). Only a top-level (column-0) `taski-skip` key is
/// matched (nested/indented keys are ignored). The first such key wins; its value is
/// truthy iff it case-insensitively equals `true`, or equals the quoted variants
/// `"true"` / `'true'`. All other spellings (`false`, `yes`, `on`, empty, …) are NOT
/// truthy. See ADR-0017.
pub fn taski_skip_enabled(markdown: &str) -> bool {
    let mut lines = markdown.lines();
    // Opener must be the very first line.
    let first = match lines.next() {
        Some(l) => l,
        None => return false,
    };
    if first.trim_end() != "---" {
        return false;
    }
    // First-key-wins value, deferred until a closing fence confirms a well-formed
    // block. If the loop ends with no closer, the note has no frontmatter (ADR-0017).
    let mut pending: Option<bool> = None;
    for line in lines {
        if line.trim_end() == "---" {
            return pending.unwrap_or(false);
        }
        if pending.is_none()
            && let Some(raw_value) = frontmatter_value_for_key(line, "taski-skip")
        {
            pending = Some(is_taski_skip_truthy(raw_value));
        }
    }
    false
}

/// If `line` is a top-level (no leading whitespace) YAML mapping line for `key`, return the
/// raw value text after the colon (may be empty). Otherwise `None`.
fn frontmatter_value_for_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    if !line.starts_with(key) {
        return None;
    }
    let rest = line[key.len()..].trim_start();
    let after_colon = rest.strip_prefix(':')?;
    Some(after_colon)
}

/// Truthiness for the `taski-skip` value: literal `true` (case-insensitive) or the quoted
/// `"true"` / `'true'` variants. Anything else (including `yes`/`on`/empty) is not truthy.
fn is_taski_skip_truthy(raw_value: &str) -> bool {
    let v = raw_value.trim();
    v.eq_ignore_ascii_case("true") || v == "\"true\"" || v == "'true'"
}

/// Try to interpret a single line as a task checkbox line.
fn parse_task_line(raw_line: &str, note_path: &str, line_number: usize, now: i64) -> Option<Task> {
    let (checkbox_char, body) = task_captures(raw_line)?;
    let body = body.trim();
    let status = Status::from_checkbox_char(checkbox_char);
    let text_hash = hash_str(body);
    let indent = count_indent(raw_line);

    Some(Task {
        id: 0, // placeholder — the DB assigns the surrogate rowid on INSERT (ADR-0005).
        note_path: note_path.to_string(),
        line_number,
        indent,
        text: body.to_string(),
        text_hash,
        status,
        raw_checkbox_char: checkbox_char.to_string(),
        note_hash: None,
        note_mtime: None,
        due_date: extract_due_date(body),
        scheduled_date: extract_scheduled_date(body),
        tags: extract_tags(body),
        priority: extract_priority(body),
        start_date: extract_start_date(body),
        created_date: extract_created_date(body),
        done_date: extract_done_date(body),
        cancelled_date: extract_cancelled_date(body),
        updated_at: now,
    })
}

/// Variation selector 16 (U+FE0F), optionally present after an emoji to request
/// emoji-style rendering.
const VS16: char = '\u{FE0F}';

/// Locate the first parseable date token in `body` matching one of `emojis`,
/// returning `(byte_span, date_string)` where `byte_span` is the `[start, end)`
/// byte range of the ENTIRE token — `emoji` + optional VS16 + ≥1 ASCII whitespace
/// + strict `YYYY-MM-DD` — and `date_string` is the normalized `YYYY-MM-DD`.
///
/// This is the **single source of truth** for the date-token grammar, shared by
/// the read path ([`extract_emoji_date`] returns the date) and the write path
/// ([`rewrite_scheduled`] rewrites the span). Scans left-to-right; at each emoji
/// in `emojis` it skips an optional VS16, requires ≥1 ASCII whitespace, then
/// attempts a strict `YYYY-MM-DD` (bad format / out-of-range → scan continues).
/// "First valid date wins" — the same semantics the read path has always had.
fn find_emoji_date_span(body: &str, emojis: &[char]) -> Option<(std::ops::Range<usize>, String)> {
    let bytes = body.as_bytes();
    for (emoji_off, ch) in body.char_indices() {
        if !emojis.contains(&ch) {
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
            // `parse_date_at` validated exactly 10 bytes here.
            return Some((emoji_off..pos + 10, date));
        }
    }
    None
}

/// Extract the first date from a task body matching the Obsidian Tasks emoji
/// convention: one of `emojis` + optional VS16 + whitespace + strict
/// `YYYY-MM-DD`. Returns the normalized `"YYYY-MM-DD"` string, or `None` if no
/// valid date is present. Thin wrapper over [`find_emoji_date_span`].
fn extract_emoji_date(body: &str, emojis: &[char]) -> Option<String> {
    find_emoji_date_span(body, emojis).map(|(_, date)| date)
}

/// Extract the due date (`📅`/`📆`/`🗓` + optional VS16 + whitespace +
/// `YYYY-MM-DD`) from a task body, per the Obsidian Tasks emoji convention.
/// Returns the normalized `"YYYY-MM-DD"` string, or `None` if no valid due date
/// is present. See [`find_emoji_date_span`] for the full scan semantics.
pub fn extract_due_date(body: &str) -> Option<String> {
    extract_emoji_date(body, &['📅', '📆', '🗓'])
}

/// Extract the scheduled date (`⏳` U+23F3 + optional VS16 + whitespace +
/// `YYYY-MM-DD`) from a task body, per the Obsidian Tasks emoji convention
/// (ADR-0009). Grammar identical to [`extract_due_date`]; only the leading
/// emoji differs. Public so the write-path oracle proptest can verify
/// `rewrite_scheduled`'s output through the same primitive the parser uses.
pub fn extract_scheduled_date(body: &str) -> Option<String> {
    extract_emoji_date(body, &['⏳'])
}

/// Extract the start date (`🛫` + optional VS16 + whitespace + `YYYY-MM-DD`)
/// from a task body, per the Obsidian Tasks emoji convention. Thin wrapper
/// over [`extract_emoji_date`]; only the leading emoji differs.
pub fn extract_start_date(body: &str) -> Option<String> {
    extract_emoji_date(body, &['🛫'])
}

/// Extract the created date (`➕` + optional VS16 + whitespace + `YYYY-MM-DD`)
/// from a task body, per the Obsidian Tasks emoji convention.
pub fn extract_created_date(body: &str) -> Option<String> {
    extract_emoji_date(body, &[CREATED_EMOJI])
}

/// Extract the done date (`✅` + optional VS16 + whitespace + `YYYY-MM-DD`)
/// from a task body, per the Obsidian Tasks emoji convention.
pub fn extract_done_date(body: &str) -> Option<String> {
    extract_emoji_date(body, &['✅'])
}

/// Extract the cancelled date (`❌` + optional VS16 + whitespace + `YYYY-MM-DD`)
/// from a task body, per the Obsidian Tasks emoji convention.
pub fn extract_cancelled_date(body: &str) -> Option<String> {
    extract_emoji_date(body, &['❌'])
}

/// Extract the priority from a task body — the **first** occurrence (left to
/// right) of any of the five canonical Obsidian Tasks priority emojis. Returns
/// `None` when no priority emoji is present. **Does not emit** [`Priority::Other`]
/// — there is no reliable way to know an unknown bare glyph was meant as a
/// priority marker, so this stays a closed set (matches `find_emoji_date_span`'s
/// "first match wins" precedent).
///
/// Walks via `char_indices()` (UTF-8-safe; all priority emojis are multi-byte).
/// A trailing VS16 after a non-matching emoji is irrelevant because the scan
/// never advances past a matched emoji (first match returns immediately).
pub fn extract_priority(body: &str) -> Option<Priority> {
    for (_, ch) in body.char_indices() {
        if let Some(p) = Priority::from_emoji(ch) {
            return Some(p);
        }
    }
    None
}

/// Extract inline tags (`#foo`) from a task body. Always returns a `Vec` (empty
/// when none). Tags are returned **without** the `#` prefix, deduped preserving
/// first-seen order.
///
/// Grammar (matches Obsidian core's tag-recognition rules, intentionally
/// stricter than the Tasks plugin's regex):
/// - A tag starts at a `#` that is either at the body start **or** immediately
///   preceded by ASCII whitespace (space / tab). (`foo#bar` and URL fragments
///   like `https://x.com/y#section` are rejected.)
/// - The char immediately after `#` must be an ASCII letter (`a-z`/`A-Z`) or
///   `_`. (Rejects `#123`, `#!`, `#-`.)
/// - The run continues with ASCII letters/digits/`_`/`-`/`/` until any other
///   char or whitespace. (Allows nested `#project/sub`, hyphenated `#foo-bar`.)
///
/// Walks via `char_indices()` (UTF-8-safe).
pub fn extract_tags(body: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut i = 0;
    while i < bytes.len() {
        // Tag boundary: a `#` at body start or immediately preceded by ASCII ws.
        let is_boundary = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if bytes[i] != b'#' || !is_boundary {
            i += 1;
            continue;
        }
        // Skip the `#`.
        i += 1;

        // The char after `#` must be ASCII letter or `_`.
        if i >= bytes.len() || !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            continue;
        }

        // Walk the run in `char_indices()` to find the tag's UTF-8-safe end.
        // Start scanning from the current byte offset; collect bytes for the tag.
        let tag_start_byte = i;
        let mut tag_end_byte = i;
        for (off, ch) in body[i..].char_indices() {
            let abs = i + off;
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '/' {
                tag_end_byte = abs + ch.len_utf8();
            } else {
                break;
            }
        }
        let tag = &body[tag_start_byte..tag_end_byte];
        let tag_string = tag.to_string();
        if seen.insert(tag_string.clone()) {
            out.push(tag_string);
        }
        // Advance `i` past the consumed tag.
        i = tag_end_byte;
    }

    out
}

/// Outcome of [`rewrite_scheduled`]: the pure line-rewrite behind the ADR-0009
/// Phase 2 "mark for today" write gesture. `Unchanged` and `Unparseable` carry
/// no data; `Rewritten` carries the full rewritten line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteResult {
    /// The line already has the desired scheduled state — the caller must not
    /// write (idempotent). Reached when `desired` equals the existing date, or
    /// when `desired = None` and there is no `⏳` to remove.
    Unchanged,
    /// The rewritten line (caller splices it into the note and writes).
    Rewritten(String),
    /// The existing `⏳` is malformed (bad date, NBSP, stray variation selector,
    /// more than one `⏳`), or `desired` is not a strict `YYYY-MM-DD`. The
    /// caller must refuse rather than guess.
    Unparseable,
}

/// The hourglass emoji `⏳` (U+23F3) — the Obsidian Tasks "scheduled" marker.
const SCHEDULED_EMOJI: char = '⏳';

/// The check-mark button emoji `✅` (U+2705) — the Obsidian Tasks "done" marker.
const DONE_EMOJI: char = '✅';

/// The cross-mark emoji `❌` (U+274C) — the Obsidian Tasks "cancelled" marker.
/// ADR-0013: stamped on a `- [-]` cancel flip, the `❌` sibling of the ADR-0012
/// `✅` done-date stamp. Read by the pre-existing [`extract_cancelled_date`]
/// (Tier 1, schema v6); this const backs the write-path oracle.
const CANCELLED_EMOJI: char = '❌';

/// The heavy plus sign emoji `➕` (U+2795) — the Obsidian Tasks "created" marker.
/// ADR-0014: stamped on a quick-add'd inbox task (`- [ ] <text> ➕ <today>`),
/// the first written token in a *creation* context. Read by the pre-existing
/// [`extract_created_date`] (Tier 1, schema v6); this const backs the
/// write-path construction oracle [`inbox_line_for`].
const CREATED_EMOJI: char = '➕';

/// Pure line-rewrite for the ADR-0009 Phase 2 "mark/unmark for today" gesture.
/// Given a full task line (including the `- [ ] ` prefix, WITHOUT its trailing
/// `\n` — the caller handles line terminators) and a desired scheduled state:
///
/// - `desired = Some("YYYY-MM-DD")` (mark / re-schedule): if a parseable `⏳`
///   token already exists, **replace** its date bytes with `desired` (keeping
///   the `⏳`, its VS16, and its spacing); otherwise **append** ` ⏳ YYYY-MM-DD`
///   at the end of the line.
/// - `desired = None` (unmark): if a parseable `⏳` token exists, **remove** it
///   together with its single preceding ASCII space; otherwise return
///   [`RewriteResult::Unchanged`].
///
/// **Never guesses.** If the line carries a `⏳` that does not form a valid
/// token (malformed date, NBSP, stray variation selectors), or carries more
/// than one `⏳`, the result is [`RewriteResult::Unparseable`] — the caller
/// refuses. `desired` is defensively validated; a malformed date also yields
/// `Unparseable`. Idempotent: an already-matching date returns `Unchanged`.
///
/// The checkbox marker (`- [ ]`/`- [x]`) and every other byte (text, tags,
/// other emojis like 📅/🛫) outside the `⏳` token's span is preserved
/// byte-for-byte. Pure (no I/O) so it is exhaustively proptested in isolation.
///
/// Thin wrapper over the shared [`rewrite_emoji_date`] core (ADR-0012
/// generalized the body so `✅` done-dates reuse the identical grammar + splice).
pub fn rewrite_scheduled(line: &str, desired: Option<&str>) -> RewriteResult {
    rewrite_emoji_date(line, desired, SCHEDULED_EMOJI)
}

/// Pure line-rewrite for the ADR-0012 done-date stamp (the `✅` stamp composed
/// into a checkbox flip). Given a full task line (WITHOUT its trailing `\n` —
/// the caller handles line terminators) and a desired done-date state:
///
/// - `desired = Some("YYYY-MM-DD")` (stamp on completion): if a parseable `✅`
///   token already exists, **replace** its date bytes with `desired` (canonical
///   re-done behaviour); otherwise **append** ` ✅ YYYY-MM-DD` at the end of the
///   line.
/// - `desired = None` (clear on un-completion): if a parseable `✅` token
///   exists, **remove** it and its single preceding ASCII space; otherwise
///   return [`RewriteResult::Unchanged`].
///
/// Identical grammar + splice to [`rewrite_scheduled`]; only the leading emoji
/// differs (`✅` U+2705 instead of `⏳` U+23F3). See that function's docs for the
/// full "never guesses" contract — every guarantee (idempotency, malformed
/// refusal, byte-for-byte preservation of unrelated content) carries over.
pub fn rewrite_done_date(line: &str, desired: Option<&str>) -> RewriteResult {
    rewrite_emoji_date(line, desired, DONE_EMOJI)
}

/// Pure line-rewrite for the ADR-0013 cancelled-date stamp (the `❌` stamp
/// composed into a cancel checkbox flip, `- [ ]` → `- [-]`). The direct sibling
/// of [`rewrite_done_date`] (ADR-0012) on the `❌` axis. Given a full task line
/// (WITHOUT its trailing `\n` — the caller handles line terminators) and a
/// desired cancelled-date state:
///
/// - `desired = Some("YYYY-MM-DD")` (stamp on cancel): if a parseable `❌`
///   token already exists, **replace** its date bytes with `desired` (canonical
///   re-cancel behaviour); otherwise **append** ` ❌ YYYY-MM-DD` at the end of
///   the line.
/// - `desired = None` (clear on un-cancel): if a parseable `❌` token exists,
///   **remove** it and its single preceding ASCII space; otherwise return
///   [`RewriteResult::Unchanged`].
///
/// Identical grammar + splice to [`rewrite_done_date`] / [`rewrite_scheduled`];
/// only the leading emoji differs (`❌` U+274C). See [`rewrite_done_date`]'s docs
/// for the full "never guesses" contract — every guarantee carries over.
pub fn rewrite_cancelled_date(line: &str, desired: Option<&str>) -> RewriteResult {
    rewrite_emoji_date(line, desired, CANCELLED_EMOJI)
}

/// ADR-0014: the pure construction oracle for quick-add. Given the user-typed
/// task `text` and a `YYYY-MM-DD` `today` string, construct a canonical
/// Obsidian-Tasks inbox line:
///
/// ```text
/// - [ ] <text> ➕ <today>
/// ```
///
/// This is a **construction** oracle (simpler than the rewrite oracles above — it
/// builds a line, not edits one). Strips embedded newlines from `text` (the
/// quick-add modal is single-line only). Empty `text` produces
/// `"- [ ]  ➕ <today>"` (valid syntax — two spaces between `]` and `➕` — and the
/// user can fill in the body in Obsidian). Text containing emoji dates (`✅`/`❌`/
/// `📅`/`⏳`) is preserved verbatim — the scanner will parse them as metadata; a
/// known edge case the user can fix in Obsidian.
///
/// Pure (no I/O) so it is exhaustively proptested in isolation. The daemon's
/// `process_quick_add` calls this and the proptest cross-checks against it.
pub fn inbox_line_for(text: &str, today: &str) -> String {
    let clean: String = text.chars().filter(|&c| c != '\n' && c != '\r').collect();
    format!("- [ ] {clean} {CREATED_EMOJI} {today}")
}

/// The leading emoji of every Obsidian Tasks metadata token Taski recognizes —
/// the four written date stamps, the read-path date emojis, the recurrence
/// marker, and the five priority glyphs. Used by [`insert_notes_link`] as the
/// boundary: description text (the notes link) must land **before** any trailing
/// metadata, since the Tasks plugin parses metadata from the end of the line.
const METADATA_EMOJIS: &[char] = &[
    SCHEDULED_EMOJI, // ⏳
    DONE_EMOJI,      // ✅
    CANCELLED_EMOJI, // ❌
    CREATED_EMOJI,   // ➕
    '📅',
    '📆',
    '🗓',
    '🛫',
    '🔁',
    '🔺',
    '⏫',
    '🔼',
    '🔽',
    '⏬',
];

/// Byte offset within `body` of the first Tasks metadata emoji (see
/// [`METADATA_EMOJIS`]), or `None` if the body carries no metadata. The link is
/// inserted *before* this offset so it stays in the description.
fn first_metadata_offset(body: &str) -> Option<usize> {
    body.char_indices()
        .find(|&(_, c)| METADATA_EMOJIS.contains(&c))
        .map(|(i, _)| i)
}

/// ADR-0019: the pure line-rewrite that inserts a single aliased in-page wikilink
/// `[[#notes-<id>|Notes]]` into a task line's description, immediately before the
/// first Tasks-plugin metadata token (or at end-of-line if the line carries no
/// metadata). The link is description text, so it must precede `⏳`/`📅`/`✅`/…
/// to keep the plugin's right-to-left metadata parse intact.
///
/// **Idempotent:** if the line already contains a `[[#notes-<id>|` link for this
/// `id`, the line is returned unchanged (the replay/double-press guard). If the
/// line does not parse as a checkbox task (defensive — the daemon already
/// verified it), it is also returned unchanged.
///
/// Every byte outside the inserted span is preserved verbatim; exactly one ASCII
/// space is guaranteed on each side of the inserted link. Pure (no I/O) so it is
/// proptested in isolation.
pub fn insert_notes_link(line: &str, id: &str) -> String {
    // Idempotent: never insert a second link for this id.
    if line.contains(&format!("[[#notes-{id}|")) {
        return line.to_string();
    }
    let link = format!("[[#notes-{id}|Notes]]");
    let Some((_, body)) = task_captures(line) else {
        // Not a checkbox task line — defensive no-op (daemon verifies upstream).
        return line.to_string();
    };
    // `body` is a suffix slice of `line`, so its start offset is exact.
    let body_start = line.len() - body.len();
    match first_metadata_offset(body) {
        Some(off) => {
            let at = body_start + off;
            let left = &line[..at];
            // Valid lines already have whitespace before the metadata emoji; the
            // separator guard covers the rare malformed line with none.
            let sep = if left.ends_with(|c: char| c.is_whitespace()) {
                ""
            } else {
                " "
            };
            format!("{left}{sep}{link} {}", &line[at..])
        }
        // No metadata: append at end of line, after trimming trailing whitespace.
        None => format!("{} {link}", line.trim_end()),
    }
}

/// ADR-0019: extract the `<id>` from a task line's `[[#notes-<id>|…]]` link, or
/// `None` if the line carries no such link. The daemon uses this to decide
/// first-note (insert link + create heading) vs append-note (text under the
/// existing `### notes-<id>` heading), and to locate that heading. The `<id>` is
/// the run of bytes between `[[#notes-` and the first `|` (aliased) or `]`.
pub fn notes_link_id(line: &str) -> Option<String> {
    let start = line.find("[[#notes-")? + "[[#notes-".len();
    let rest = &line[start..];
    let end = rest.find(['|', ']'])?;
    let id = &rest[..end];
    (!id.is_empty()).then(|| id.to_string())
}

/// ADR-0019: the pure construction oracle for a task-note bullet. Given the
/// user-typed `text`, produce a single Markdown list item `- <text>` to append
/// under a `### notes-<id>` heading. Strips embedded newlines (the note modal is
/// single-line) and trims surrounding whitespace.
///
/// **Phantom-task guard:** if `- <text>` would itself parse as a checkbox task
/// (e.g. `text == "[ ] buy milk"` → `- [ ] buy milk`), the first `[` is escaped
/// (`- \[ ] buy milk`) so the indexer can never mistake a note for a task. A
/// backslash before the `[` defeats [`task_captures`] (which requires a literal
/// `[` opener) while Obsidian renders the bracket literally. Pure (no I/O).
pub fn note_bullet_for(text: &str) -> String {
    let clean: String = text.chars().filter(|&c| c != '\n' && c != '\r').collect();
    let mut bullet = format!("- {}", clean.trim());
    if task_captures(&bullet).is_some()
        && let Some(pos) = bullet.find('[')
    {
        bullet.insert(pos, '\\');
    }
    bullet
}

/// Shared core behind [`rewrite_scheduled`] (ADR-0009) and [`rewrite_done_date`]
/// (ADR-0012). The two emojis share the identical insertion grammar (emoji +
/// optional VS16 + whitespace + strict `YYYY-MM-DD`, scanned right-to-left by
/// the Tasks parser) and the identical append / replace / remove splice, so this
/// private helper takes the leading `emoji` as a parameter. Pure (no I/O) so it
/// is exhaustively proptested in isolation via both wrappers.
fn rewrite_emoji_date(line: &str, desired: Option<&str>, emoji: char) -> RewriteResult {
    // Defensive: the TUI passes `ymd_from_unix(now)`, but the write path never
    // trusts the caller. A malformed `desired` is refused, never written.
    if let Some(d) = desired
        && parse_date_at(d.as_bytes(), 0).as_deref() != Some(d)
    {
        return RewriteResult::Unparseable;
    }

    let bytes = line.as_bytes();
    // How many raw `emoji` chars are on the line. Zero/one is workable; two or
    // more is ambiguous (which one to edit?) → refuse rather than guess.
    let emoji_count = line.chars().filter(|&c| c == emoji).count();
    // The first parseable `emoji` token, if any (parser-consistent via the shared
    // grammar helper). When exactly one `emoji` is present, this is Some iff that
    // `emoji` forms a clean token; None means the lone `emoji` is malformed.
    let token = find_emoji_date_span(line, &[emoji]);

    match desired {
        None => match (emoji_count, token) {
            (0, _) => RewriteResult::Unchanged,
            (1, Some((span, _))) => {
                // Remove the token and its single preceding ASCII space.
                let remove_start = if span.start > 0 && bytes[span.start - 1] == b' ' {
                    span.start - 1
                } else {
                    span.start
                };
                let mut out = String::with_capacity(line.len() - (span.end - remove_start));
                out.push_str(&line[..remove_start]);
                out.push_str(&line[span.end..]);
                RewriteResult::Rewritten(out)
            }
            // 1 malformed `emoji`, or ≥2 `emoji` → never guess.
            _ => RewriteResult::Unparseable,
        },
        Some(date) => match (emoji_count, token) {
            (0, None) => {
                // No `emoji` at all → append ` <emoji> YYYY-MM-DD` at the logical line end.
                let mut out = String::with_capacity(line.len() + 12);
                out.push_str(line);
                out.push(' ');
                out.push(emoji);
                out.push(' ');
                out.push_str(date);
                RewriteResult::Rewritten(out)
            }
            (1, Some((span, _))) => {
                // Exactly one clean token: idempotent if the date already matches,
                // else replace ONLY its date bytes (keep `<emoji>` + VS16 + whitespace).
                let existing_date = &line[span.end - 10..span.end];
                if existing_date == date {
                    return RewriteResult::Unchanged;
                }
                let mut out = String::with_capacity(line.len() + 10);
                out.push_str(&line[..span.end - 10]);
                out.push_str(date);
                out.push_str(&line[span.end..]);
                RewriteResult::Rewritten(out)
            }
            // An `emoji` is present but malformed (lone bad token), or ≥2 `emoji` → refuse.
            _ => RewriteResult::Unparseable,
        },
    }
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

/// Convert a Unix timestamp (seconds since 1970-01-01 UTC) to a `YYYY-MM-DD`
/// calendar date string.
///
/// Pure (no I/O) so it is unit-testable, and used by the TUI to derive "today"
/// from the wall clock without pulling in a date crate. Uses Howard Hinnant's
/// `civil_from_days` algorithm, which handles the full proleptic Gregorian
/// calendar (including leap years and dates before the epoch). Negative
/// timestamps (pre-1970) are handled via floor division of days.
pub fn ymd_from_unix(secs: i64) -> String {
    // 86_400 seconds per day. `div_euclid` floors toward negative infinity, so
    // a pre-epoch timestamp maps to the correct prior calendar day.
    let days = secs.div_euclid(86_400);
    civil_from_days(days)
}

/// Howard Hinnant's `civil_from_days`: convert a count of days since
/// 1970-01-01 into a `(year, month, day)` Gregorian calendar date. Returns the
/// formatted `YYYY-MM-DD`. All arithmetic is in `i64` to avoid unsigned
/// underflow in the month/dance steps.
fn civil_from_days(z_in: i64) -> String {
    let z = z_in + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = y + if m <= 2 { 1 } else { 0 };
    format!("{year:04}-{m:02}-{d:02}")
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

/// Count the leading whitespace of a line as a visual column offset. Spaces count
/// 1:1; a tab advances to the next multiple of 4 (the common Obsidian tab width).
/// Stops at the first non-whitespace character. Returns 0 for lines with no leading
/// whitespace (including blockquote-prefixed lines like `> - [ ] task`, where the
/// `>` is not nesting indentation).
fn count_indent(line: &str) -> usize {
    let mut col = 0usize;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col = (col / 4 + 1) * 4,
            _ => break,
        }
    }
    col
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

/// Toggle a single task line between checkbox format and plain bullet format
/// (ADR-0011). The inverse of itself: calling twice on any valid input returns
/// the original line.
///
/// - `- [ ] text` or `- [x] text` → `- text` (strip checkbox, keep body)
/// - `- text` → `- [ ] text` (add open checkbox)
/// - Any other format (no bullet char, malformed, etc.) → `Unparseable`
///
/// Pure (no I/O), symmetric (self-inverse on all valid inputs), and never
/// panics. The caller (the daemon) handles the actual vault write via
/// [`atomic_write`]; this is only the line-rewrite oracle.
///
/// Leading whitespace and blockquote markers (`>`) are preserved exactly.
pub fn toggle_bullet(line: &str) -> RewriteResult {
    let bytes = line.as_bytes();
    let mut i = 0;

    advance_past_leading_markers(bytes, &mut i);

    // Must have a bullet char.
    if i >= bytes.len() || !matches!(bytes[i], b'-' | b'*' | b'+') {
        return RewriteResult::Unparseable;
    }
    i += 1;

    // Must have at least one whitespace after the bullet.
    let ws_start = i;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i == ws_start || i >= bytes.len() {
        return RewriteResult::Unparseable;
    }

    if bytes[i] == b'[' {
        // ── Checkbox → bullet ────────────────────────────────────────────
        // Find the closing `]`.
        let mut close = i + 1;
        while close < bytes.len() && bytes[close] != b']' {
            close += 1;
        }
        if close >= bytes.len() {
            return RewriteResult::Unparseable;
        }
        // Find the body start after `]`.
        let mut body = close + 1;
        while body < bytes.len() && bytes[body].is_ascii_whitespace() {
            body += 1;
        }
        // Reconstruct: everything up to (but not including) the checkbox `[`,
        // then the body. This strips `[x] ` (or whatever the checkbox was).
        let mut out = String::with_capacity(line.len() - (body - i));
        out.push_str(&line[..i]);
        out.push_str(&line[body..]);
        RewriteResult::Rewritten(out)
    } else {
        // ── Bullet → checkbox ────────────────────────────────────────────
        // Insert `[ ] ` between the bullet's trailing whitespace and the body.
        let mut out = String::with_capacity(line.len() + 4);
        out.push_str(&line[..i]);
        out.push_str("[ ] ");
        out.push_str(&line[i..]);
        RewriteResult::Rewritten(out)
    }
}

/// ADR-0020: permute the *contents* of the lines named in `desired_order`
/// (1-based line numbers) among those same lines' positions, leaving every other
/// line and every line terminator byte-identical.
///
/// `desired_order` is the listed lines in their new top-to-bottom order. The i-th
/// smallest of those line numbers (the target positions, ascending) receives the
/// content of `desired_order[i]`. "Content" is the text of a line *excluding* its
/// terminator (`\n` / `\r\n` / none-at-EOF); **terminators stay with positions**,
/// so a `\r\n`-terminated position stays `\r\n` regardless of which content moves
/// in (the CRLF-preservation discipline the single-line rewrites also follow).
///
/// Returns the input unchanged (an idempotent no-op) when `desired_order` is empty,
/// contains an out-of-range or duplicate line number, or is already in ascending
/// order. This is the pure reorder oracle the daemon's `process_reorder` applies;
/// it is a permutation, so it never invents or drops a line.
pub fn permute_lines(content: &str, desired_order: &[usize]) -> String {
    let segments = split_lines_with_terminators(content);
    let n = segments.len();

    if desired_order.is_empty() {
        return content.to_string();
    }
    // Validate: every line number in range and unique. Bail to a no-op otherwise —
    // the daemon validates separately and refuses; this keeps the oracle total.
    let mut seen = vec![false; n];
    for &ln in desired_order {
        if ln == 0 || ln > n || seen[ln - 1] {
            return content.to_string();
        }
        seen[ln - 1] = true;
    }

    // Target positions = the listed lines, ascending. Place the content of
    // `desired_order[i]` into the i-th smallest target position.
    let mut targets: Vec<usize> = desired_order.iter().map(|&ln| ln - 1).collect();
    targets.sort_unstable();

    let mut new_contents: Vec<&str> = segments.iter().map(|&(c, _)| c).collect();
    for (i, &tgt) in targets.iter().enumerate() {
        new_contents[tgt] = segments[desired_order[i] - 1].0;
    }

    let mut out = String::with_capacity(content.len());
    for (i, &(_, term)) in segments.iter().enumerate() {
        out.push_str(new_contents[i]);
        out.push_str(term);
    }
    out
}

/// Return the content of the lines named in `line_numbers` (1-based), in the order
/// the numbers are given, each **without** its terminator — the block to append to
/// the archive (ADR-0021 Phase A). Out-of-range numbers (zero or beyond the line
/// count) are skipped; the caller passes a validated subset. Pure: no I/O, the
/// structural read-half of the archive move.
pub fn extract_lines(content: &str, line_numbers: &[usize]) -> Vec<String> {
    let segments = split_lines_with_terminators(content);
    let n = segments.len();
    line_numbers
        .iter()
        .filter(|&&ln| ln >= 1 && ln <= n)
        .map(|&ln| segments[ln - 1].0.to_string())
        .collect()
}

/// Remove the lines named in `line_numbers` (1-based) from `content`, leaving every
/// surviving line and its terminator (`\n` / `\r\n` / none-at-EOF) byte-identical and
/// in order — the structural **deletion** analogue of [`permute_lines`] (ADR-0021
/// Phase B). Out-of-range and duplicate numbers are ignored. Pure: no I/O, so it
/// never invents or reorders a surviving line.
pub fn remove_lines(content: &str, line_numbers: &[usize]) -> String {
    let segments = split_lines_with_terminators(content);
    let n = segments.len();
    let mut drop = vec![false; n];
    for &ln in line_numbers {
        if ln >= 1 && ln <= n {
            drop[ln - 1] = true;
        }
    }
    let mut out = String::with_capacity(content.len());
    for (i, &(c, term)) in segments.iter().enumerate() {
        if !drop[i] {
            out.push_str(c);
            out.push_str(term);
        }
    }
    out
}

/// Split `content` into `(line_content, terminator)` pairs, one per line, where the
/// terminator is `"\r\n"`, `"\n"`, or `""` (the final line when the file has no
/// trailing newline). The 0-based index of each pair equals its 1-based line number
/// minus one, matching the line numbering [`parse_tasks`] derives via `str::lines`.
fn split_lines_with_terminators(content: &str) -> Vec<(&str, &str)> {
    let mut segments = Vec::new();
    for piece in content.split_inclusive('\n') {
        if let Some(body) = piece.strip_suffix("\r\n") {
            segments.push((body, "\r\n"));
        } else if let Some(body) = piece.strip_suffix('\n') {
            segments.push((body, "\n"));
        } else {
            segments.push((piece, ""));
        }
    }
    segments
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

    /// Tilde fences (`~~~`) are valid CommonMark and appear in some Obsidian notes.
    /// Tasks inside a tilde-fenced block must NOT be parsed — mirrors the backtick
    /// fence behaviour exercised above.
    #[test]
    fn skips_tasks_inside_tilde_fences() {
        let md = "\
- [ ] real task before the fence

~~~text
- [ ] fake task inside a tilde fence
- [x] also fake
~~~

- [x] real task after the fence
";

        let tasks = parse_tasks(md, "tilde.md");
        assert_eq!(tasks.len(), 2, "should skip the tilde-fenced tasks");
        assert_eq!(tasks[0].status, Status::Open);
        assert_eq!(tasks[0].text, "real task before the fence");
        assert_eq!(tasks[1].status, Status::Done);
        assert_eq!(tasks[1].text, "real task after the fence");
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
        // Indent is captured: star/plus at column 0, the indented one at column 2.
        assert_eq!(tasks[0].indent, 0); // `* [ ]  star bullet works`
        assert_eq!(tasks[1].indent, 0); // `+ [ ] plus bullet works`
        assert_eq!(tasks[2].indent, 2); // `  - [ ] indented works`
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

    // --- extract_scheduled_date unit tests (ADR-0009 Phase 1) ---------------

    #[test]
    fn extract_scheduled_date_plain() {
        assert_eq!(
            extract_scheduled_date("buy shampoo ⏳ 2026-06-20"),
            Some("2026-06-20".to_string())
        );
    }

    #[test]
    fn extract_scheduled_date_none_when_no_emoji() {
        assert_eq!(extract_scheduled_date("no date here"), None);
    }

    #[test]
    fn extract_scheduled_date_tolerates_variation_selector() {
        // VS16 (U+FE0F) immediately after ⏳ is optional but accepted, matching
        // the due-date parser's behaviour for 📅.
        assert_eq!(
            extract_scheduled_date("task \u{23F3}\u{FE0F} 2026-06-20"),
            Some("2026-06-20".to_string())
        );
    }

    #[test]
    fn extract_scheduled_date_allows_multiple_spaces() {
        assert_eq!(
            extract_scheduled_date("⏳   2026-06-20"),
            Some("2026-06-20".to_string())
        );
    }

    #[test]
    fn extract_scheduled_date_none_when_bad_format() {
        assert_eq!(extract_scheduled_date("⏳ 20260620"), None);
        assert_eq!(extract_scheduled_date("⏳ 26-06-20"), None);
        assert_eq!(extract_scheduled_date("⏳ 2026/06/20"), None);
    }

    /// `⏳` with no whitespace before the date is rejected — exactly as `📅` is
    /// (the two parsers share `extract_emoji_date`, so behaviour is consistent).
    #[test]
    fn extract_scheduled_date_no_space_is_rejected_like_due() {
        assert_eq!(extract_scheduled_date("⏳2026-06-20"), None);
        assert_eq!(extract_due_date("📅2026-06-20"), None);
    }

    /// A body with both a due date and a scheduled date parses each
    /// independently (the two date axes are orthogonal — ADR-0009).
    #[test]
    fn due_and_scheduled_parse_independently() {
        let body = "📅 2026-06-20 ⏳ 2026-06-21";
        assert_eq!(extract_due_date(body).as_deref(), Some("2026-06-20"));
        assert_eq!(extract_scheduled_date(body).as_deref(), Some("2026-06-21"));
    }

    #[test]
    fn parse_task_line_wires_scheduled_date_into_task() {
        let tasks = parse_tasks("- [ ] buy shampoo ⏳ 2026-06-20\n", "d.md");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].scheduled_date.as_deref(), Some("2026-06-20"));
        assert!(tasks[0].due_date.is_none());
    }

    // --- Tier 1: extract_start_date / created / done / cancelled ------------

    #[test]
    fn extract_start_date_plain_departure_emoji() {
        assert_eq!(
            extract_start_date("ship 🛫 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_start_date_none_when_no_emoji() {
        assert_eq!(extract_start_date("plain task"), None);
    }

    #[test]
    fn extract_start_date_tolerates_vs16() {
        // VS16 (U+FE0F) immediately after 🛫 is optional but accepted — this
        // confirms the shared `extract_emoji_date` helper handles VS16 for the
        // start-date emoji just as it does for 📅 and ⏳.
        assert_eq!(
            extract_start_date("\u{1F6EB}\u{FE0F} 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_start_date_no_space_rejected_like_due() {
        // Taski requires ≥1 whitespace between the emoji and the date; the
        // Obsidian Tasks plugin's zero-space tolerance is a pre-existing interop
        // gap NOT in scope for Tier 1. Pinning consistency with the existing
        // date extractors here.
        assert_eq!(extract_start_date("\u{1F6EB}2025-12-31"), None);
    }

    #[test]
    fn extract_created_date_plain_plus_emoji() {
        assert_eq!(
            extract_created_date("ship ➕ 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_created_date_none_when_no_emoji() {
        assert_eq!(extract_created_date("plain task"), None);
    }

    #[test]
    fn extract_created_date_tolerates_vs16() {
        assert_eq!(
            extract_created_date("\u{2795}\u{FE0F} 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_created_date_no_space_rejected_like_due() {
        assert_eq!(extract_created_date("\u{2795}2025-12-31"), None);
    }

    #[test]
    fn extract_done_date_plain_check_emoji() {
        assert_eq!(
            extract_done_date("ship ✅ 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_done_date_none_when_no_emoji() {
        assert_eq!(extract_done_date("plain task"), None);
    }

    #[test]
    fn extract_done_date_tolerates_vs16() {
        assert_eq!(
            extract_done_date("\u{2705}\u{FE0F} 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_done_date_no_space_rejected_like_due() {
        assert_eq!(extract_done_date("\u{2705}2025-12-31"), None);
    }

    #[test]
    fn extract_cancelled_date_plain_cross_emoji() {
        assert_eq!(
            extract_cancelled_date("ship ❌ 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_cancelled_date_none_when_no_emoji() {
        assert_eq!(extract_cancelled_date("plain task"), None);
    }

    #[test]
    fn extract_cancelled_date_tolerates_vs16() {
        assert_eq!(
            extract_cancelled_date("\u{274C}\u{FE0F} 2025-12-31"),
            Some("2025-12-31".to_string())
        );
    }

    #[test]
    fn extract_cancelled_date_no_space_rejected_like_due() {
        assert_eq!(extract_cancelled_date("\u{274C}2025-12-31"), None);
    }

    // --- Tier 1: extract_priority ------------------------------------------

    #[test]
    fn extract_priority_maps_each_known_emoji() {
        // Source-verified mapping against Obsidian Tasks `Priority.ts` +
        // `DefaultTaskSerializer.ts`. ⚠️ `⏫` is High, NOT Highest; `🔺` is
        // Highest — a common mix-up.
        assert_eq!(extract_priority("task \u{1F53A}"), Some(Priority::Highest));
        assert_eq!(extract_priority("task \u{23EB}"), Some(Priority::High));
        assert_eq!(extract_priority("task \u{1F53C}"), Some(Priority::Medium));
        assert_eq!(extract_priority("task \u{1F53D}"), Some(Priority::Low));
        assert_eq!(extract_priority("task \u{23EC}"), Some(Priority::Lowest));
    }

    #[test]
    fn extract_priority_none_when_no_emoji() {
        assert_eq!(extract_priority("plain task"), None);
    }

    #[test]
    fn extract_priority_takes_first_when_multiple() {
        // note: first-wins matches find_emoji_date_span precedent; a line with
        // two priority emojis is malformed regardless.
        assert_eq!(
            extract_priority("\u{1F53C} \u{1F53A}"),
            Some(Priority::Medium)
        );
    }

    #[test]
    fn extract_priority_tolerates_vs16() {
        // First-match-returns immediately, so a trailing VS16 after the match
        // is irrelevant; this pins that behaviour. (A VS16 before the next
        // non-matching char would also be skipped by the char-by-char scan.)
        assert_eq!(
            extract_priority("task \u{1F53C}\u{FE0F}"),
            Some(Priority::Medium)
        );
    }

    #[test]
    fn priority_round_trips_through_emoji_helpers() {
        // Each known variant maps to exactly one canonical emoji char and back.
        for (emoji, variant) in [
            ('\u{1F53A}', Priority::Highest),
            ('\u{23EB}', Priority::High),
            ('\u{1F53C}', Priority::Medium),
            ('\u{1F53D}', Priority::Low),
            ('\u{23EC}', Priority::Lowest),
        ] {
            assert_eq!(variant.to_emoji(), Some(emoji.to_string().as_str()));
            assert_eq!(Priority::from_emoji(emoji), Some(variant.clone()));
            assert_eq!(Priority::from_emoji_str(&emoji.to_string()), Some(variant));
        }
        // `Other` has no canonical glyph.
        assert_eq!(Priority::Other("z".to_string()).to_emoji(), None);
        // Unknown char → None on both paths.
        assert_eq!(Priority::from_emoji('z'), None);
        assert_eq!(Priority::from_emoji_str("z"), None);
        assert_eq!(Priority::from_emoji_str(""), None);
    }

    // --- Tier 1: extract_tags -----------------------------------------------

    #[test]
    fn extract_tags_single() {
        assert_eq!(extract_tags("body #foo"), vec!["foo".to_string()]);
    }

    #[test]
    fn extract_tags_multiple() {
        assert_eq!(
            extract_tags("#foo #bar"),
            vec!["foo".to_string(), "bar".to_string()]
        );
    }

    #[test]
    fn extract_tags_strips_hash_prefix() {
        let tags = extract_tags("#foo #bar #baz");
        assert!(
            tags.iter().all(|t| !t.starts_with('#')),
            "no tag should retain its '#' prefix: {tags:?}"
        );
    }

    #[test]
    fn extract_tags_nested_slash() {
        assert_eq!(
            extract_tags("#project/sub"),
            vec!["project/sub".to_string()]
        );
    }

    #[test]
    fn extract_tags_allows_hyphen_and_underscore() {
        assert_eq!(
            extract_tags("#foo-bar_baz"),
            vec!["foo-bar_baz".to_string()]
        );
    }

    #[test]
    fn extract_tags_empty_when_no_tag() {
        assert!(extract_tags("plain task").is_empty());
    }

    #[test]
    fn extract_tags_ignores_hash_followed_by_digit() {
        // Obsidian core rule: a tag must start with a letter or `_`.
        assert!(extract_tags("see #123").is_empty());
    }

    #[test]
    fn extract_tags_ignores_hash_inside_word() {
        assert!(extract_tags("foo#bar").is_empty());
    }

    #[test]
    fn extract_tags_ignores_hash_in_url_fragment() {
        assert!(extract_tags("see https://x.com/y#section").is_empty());
    }

    #[test]
    fn extract_tags_dedups_preserving_order() {
        assert_eq!(
            extract_tags("#foo #bar #foo"),
            vec!["foo".to_string(), "bar".to_string()]
        );
    }

    #[test]
    fn extract_tags_tag_at_body_start() {
        // Start-of-string is a valid tag boundary (equivalent to a leading ws).
        assert_eq!(extract_tags("#foo bar"), vec!["foo".to_string()]);
    }

    /// Documents a deliberate gap, not a bug.
    #[test]
    fn extract_tags_inside_inline_code_is_known_limitation() {
        // TODO(tier-2): inline code spans are not handled (tech.md L35 —
        // deferred until pulldown-cmark adoption); documenting so this is
        // recognized as a known gap, not a future regression.
        //
        // The boundary rule ("`#` at body start or preceded by ASCII ws")
        // happens to reject the backtick-adjacent form `` `#ref` `` (the
        // backtick is not whitespace), but it does NOT reject the
        // space-padded form `` ` #ref` `` — the space inside the code span
        // counts as a tag boundary, so `ref` is extracted. Real inline-code
        // awareness would skip both.
        assert!(
            extract_tags("`#ref`").is_empty(),
            "backtick-adjacent form rejected"
        );
        assert_eq!(
            extract_tags("` #ref`"),
            vec!["ref".to_string()],
            "space-padded form is a known false positive (no inline-code awareness)"
        );
    }

    // --- Tier 1: coexistence (all metadata axes parse independently) -------

    #[test]
    fn parse_task_line_parses_all_metadata_independently() {
        let body = "ship it \u{1F4C5} 2026-07-01 \u{23F3} 2026-06-20 \u{1F6EB} 2026-06-15 \u{2795} 2026-01-01 #urgent #backend \u{1F53C}";
        let tasks = parse_tasks(&format!("- [ ] {body}\n"), "d.md");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_date.as_deref(), Some("2026-07-01"));
        assert_eq!(tasks[0].scheduled_date.as_deref(), Some("2026-06-20"));
        assert_eq!(tasks[0].start_date.as_deref(), Some("2026-06-15"));
        assert_eq!(tasks[0].created_date.as_deref(), Some("2026-01-01"));
        assert_eq!(
            tasks[0].tags,
            vec!["urgent".to_string(), "backend".to_string()]
        );
        assert_eq!(tasks[0].priority, Some(Priority::Medium));
        assert!(tasks[0].done_date.is_none());
        assert!(tasks[0].cancelled_date.is_none());
    }

    // --- ymd_from_unix unit tests (ADR-0009 Phase 1) -----------------------

    #[test]
    fn ymd_from_unix_epoch_is_1970_01_01() {
        assert_eq!(ymd_from_unix(0), "1970-01-01");
    }

    #[test]
    fn ymd_from_unix_known_anchors() {
        // 2026-06-20 = day 20624 = 1_781_913_600 seconds.
        assert_eq!(ymd_from_unix(1_781_913_600), "2026-06-20");
        // 2024-02-29 (leap day) = day 19782 = 1_709_164_800 seconds.
        assert_eq!(ymd_from_unix(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn ymd_from_unix_handles_leap_days() {
        // Div-by-400 leap (2000) and the day after a leap day (2012-03-01) round-trip.
        assert_eq!(ymd_from_unix(951_782_400), "2000-02-29");
        assert_eq!(ymd_from_unix(1_330_560_000), "2012-03-01");
    }

    #[test]
    fn ymd_from_unix_pre_epoch_floor_division() {
        // One second before the epoch lands on 1969-12-31 (floor toward -inf).
        assert_eq!(ymd_from_unix(-1), "1969-12-31");
    }

    // --- rewrite_scheduled unit tests (ADR-0009 Phase 2) -------------------

    #[test]
    fn rewrite_mark_appends_when_no_hourglass() {
        let r = rewrite_scheduled("- [ ] buy shampoo", Some("2026-06-20"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [ ] buy shampoo ⏳ 2026-06-20".to_string())
        );
        // The appended token is parseable and is the only ⏳.
        let RewriteResult::Rewritten(s) = r else {
            unreachable!()
        };
        assert_eq!(extract_scheduled_date(&s).as_deref(), Some("2026-06-20"));
    }

    #[test]
    fn rewrite_mark_replaces_existing_date() {
        let r = rewrite_scheduled("- [ ] plan ⏳ 2026-06-21", Some("2026-06-20"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [ ] plan ⏳ 2026-06-20".to_string())
        );
    }

    #[test]
    fn rewrite_mark_preserves_due_date_tags_and_checkbox() {
        // 📅 due date, #tags, and the checkbox marker are untouched; only ⏳ is added.
        let r = rewrite_scheduled("- [x] ship 📅 2026-07-01 #urgent", Some("2026-06-20"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [x] ship 📅 2026-07-01 #urgent ⏳ 2026-06-20".to_string())
        );
        // Replacing an existing ⏳ must keep a preceding 📅 byte-for-byte.
        let r2 = rewrite_scheduled("- [ ] ship 📅 2026-07-01 ⏳ 2026-06-21", Some("2026-06-20"));
        assert_eq!(
            r2,
            RewriteResult::Rewritten("- [ ] ship 📅 2026-07-01 ⏳ 2026-06-20".to_string())
        );
    }

    #[test]
    fn rewrite_mark_keeps_vs16_and_spacing_when_replacing() {
        // The token's leading ⏳ + VS16 + whitespace run is preserved; only the
        // date bytes change.
        let r = rewrite_scheduled("- [ ] x \u{23F3}\u{FE0F}  2026-06-21", Some("2026-06-20"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [ ] x \u{23F3}\u{FE0F}  2026-06-20".to_string())
        );
    }

    #[test]
    fn rewrite_mark_is_idempotent_when_already_matching() {
        let r = rewrite_scheduled("- [ ] plan ⏳ 2026-06-20", Some("2026-06-20"));
        assert_eq!(r, RewriteResult::Unchanged);
    }

    #[test]
    fn rewrite_unmark_removes_token_and_preceding_space() {
        let r = rewrite_scheduled("- [ ] plan ⏳ 2026-06-20", None);
        assert_eq!(r, RewriteResult::Rewritten("- [ ] plan".to_string()));
    }

    #[test]
    fn rewrite_unmark_keeps_surrounding_content() {
        // Tags and the 📅 due date survive the unmark.
        let r = rewrite_scheduled("- [ ] ship 📅 2026-07-01 #urgent ⏳ 2026-06-20", None);
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [ ] ship 📅 2026-07-01 #urgent".to_string())
        );
    }

    #[test]
    fn rewrite_unmark_is_unchanged_when_no_token() {
        assert_eq!(
            rewrite_scheduled("- [ ] no marker here", None),
            RewriteResult::Unchanged
        );
    }

    #[test]
    fn rewrite_unparseable_when_existing_date_malformed() {
        // ⏳ present but the date is garbage → refuse, never guess (never append a 2nd ⏳).
        assert_eq!(
            rewrite_scheduled("- [ ] x ⏳ not-a-date", Some("2026-06-20")),
            RewriteResult::Unparseable
        );
        assert_eq!(
            rewrite_scheduled("- [ ] x ⏳ not-a-date", None),
            RewriteResult::Unparseable
        );
    }

    #[test]
    fn rewrite_unparseable_when_nbsp_instead_of_ascii_space() {
        // NBSP (U+00A0) is not ASCII whitespace → the lone ⏳ is malformed → refuse.
        let nbsp = "\u{00A0}";
        let line = format!("- [ ] x ⏳{nbsp}2026-06-20");
        assert_eq!(
            rewrite_scheduled(&line, Some("2026-06-21")),
            RewriteResult::Unparseable
        );
    }

    #[test]
    fn rewrite_unparseable_when_two_hourglasses() {
        // Ambiguous: which one do we edit? Refuse rather than guess.
        let line = "- [ ] x ⏳ 2026-06-20 ⏳ 2026-06-21";
        assert_eq!(
            rewrite_scheduled(line, Some("2026-06-22")),
            RewriteResult::Unparseable
        );
        assert_eq!(rewrite_scheduled(line, None), RewriteResult::Unparseable);
    }

    #[test]
    fn rewrite_unparseable_when_desired_malformed() {
        // Defensive: the write path never trusts the caller's date.
        assert_eq!(
            rewrite_scheduled("- [ ] x", Some("2026/06/20")),
            RewriteResult::Unparseable
        );
        assert_eq!(
            rewrite_scheduled("- [ ] x", Some("2026-13-40")),
            RewriteResult::Unparseable
        );
        assert_eq!(
            rewrite_scheduled("- [ ] x", Some("not-a-date")),
            RewriteResult::Unparseable
        );
    }

    #[test]
    fn rewrite_never_panics_on_arbitrary_input() {
        // A sampling of weird inputs must produce a result, not a panic.
        for (line, desired) in [
            ("", None),
            ("", Some("2026-06-20")),
            ("⏳", None),
            ("⏳ 2026-06-20", None),
            ("\u{FE0F}\u{FE0F}⏳", Some("2026-06-20")),
            ("- [ ] 🇯🇵 emoji ⏳", Some("2026-06-20")),
            ("- [ ] \0 binary ⏳ 2026-06-20", None),
        ] {
            let _ = rewrite_scheduled(line, desired);
        }
    }

    // ── rewrite_done_date unit tests (ADR-0012) ─────────────────────
    //
    // Mirrors the rewrite_scheduled suite on the `✅` axis. Identical grammar
    // + splice, different leading emoji — these tests pin that the
    // `rewrite_emoji_date` refactor preserved byte-for-byte behaviour when the
    // emoji parameter is `✅` (U+2705) instead of `⏳` (U+23F3).

    #[test]
    fn done_date_appended_when_none_present() {
        let r = rewrite_done_date("- [ ] buy shampoo", Some("2026-06-21"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [ ] buy shampoo ✅ 2026-06-21".to_string())
        );
        // The appended token is parseable and is the only ✅.
        let RewriteResult::Rewritten(s) = r else {
            unreachable!()
        };
        assert_eq!(extract_done_date(&s).as_deref(), Some("2026-06-21"));
    }

    #[test]
    fn done_date_replaced_when_other_date_present() {
        // Canonical re-done: existing ✅ date is overwritten with `desired`.
        let r = rewrite_done_date("- [x] ship ✅ 2026-06-19", Some("2026-06-21"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [x] ship ✅ 2026-06-21".to_string())
        );
    }

    #[test]
    fn done_date_preserves_due_and_tags_on_append() {
        // 📅 due date and #tags are untouched; the ✅ is appended AFTER the tags.
        let r = rewrite_done_date("- [x] ship 📅 2026-07-01 #urgent", Some("2026-06-21"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [x] ship 📅 2026-07-01 #urgent ✅ 2026-06-21".to_string())
        );
        // Replacing an existing ✅ must keep a preceding 📅 byte-for-byte.
        let r2 = rewrite_done_date("- [ ] ship 📅 2026-07-01 ✅ 2026-06-19", Some("2026-06-21"));
        assert_eq!(
            r2,
            RewriteResult::Rewritten("- [ ] ship 📅 2026-07-01 ✅ 2026-06-21".to_string())
        );
    }

    #[test]
    fn done_date_keeps_vs16_and_spacing_when_replacing() {
        // The token's leading ✅ + VS16 + whitespace run is preserved; only the
        // date bytes change.
        let r = rewrite_done_date("- [ ] x \u{2705}\u{FE0F}  2026-06-19", Some("2026-06-21"));
        assert_eq!(
            r,
            RewriteResult::Rewritten("- [ ] x \u{2705}\u{FE0F}  2026-06-21".to_string())
        );
    }

    #[test]
    fn done_date_unchanged_when_already_today() {
        // Idempotent: an already-matching ✅ date returns Unchanged (only the
        // checkbox flip would be written by the daemon).
        let r = rewrite_done_date("- [x] ship ✅ 2026-06-21", Some("2026-06-21"));
        assert_eq!(r, RewriteResult::Unchanged);
    }

    #[test]
    fn done_date_removed_on_unmark() {
        // Un-completion: the ✅ token and its single preceding ASCII space are
        // stripped, mirroring how unmark removes a ⏳.
        let r = rewrite_done_date("- [x] ship ✅ 2026-06-20", None);
        assert_eq!(r, RewriteResult::Rewritten("- [x] ship".to_string()));
    }

    #[test]
    fn done_date_unparseable_on_malformed() {
        // A ✅ present but the date is garbage → refuse, never guess (never
        // append a 2nd ✅). Fires for both Some (stamp) and None (clear).
        assert_eq!(
            rewrite_done_date("- [x] x ✅ not-a-date", Some("2026-06-21")),
            RewriteResult::Unparseable
        );
        assert_eq!(
            rewrite_done_date("- [x] x ✅ not-a-date", None),
            RewriteResult::Unparseable
        );
    }

    #[test]
    fn done_date_unparseable_on_nbsp_instead_of_ascii_space() {
        // NBSP (U+00A0) is not ASCII whitespace → the lone ✅ is malformed → refuse.
        let nbsp = "\u{00A0}";
        let line = format!("- [x] x ✅{nbsp}2026-06-20");
        assert_eq!(
            rewrite_done_date(&line, Some("2026-06-21")),
            RewriteResult::Unparseable
        );
    }

    #[test]
    fn done_date_unparseable_on_two_done_emojis() {
        // Ambiguous: which one do we edit? Refuse rather than guess.
        let line = "- [x] x ✅ 2026-06-20 ✅ 2026-06-21";
        assert_eq!(
            rewrite_done_date(line, Some("2026-06-22")),
            RewriteResult::Unparseable
        );
        assert_eq!(rewrite_done_date(line, None), RewriteResult::Unparseable);
    }

    #[test]
    fn done_date_unparseable_on_bad_desired() {
        // Defensive: the write path never trusts the caller's date.
        assert_eq!(
            rewrite_done_date("- [x] x", Some("2026/06/21")),
            RewriteResult::Unparseable
        );
        assert_eq!(
            rewrite_done_date("- [x] x", Some("2026-13-40")),
            RewriteResult::Unparseable
        );
        assert_eq!(
            rewrite_done_date("- [x] x", Some("not-a-date")),
            RewriteResult::Unparseable
        );
    }

    #[test]
    fn done_date_unchanged_when_no_token_on_clear() {
        // Clearing a ✅ that isn't there is a no-op (only the flip would write).
        assert_eq!(
            rewrite_done_date("- [x] ship", None),
            RewriteResult::Unchanged
        );
    }

    #[test]
    fn done_date_preserves_recurring_token() {
        // A recurring task's 🔁 recurrence token is ordinary trailing content —
        // preserved byte-for-byte — while only the ✅ date token is replaced,
        // removed, or appended. Parallel to rewrite_scheduled's recurring test.
        let line = "- [ ] build feature 🔁 every day ✅ 2026-06-19";

        // Replace the date: 🔁 preserved, only the ✅ date swaps.
        assert_eq!(
            rewrite_done_date(line, Some("2026-07-01")),
            RewriteResult::Rewritten("- [ ] build feature 🔁 every day ✅ 2026-07-01".to_string())
        );

        // Remove the done date: 🔁 preserved, ✅ token gone.
        assert_eq!(
            rewrite_done_date(line, None),
            RewriteResult::Rewritten("- [ ] build feature 🔁 every day".to_string())
        );

        // Idempotent: asking for the date already present is a no-op.
        assert_eq!(
            rewrite_done_date(line, Some("2026-06-19")),
            RewriteResult::Unchanged
        );

        // Append when no ✅ is present: the recurrence token is preserved and the
        // new ✅ is appended at the end of the content.
        let recurring_no_done = "- [ ] build feature 🔁 every day";
        assert_eq!(
            rewrite_done_date(recurring_no_done, Some("2026-07-01")),
            RewriteResult::Rewritten("- [ ] build feature 🔁 every day ✅ 2026-07-01".to_string())
        );
    }

    // ── toggle_bullet unit tests (ADR-0011) ────────────────────────

    #[test]
    fn toggle_bullet_converts_open_checkbox_to_bullet() {
        assert_eq!(
            toggle_bullet("- [ ] First task"),
            RewriteResult::Rewritten("- First task".to_string())
        );
    }

    #[test]
    fn toggle_bullet_converts_done_checkbox_to_bullet() {
        assert_eq!(
            toggle_bullet("- [x] Done task"),
            RewriteResult::Rewritten("- Done task".to_string())
        );
    }

    #[test]
    fn toggle_bullet_converts_bullet_to_open_checkbox() {
        assert_eq!(
            toggle_bullet("- Just text"),
            RewriteResult::Rewritten("- [ ] Just text".to_string())
        );
    }

    #[test]
    fn toggle_bullet_preserves_indentation() {
        assert_eq!(
            toggle_bullet("  - [ ] indented task"),
            RewriteResult::Rewritten("  - indented task".to_string())
        );
        assert_eq!(
            toggle_bullet("  - indented bullet"),
            RewriteResult::Rewritten("  - [ ] indented bullet".to_string())
        );
    }

    #[test]
    fn count_indent_spaces_tabs_and_mixed() {
        // No leading whitespace.
        assert_eq!(count_indent("- [ ] top"), 0);
        // Spaces count 1:1.
        assert_eq!(count_indent("  - [ ] nested"), 2);
        assert_eq!(count_indent("    - [ ] deep"), 4);
        // A tab advances to the next multiple of 4.
        assert_eq!(count_indent("\t- [ ] tabbed"), 4);
        // Column 2 + tab → next multiple of 4 = 4.
        assert_eq!(count_indent("  \t- [ ] mixed"), 4);
        // Column 5 + tab → next multiple of 4 = 8.
        assert_eq!(count_indent("     \t- [ ] mixed2"), 8);
        // Blockquote marker is not whitespace — indent is 0.
        assert_eq!(count_indent("> - [ ] quoted"), 0);
        // Indented blockquote: 2 spaces of whitespace, then `>`.
        assert_eq!(count_indent("  > - [ ] quoted"), 2);
        // Empty / all-whitespace lines.
        assert_eq!(count_indent(""), 0);
        assert_eq!(count_indent("   "), 3);
    }

    #[test]
    fn parse_tasks_captures_indent_for_nested_subtasks() {
        let md = "\
- [ ] parent
  - [ ] child
    - [ ] grandchild
";
        let tasks = parse_tasks(md, "nest.md");
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].indent, 0, "parent at column 0");
        assert_eq!(tasks[1].indent, 2, "child at column 2");
        assert_eq!(tasks[2].indent, 4, "grandchild at column 4");
    }

    #[test]
    fn toggle_bullet_preserves_blockquote() {
        assert_eq!(
            toggle_bullet("> - [ ] quoted task"),
            RewriteResult::Rewritten("> - quoted task".to_string())
        );
    }

    #[test]
    fn toggle_bullet_is_self_inverse_for_open_checkboxes() {
        // Only open checkboxes round-trip exactly (done/in-progress open as `[ ]`).
        let cases = [
            "- [ ] deploy the migration",
            "  - [ ] indented task",
            "> - [ ] quoted task",
            "- [ ] has body text with multiple words",
        ];
        for original in &cases {
            let first = toggle_bullet(original);
            let RewriteResult::Rewritten(intermediate) = first else {
                panic!("first toggle should be Rewritten, got {first:?} on {original:?}");
            };
            let second = toggle_bullet(&intermediate);
            let RewriteResult::Rewritten(restored) = second else {
                panic!("second toggle should be Rewritten, got {second:?} on {intermediate:?}");
            };
            assert_eq!(
                &restored, original,
                "toggle_bullet should be self-inverse on open checkbox"
            );
        }
    }

    #[test]
    fn toggle_bullet_non_open_restores_as_open() {
        // Done/in-progress tasks always become `[ ]` when toggled back from bullet.
        let r1 = match toggle_bullet("- [x] done task") {
            RewriteResult::Rewritten(s) => toggle_bullet(&s),
            _ => panic!("first toggle should be Rewritten"),
        };
        assert_eq!(r1, RewriteResult::Rewritten("- [ ] done task".to_string()));
        let r2 = match toggle_bullet("- [/] in-progress") {
            RewriteResult::Rewritten(s) => toggle_bullet(&s),
            _ => panic!("first toggle should be Rewritten"),
        };
        assert_eq!(
            r2,
            RewriteResult::Rewritten("- [ ] in-progress".to_string())
        );
    }

    #[test]
    fn toggle_bullet_unparseable_on_no_bullet_char() {
        assert_eq!(toggle_bullet("plain text"), RewriteResult::Unparseable);
        assert_eq!(toggle_bullet("[ ] not a task"), RewriteResult::Unparseable);
    }

    #[test]
    fn toggle_bullet_unparseable_on_missing_whitespace_after_bullet() {
        assert_eq!(toggle_bullet("-"), RewriteResult::Unparseable);
        assert_eq!(toggle_bullet("-[ ] no space"), RewriteResult::Unparseable);
    }

    #[test]
    fn toggle_bullet_never_panics_on_arbitrary_input() {
        // A sampling of weird inputs must produce a result, not a panic.
        for line in [
            "",
            "-",
            "   ",
            "-   ",
            "- [] no checkbox space",
            "- [",
            "- [ ",
            "- [ ]",
            "\0",
            "- [ ] 🇯🇵 emoji here",
            "> just a quote",
        ] {
            let _ = toggle_bullet(line);
        }
    }

    // ── taski_skip_enabled unit tests (ADR-0017) ────────────────────────
    //
    // The frontmatter `taski-skip: true` opt-out. Pure detector — exercises
    // every grammar arm: truthy values, non-truthy spellings, missing/misplaced
    // frontmatter, nested keys, CRLF, and first-key-wins.

    #[test]
    fn taski_skip_enabled_true() {
        let md = "---\ntaski-skip: true\n---\n\n- [ ] body task\n";
        assert!(taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_false_value() {
        let md = "---\ntaski-skip: false\n---\n\n- [ ] body task\n";
        assert!(!taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_yes_not_honored() {
        // Deliberately rejects YAML-1.1 truthy spellings (yes/on/1) — only
        // literal `true` is honored so the opt-in is explicit.
        assert!(!taski_skip_enabled("---\ntaski-skip: yes\n---\n"));
        assert!(!taski_skip_enabled("---\ntaski-skip: on\n---\n"));
        assert!(!taski_skip_enabled("---\ntaski-skip: 1\n---\n"));
    }

    #[test]
    fn taski_skip_enabled_empty_value() {
        // `taski-skip:` with no value → not truthy.
        assert!(!taski_skip_enabled("---\ntaski-skip:\n---\n"));
    }

    #[test]
    fn taski_skip_enabled_quoted_true() {
        // Both single- and double-quoted `"true"` are truthy.
        assert!(taski_skip_enabled("---\ntaski-skip: \"true\"\n---\n"));
        assert!(taski_skip_enabled("---\ntaski-skip: 'true'\n---\n"));
        // Quoted non-true is not truthy.
        assert!(!taski_skip_enabled("---\ntaski-skip: \"false\"\n---\n"));
        assert!(!taski_skip_enabled("---\ntaski-skip: 'yes'\n---\n"));
    }

    #[test]
    fn taski_skip_enabled_case_insensitive_true() {
        // `True` / `TRUE` / `tRuE` all match (case-insensitive literal `true`).
        assert!(taski_skip_enabled("---\ntaski-skip: True\n---\n"));
        assert!(taski_skip_enabled("---\ntaski-skip: TRUE\n---\n"));
        assert!(taski_skip_enabled("---\ntaski-skip: tRuE\n---\n"));
    }

    #[test]
    fn taski_skip_enabled_no_frontmatter() {
        // Plain note with no `---` opener at all.
        let md = "# Title\n\n- [ ] task\n";
        assert!(!taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_hr_not_frontmatter() {
        // A `---` horizontal rule NOT on line 1 (a blank line first) is not a
        // frontmatter opener, so the flag is not honored.
        let md = "\n---\ntaski-skip: true\n---\n\n- [ ] task\n";
        assert!(!taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_fenced_block_not_frontmatter() {
        // `taski-skip: true` inside a fenced code block at the very top of the
        // file: line 1 is ``` not `---`, so this is not frontmatter.
        let md = "```\ntaski-skip: true\n```\n\n- [ ] real task\n";
        assert!(!taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_nested_key_ignored() {
        // An indented `  taski-skip: true` nested under another key is ignored
        // (top-level / column-0 key only). The note is NOT skipped.
        let md = "---\nmeta:\n  taski-skip: true\n---\n\n- [ ] task\n";
        assert!(!taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_first_key_wins() {
        // Two `taski-skip:` lines: the first one wins. First `false`, second
        // `true` → the note is NOT skipped (first value wins).
        let md_false_then_true = "---\ntaski-skip: false\ntaski-skip: true\n---\n";
        assert!(!taski_skip_enabled(md_false_then_true));
        // And the reverse: first `true`, second `false` → skipped.
        let md_true_then_false = "---\ntaski-skip: true\ntaski-skip: false\n---\n";
        assert!(taski_skip_enabled(md_true_then_false));
    }

    #[test]
    fn taski_skip_enabled_crlf() {
        // CRLF line endings: `markdown.lines()` strips the trailing `\r`, and
        // the fence comparison also `.trim_end()`s it. The flag is honored.
        let md = "---\r\ntaski-skip: true\r\n---\r\n\r\n- [ ] body\r\n";
        assert!(taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_no_closing_fence() {
        // Opener `---` present but never closed → treated as no frontmatter;
        // the flag is absent even though the key appears below the opener.
        let md = "---\ntaski-skip: true\n\n- [ ] no closing fence\n";
        assert!(!taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_prefix_key_rejected() {
        // `taski-skipper: true` is a different key (the char after `taski-skip`
        // is `p`, not whitespace/`:`), so it is rejected.
        let md = "---\ntaski-skipper: true\n---\n\n- [ ] task\n";
        assert!(!taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_extra_keys_around() {
        // Other keys before and after the flag don't affect detection; the
        // flag is still honored when it is present and truthy.
        let md = "---\ntitle: My Note\ntags: [game]\ntaski-skip: true\ncreated: 2026-06-22\n---\n\n- [ ] task\n";
        assert!(taski_skip_enabled(md));
    }

    #[test]
    fn taski_skip_enabled_tolerates_whitespace_around_colon() {
        // Optional whitespace around the colon is accepted per the grammar
        // (`taski-skip : true` and `taski-skip:   true` both truthy).
        assert!(taski_skip_enabled("---\ntaski-skip : true\n---\n"));
        assert!(taski_skip_enabled("---\ntaski-skip:   true\n---\n"));
    }

    #[test]
    fn taski_skip_enabled_grammar_edge_pins() {
        // No space after the colon is fine — the grammar is `:\s*`, so the value
        // parser does `trim_start` then `strip_prefix(':')`; `taski-skip:true` →
        // `Some("true")` → truthy.
        assert!(taski_skip_enabled("---\ntaski-skip:true\n---\n"));
        // Trailing whitespace on the value is trimmed by `is_taski_skip_truthy`.
        assert!(taski_skip_enabled("---\ntaski-skip: true   \n---\n"));
        // An indented `  ---` does NOT close the block: the closer comparison
        // (`line.trim_end() == "---"`) only strips TRAILING whitespace, so the
        // leading spaces make `"  ---"` != `"---"`. With no real closer anywhere,
        // the block is unclosed → no frontmatter → the flag does not take effect.
        assert!(!taski_skip_enabled("---\ntaski-skip: true\n  ---\n"));
        // And when a real `---` closer follows the indented one, the indented line
        // is ignored (it doesn't end the block) and the flag IS honored.
        assert!(taski_skip_enabled(
            "---\ntaski-skip: true\n  ---\nbody\n---\n"
        ));
    }

    #[test]
    fn taski_skip_enabled_empty_markdown() {
        assert!(!taski_skip_enabled(""));
    }

    #[test]
    fn taski_skip_enabled_only_opener() {
        // Just `---` with nothing else: no closing fence and no key → false.
        assert!(!taski_skip_enabled("---"));
        assert!(!taski_skip_enabled("---\n"));
    }

    // --- ADR-0019: task-note oracles ---------------------------------------

    #[test]
    fn insert_notes_link_before_metadata() {
        // The link lands in the description, before the ⏳ token, one space each side.
        assert_eq!(
            insert_notes_link("- [ ] Redesign page ⏳ 2026-06-25", "1234"),
            "- [ ] Redesign page [[#notes-1234|Notes]] ⏳ 2026-06-25"
        );
    }

    #[test]
    fn insert_notes_link_no_metadata_appends_at_end() {
        assert_eq!(
            insert_notes_link("- [ ] Plain task", "1234"),
            "- [ ] Plain task [[#notes-1234|Notes]]"
        );
        // Trailing whitespace is trimmed before the appended link.
        assert_eq!(
            insert_notes_link("- [ ] Plain task   ", "9"),
            "- [ ] Plain task [[#notes-9|Notes]]"
        );
    }

    #[test]
    fn insert_notes_link_is_idempotent_for_same_id() {
        let once = insert_notes_link("- [ ] Task 📅 2026-01-01", "77");
        assert_eq!(insert_notes_link(&once, "77"), once);
    }

    #[test]
    fn insert_notes_link_before_priority_emoji() {
        assert_eq!(
            insert_notes_link("- [ ] Task 🔼", "5"),
            "- [ ] Task [[#notes-5|Notes]] 🔼"
        );
    }

    #[test]
    fn insert_notes_link_noop_on_non_task() {
        assert_eq!(insert_notes_link("not a task", "1"), "not a task");
    }

    #[test]
    fn notes_link_id_roundtrip() {
        let line = insert_notes_link("- [ ] Task ⏳ 2026-06-25", "1719153000123");
        assert_eq!(notes_link_id(&line), Some("1719153000123".to_string()));
        assert_eq!(notes_link_id("- [ ] no link here"), None);
    }

    #[test]
    fn note_bullet_for_basic() {
        assert_eq!(note_bullet_for("  went with hero  "), "- went with hero");
        assert_eq!(note_bullet_for("a\nb"), "- ab"); // newlines stripped (single-line)
    }

    #[test]
    fn note_bullet_for_escapes_phantom_checkbox() {
        // A note whose text forms a checkbox must NOT parse as a task.
        let bullet = note_bullet_for("[ ] buy milk");
        assert_eq!(bullet, "- \\[ ] buy milk");
        assert!(task_captures(&bullet).is_none());
        // A plain note is never escaped and is never a task.
        let plain = note_bullet_for("just a thought");
        assert!(task_captures(&plain).is_none());
    }

    // ── permute_lines (ADR-0020) ───────────────────────────────────────────

    #[test]
    fn permute_lines_swaps_two_adjacent() {
        let s = "- [ ] a\n- [ ] b\n- [ ] c\n";
        // Move line 2 above line 1: desired order [2, 1] over positions {1, 2}.
        assert_eq!(permute_lines(s, &[2, 1]), "- [ ] b\n- [ ] a\n- [ ] c\n");
    }

    #[test]
    fn permute_lines_rotation() {
        let s = "- [ ] x\n- [ ] a\n- [ ] b\n";
        // Bubble line 3 to the top: new top-to-bottom order is [3, 1, 2].
        assert_eq!(permute_lines(s, &[3, 1, 2]), "- [ ] b\n- [ ] x\n- [ ] a\n");
    }

    #[test]
    fn permute_lines_subset_leaves_others_in_place() {
        // Only lines 1 and 3 are reordered; line 2 (a non-listed line) stays put.
        let s = "- [ ] a\nplain prose\n- [ ] c\n";
        assert_eq!(permute_lines(s, &[3, 1]), "- [ ] c\nplain prose\n- [ ] a\n");
    }

    #[test]
    fn permute_lines_preserves_crlf_per_position() {
        // Position 1 is CRLF, position 2 is LF: terminators stay with positions.
        let s = "- [ ] a\r\n- [ ] b\n";
        assert_eq!(permute_lines(s, &[2, 1]), "- [ ] b\r\n- [ ] a\n");
    }

    #[test]
    fn permute_lines_no_trailing_newline() {
        let s = "- [ ] a\n- [ ] b";
        assert_eq!(permute_lines(s, &[2, 1]), "- [ ] b\n- [ ] a");
    }

    #[test]
    fn permute_lines_identity_and_invalid_are_noops() {
        let s = "- [ ] a\n- [ ] b\n- [ ] c\n";
        assert_eq!(permute_lines(s, &[1, 2, 3]), s); // already ascending
        assert_eq!(permute_lines(s, &[]), s); // empty
        assert_eq!(permute_lines(s, &[0, 1]), s); // line 0 invalid
        assert_eq!(permute_lines(s, &[1, 9]), s); // out of range
        assert_eq!(permute_lines(s, &[2, 2]), s); // duplicate
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// The "never-corrupts" contract for the reorder oracle: for any note and
        /// any permutation of any subset of its lines, the output is line-count-
        /// preserving, a true permutation of line contents, pins every non-listed
        /// line, and is invertible (ADR-0020).
        #[test]
        fn permute_lines_never_corrupts(
            // 1..8 short non-empty line bodies; CRLF or LF; optional trailing
            // newline. Bodies are non-empty so `bodies.len()` equals both the
            // `lines()` count and the oracle's segment count (an empty body could
            // collapse a `\n`-join into fewer lines than `bodies.len()`).
            bodies in proptest::collection::vec("[a-c]{1,4}", 1..8usize),
            use_crlf in proptest::prelude::any::<bool>(),
            trailing_nl in proptest::prelude::any::<bool>(),
            // A seed used to pick a subset + shuffle it deterministically below.
            seed in proptest::prelude::any::<u64>(),
        ) {
            let sep = if use_crlf { "\r\n" } else { "\n" };
            let mut content = bodies.join(sep);
            if trailing_nl {
                content.push_str(sep);
            }
            let n = bodies.len();

            // Build a subset of line numbers (every other line, by the seed parity)
            // then rotate it by one — a non-trivial permutation when len >= 2.
            let mut subset: Vec<usize> = (1..=n)
                .filter(|i| (i.wrapping_add(seed as usize)) % 2 == 0)
                .collect();
            if subset.len() >= 2 {
                subset.rotate_left(1);
            }

            let out = permute_lines(&content, &subset);

            // (a) line count preserved.
            let in_lines: Vec<&str> = content.lines().collect();
            let out_lines: Vec<&str> = out.lines().collect();
            proptest::prop_assert_eq!(in_lines.len(), out_lines.len());

            // (b) permutation: the multiset of line contents is unchanged.
            let mut a = in_lines.clone();
            let mut b = out_lines.clone();
            a.sort_unstable();
            b.sort_unstable();
            proptest::prop_assert_eq!(a, b);

            // (c) non-listed lines pinned in place.
            for i in 1..=n {
                if !subset.contains(&i) {
                    proptest::prop_assert_eq!(in_lines[i - 1], out_lines[i - 1]);
                }
            }

            // (d) invertible: applying the inverse permutation restores the input.
            //     inverse[target_pos] = source line that landed there.
            let mut targets: Vec<usize> = subset.clone();
            targets.sort_unstable();
            // desired_order maps i-th target <- subset[i]; the inverse, expressed in
            // the same "desired_order over the same positions" form, sends each
            // target back to its origin.
            let mut inverse = vec![0usize; subset.len()];
            for (i, &src) in subset.iter().enumerate() {
                // src's content sits at targets[i] after the forward permute; to
                // invert, the position targets[i] must supply content back to src.
                let pos_idx = targets.iter().position(|&t| t == src).unwrap();
                inverse[pos_idx] = targets[i];
            }
            let restored = permute_lines(&out, &inverse);
            proptest::prop_assert_eq!(restored, content);
        }
    }

    // ── remove_lines / extract_lines (ADR-0021) ─────────────────────────────

    #[test]
    fn extract_lines_returns_named_contents_in_order() {
        let s = "- [ ] a\n- [x] b\n- [-] c\n";
        // Ask for lines 3 then 2 — order follows the argument, terminators stripped.
        assert_eq!(extract_lines(s, &[3, 2]), vec!["- [-] c", "- [x] b"]);
    }

    #[test]
    fn extract_lines_skips_out_of_range() {
        let s = "- [x] a\n- [ ] b\n";
        assert_eq!(extract_lines(s, &[1, 9, 0]), vec!["- [x] a"]);
    }

    #[test]
    fn remove_lines_deletes_named_and_pins_survivors() {
        let s = "# Inbox\n- [ ] a\n- [x] b\n- [-] c\n";
        // Remove the two closed tasks (lines 3, 4); heading + open task stay.
        assert_eq!(remove_lines(s, &[3, 4]), "# Inbox\n- [ ] a\n");
    }

    #[test]
    fn remove_lines_middle_keeps_terminators() {
        let s = "- [ ] a\n- [x] b\n- [ ] c\n";
        assert_eq!(remove_lines(s, &[2]), "- [ ] a\n- [ ] c\n");
    }

    #[test]
    fn remove_lines_preserves_crlf_on_survivors() {
        let s = "- [ ] a\r\n- [x] b\r\n- [ ] c\r\n";
        assert_eq!(remove_lines(s, &[2]), "- [ ] a\r\n- [ ] c\r\n");
    }

    #[test]
    fn remove_lines_last_line_without_trailing_newline() {
        // Removing the final, unterminated line leaves the prior line's own `\n`.
        let s = "- [ ] a\n- [x] b";
        assert_eq!(remove_lines(s, &[2]), "- [ ] a\n");
    }

    #[test]
    fn remove_lines_empty_and_out_of_range_are_noops() {
        let s = "- [ ] a\n- [x] b\n";
        assert_eq!(remove_lines(s, &[]), s);
        assert_eq!(remove_lines(s, &[0]), s);
        assert_eq!(remove_lines(s, &[9]), s);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// The "never-corrupts" contract for the archive move's structural oracles
        /// (ADR-0021): for any note and any subset of its lines, `extract_lines` ⊎
        /// the survivors of `remove_lines` equals the original line multiset (no
        /// loss), survivors stay byte-identical and in order, and the removed count
        /// is exact.
        #[test]
        fn remove_and_extract_never_lose_a_line(
            bodies in proptest::collection::vec("[a-c]{1,4}", 1..8usize),
            use_crlf in proptest::prelude::any::<bool>(),
            trailing_nl in proptest::prelude::any::<bool>(),
            seed in proptest::prelude::any::<u64>(),
        ) {
            let sep = if use_crlf { "\r\n" } else { "\n" };
            let mut content = bodies.join(sep);
            if trailing_nl {
                content.push_str(sep);
            }
            let n = bodies.len();

            // Pick a subset of line numbers by seed parity (the "completed" lines).
            let subset: Vec<usize> = (1..=n)
                .filter(|i| (i.wrapping_add(seed as usize)) % 2 == 0)
                .collect();

            let extracted = extract_lines(&content, &subset);
            let kept = remove_lines(&content, &subset);

            let in_lines: Vec<&str> = content.lines().collect();
            let kept_lines: Vec<&str> = kept.lines().collect();

            // (a) removed count is exact.
            proptest::prop_assert_eq!(kept_lines.len(), n - subset.len());

            // (b) no loss: extracted ⊎ kept == original (as multisets of contents).
            let mut union: Vec<String> =
                kept_lines.iter().map(|s| s.to_string()).collect();
            union.extend(extracted.iter().cloned());
            let mut original: Vec<String> =
                in_lines.iter().map(|s| s.to_string()).collect();
            union.sort();
            original.sort();
            proptest::prop_assert_eq!(union, original);

            // (c) survivors stay in their original relative order, byte-identical.
            let survivors: Vec<&str> = (1..=n)
                .filter(|i| !subset.contains(i))
                .map(|i| in_lines[i - 1])
                .collect();
            proptest::prop_assert_eq!(kept_lines, survivors);
        }
    }
}
