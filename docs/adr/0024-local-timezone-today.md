# ADR-0024: "Today" is the user's local calendar date, not UTC

- **Status:** Accepted
- **Date:** 2026-09-07
- **Decides:** How Taski derives "today" for the `T`/`O` view boundaries and the
  `⏳`/`✅`/`❌`/`➕` stamp dates. **Amends [ADR-0009](./0009-scheduled-date-today.md)**
  Phase 1's date-derivation detail (its `ymd_from_unix` was UTC-based); **amends**
  ADR-0012/0013/0014 stamp semantics only in the same "which date" sense. No schema
  change, no `pending_actions` change, no write-path structure change — the composed
  stamps are byte-identical in shape, just carrying the correct local date.

## Context

Every date Taski computes passes through the pure `taski_core::ymd_from_unix`, which
converts a raw Unix timestamp to a calendar date via Howard Hinnant's `civil_from_days` —
an **algorithm over UTC days**, with no timezone input. The TUI's `today_string()` and the
daemon's stamp wrappers (`process_action`, `process_quick_add`,
`process_quick_add_undo`) all fed it `SystemTime::now()` seconds unadjusted, so every
"today" in the product was silently the **UTC** calendar date.

For a UTC-resident user this is unobservable. For anyone else it shifts the entire app by
the offset — reported concretely on an `Australia/Perth` (UTC+8) machine:

- Between **local midnight and 08:00**, the app believes "today" is still *yesterday*.
  Unfinished tasks scheduled for the (now past) local date kept surfacing in the Today
  view hours after the user's calendar had moved on; symmetrically, freshly-dated tasks
  were missing from Today until 08:00.
- Any `t` (⏳), done (✅), cancel (❌), or quick-add (➕) stamp written in that window was
  written with **yesterday's date** — permanently, into the user's notes.

This is a bug, not a design choice: "today" in a personal execution tool unambiguously
means the user's local date. ADR-0009 specified the mechanism (pure, no date crate) and
the UTC implementation slipped in as an implementation artifact.

## Decision

1. **`taski_core::ymd_from_unix_local(secs, utc_offset_secs)`** — new pure helper: the
   existing UTC conversion shifted by a caller-supplied offset (positive east of UTC).
   Purity convention unchanged: the environment-dependent value (the offset) is an
   *argument*, mirroring `taski_config::config_path_from`.
2. **`taski_db::local_utc_offset_secs()`** — the one impure probe: `localtime_r(3)` +
   `tzset(3)` via a new `libc` dependency, returning `tm_gmtoff`. Lives in `taski-db`
   (both consumers already depend on it — same rationale as the `ymd_from_unix`
   re-export; keeps `taski-core` free of env probing). Non-unix targets fall back to `0`
   (the pre-fix behavior).
3. **All five "today" consumers switch to local:**
   - TUI `today_string()` (drives `T` Today view, `O` Overdue boundary, and the `t`
     mark-for-today payload — the desired `⏳` date travels in the action payload, so the
     TUI's clock is the write clock for that gesture).
   - Daemon `process_action` (✅/❌ stamps), `process_quick_add` and
     `process_quick_add_undo` (➕ stamp / undo match).

DST correctness comes free: `localtime_r` consults the system tz database, and the probe
runs per call, so a session spanning a DST transition picks up the new offset on the next
500ms/750ms tick.

### Scope deliberately held (non-goals)

- **No schema bump** — `due_date`/`scheduled_date`/stamp columns are opaque `YYYY-MM-DD`
  text; their meaning ("the writer's local date") is unchanged, only now honored.
- **No migration of previously-written stamps.** Historical `✅`/`❌`/`➕`/`⏳` values
  written by the UTC clock stay as-is; for a UTC+8 user in the 00:00–08:00 window they
  read as one day early, which is cosmetic, bounded, and self-consistent with the index
  they were scanned under.
- **`parse_tasks` date extraction is untouched** — vault-side dates are user-authored
  text; only *computed* dates change.
- **No new config knob** ("force UTC") — YAGNI for a single-user personal tool.

## Consequences

- ✅ Today/Overdue boundaries roll over at **local midnight**; sessions spanning local
  midnight (ADR-0022's edge case) refresh correctly via the existing per-tick
  `today_string()` recompute.
- ✅ All stamps written from now on carry the user's local date, matching Obsidian-Tasks
  expectations for Tasks-plugin queries.
- ⚠️ **New dependency: `libc`** (0.2, already in the tree transitively; declared
  explicitly in `taski-db`). Recorded in `tech.md`.
- ⚠️ Two small `unsafe` FFI calls (`tzset`, `localtime_r`) enter `taski-db` — both
  pointer-to-own-storage, no retained state, unit-smoke-tested.
- ⚠️ Midnight-window behavior changes for non-UTC users: tasks that used to linger in
  Today until 08:00 now roll into Overdue (per ADR-0023) at local midnight. That is the
  intended semantics.

## Alternatives considered

- **Add `chrono`/`time`/`jiff`.** Rejected: the no-date-crate stance (ADR-0009) has held
  well; the pure algorithm + offset-argument split keeps the entire date math pure and
  unit-tested, at the cost of ~20 lines of FFI. Revisit only if timezone needs grow beyond
  "current local offset" (e.g. per-note timezones — not imagined).
- **`TZ` env parsing / `/etc/localtime` reading in pure Rust.** Rejected: re-implementing
  the tz database (TZif parsing, POSIX TZ strings, DST rules) is vastly more code and
  risk than one `localtime_r` call.
- **SQLite `strftime('%Y-%m-%d','now','localtime')` via the open connection.** Rejected:
  couples a clock read to DB-handle availability and hides a timezone behavior inside the
  storage layer.
- **Probe once at startup, cache the offset.** Rejected: breaks across DST transitions in
  long-lived sessions; the probe is nanoseconds against a 500ms event-loop tick.

## Edge cases

| Case | Behavior |
|---|---|
| Session spans local midnight | Next refresh tick recomputes `today_string()` with the (same) offset; views roll over. |
| Session spans a DST transition | `localtime_r` per call returns the *new* offset after the transition; stamps/flip dates reflect it. |
| Pre-epoch clock | Falls back to epoch seconds, as before. |
| Non-unix host | Offset probe returns 0 — pre-ADR-0024 (UTC) behavior. |
| `localtime_r` failure | Returns offset 0 (UTC fallback) rather than erroring the write/view path. |
| UTC-resident user | Offset 0 → byte-identical behavior to pre-fix. |

## References

- [ADR-0009](./0009-scheduled-date-today.md) — introduced `ymd_from_unix` (UTC-based);
  the pure-oracle stance stands, the UTC default is amended here.
- [ADR-0022](./0022-today-view-includes-due-today.md) — Today view scope; its
  "session spans midnight" edge case is what the per-tick recompute now correctly serves
  in local time.
- [ADR-0023](./0023-overdue-includes-past-scheduled.md) — companion read-path change; the
  local-midnight rollover is where past-scheduled tasks surface.
- `crates/taski-core/src/lib.rs` — `ymd_from_unix_local`; `crates/taski-db/src/lib.rs` —
  `local_utc_offset_secs`; `crates/taski-tui/src/lib.rs` — `today_string`;
  `crates/taski-daemon/src/lib.rs` — the three wall-clock wrappers.
