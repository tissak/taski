# ADR-0015: Open-in-Obsidian deep-link gesture

- **Status:** Accepted
- **Date:** 2026-06-21
- **Decides:** How Taski lets the user jump from a selected task in the TUI to that
  task's source note (and optionally its exact line) in Obsidian. A new `o` TUI key
  builds an `obsidian://` deep-link URL from the task's `note_path` + `line_number`
  and hands it to the OS to open Obsidian. This is **Taski's first read-only,
  TUI-local, OS-boundary gesture** — it touches neither the vault files nor the
  daemon/DB, and therefore does **not** amend [ADR-0002](./0002-write-back-through-daemon.md)
  or any write-back ADR.

## Context

Every task the TUI shows originates in a Markdown note in the vault, and Taski
already carries each task's `note_path` (vault-relative) and `line_number`
(1-based) in the index. The read-only context pane ([ADR-0006](./0006-note-content-cached-in-index.md))
lets the user *preview* the surrounding note inline — but there was no way to
*jump into Obsidian itself* at the task's location to edit, see wider context,
follow a wikilink, or use any Obsidian-side tooling on that note. The user had to
manually find the note in Obsidian's file tree.

Obsidian registers a `obsidian://` URL scheme that external apps can invoke to
open a specific file (and, via a community plugin, target a specific line). This
is the natural bridge: the TUI already knows exactly where each task lives.

### Why this is NOT a write-back feature (and does not touch ADR-0002)

All ADRs from 0003 through 0014 concern **write-back** — reflecting a TUI action
into the vault's bytes, routed through the daemon as sole writer (ADR-0002) under
the refuse-on-conflict TOCTOU discipline (ADR-0004). Open-in-Obsidian does none
of this. It:

- does **not** mutate the vault — it asks the OS to focus Obsidian at a location;
- does **not** route through the daemon or the `pending_actions` queue;
- does **not** touch the index or any SQLite write path;
- does **not** involve `atomic_write`, content hashes, or conflict detection.

Obsidian itself opens (and may edit) the note — but Obsidian is the user's
editor and the source of truth; that is its job, not Taski's. Taski's TUI still
never opens a vault file directly. The gesture is purely "compose a URL and hand
it to `open(1)`." It is a peer to the existing read-only gestures (movement,
filtering, the context pane), not a peer to the write-back gestures.

### Why two URL modes

The native `obsidian://open?vault=<name>&file=<path>` scheme **opens the file but
cannot target a line** — Obsidian has never added a native `line=` parameter. The
[Advanced URI](https://github.com/Vinzent03/obsidian-advanced-uri) community
plugin (`obsidian://advanced-uri?…&line=<n>`) does support line targeting but
must be installed by the user. Forcing one or the other is wrong: native-only
forfeits the line jump that is the whole point of a per-task gesture; advanced-only
imposes a plugin dependency that silently breaks for users who haven't installed
it. So the mode is **configurable**, with the zero-dependency native scheme as the
default and Advanced URI as an opt-in.

## Decision

### The gesture

`o` (lowercase, in normal mode) on a selected task calls a private helper that:

1. Resolves the configured vault name (see below); if unknown, logs a warning and
   is a no-op.
2. Builds the URL via a pure, unit-tested builder (`obsidian_url`) from
   `task.note_path` + `task.line_number`.
3. Spawns `open <url>` with stdout/stderr redirected to null and **does not wait**
   (fire-and-forget). `open` hands the URL to macOS Launch Services, which focuses
   Obsidian at the target. On spawn failure, logs a `tracing::warn!`.

`o` is bound **only in normal mode** — while a `/`/`F`/`a` prompt is active, `o`
builds the search/modal query instead (per the established `run_loop`
search-state-first dispatch discipline).

`O` (uppercase) remains the Overdue toggle (ADR for `O` is implicit in the Tier 2
views work). The `o`/`O` pairing mirrors Vim's open-line-below/above convention.

### The URL builder (pure, in `taski-tui`)

```rust
fn obsidian_url(vault: &str, note_path: &str, line: usize, advanced: bool) -> String {
    let v = percent_encode_query(vault);
    let f = percent_encode_query(note_path);
    if advanced {
        format!("obsidian://advanced-uri?vault={v}&filepath={f}&line={line}")
    } else {
        format!("obsidian://open?vault={v}&file={f}")
    }
}
```

`percent_encode_query` is a hand-rolled RFC 3986 component encoder (unreserved set
`A-Za-z0-9-._~` kept; all other bytes `%XX`-encoded, UTF-8 for non-ASCII). Critically
this encodes `/`→`%2F` and space→`%20`, as Obsidian's scheme requires for query
values. No `percent-encoding`/`url` crate is pulled in — the encoder is ~10 lines,
fully unit-tested (incl. unicode), and keeps `taski-core` pure (the builder lives
in `taski-tui`, an integration concern, not core domain).

The builder is deliberately **not** placed in `taski-core`: it is Obsidian/TUI
integration glue, not domain logic, and `taski-core`'s purity rule excludes
app-specific concerns (cf. `inbox_line_for`, which *is* domain — a canonical task
line). The builder is still pure and unit-tested inline in `taski-tui`.

### Vault-name resolution

`vault=` takes Obsidian's **registered vault name** (the folder basename Obsidian
shows in its vault switcher). Resolution, done in `run_inner`:

1. If `Config::obsidian_vault` is `Some(name)`, use it verbatim (escape hatch for
   the rare case where the registered name differs from the folder name).
2. Otherwise, resolve the vault path via `taski_config::resolve_vault(None, &cfg)`
   and take its basename (`path.file_name()`).
3. If neither yields a name (e.g. TUI-only mode with no vault configured), the
   gesture is a silent no-op with a `tracing::warn!`.

This requires no daemon or DB change — the TUI already loads config in `run_inner`;
it just reads two more fields.

### New config fields (`taski-config`)

| Field | Type | Default | Purpose |
|---|---|---|---|
| `obsidian_vault` | `Option<String>` | `None` (→ basename of `vault`) | Override the vault name embedded in `obsidian://` URLs. |
| `use_advanced_uri` | `bool` (`#[serde(default)]`) | `false` | When `true`, emit `obsidian://advanced-uri?…&line=<n>` (requires the plugin). |

Both are documented in `template()` output (`--init-config`). No schema change, no
new CLI flag.

### Failure handling (v1)

Best-effort. macOS `open` essentially never fails when Obsidian is installed, and
the vault-name/selection preconditions are checked before spawn. On any failure
the gesture logs `tracing::warn!` and continues. An **in-TUI visible failure
notice** (parallel to the `recent_actions` → `render_failure_notice` path used for
write-back) is deliberately deferred — this gesture is not a `pending_actions`
action and reusing that surfacing machinery would conflate local-OS failures with
daemon-action failures. Listed in context.md's deferred section.

## Alternatives considered

1. **Native-only (no line jump).** Rejected as the sole mode: the whole value of a
   per-task gesture is jumping *to the task*, and "opens the file at the top" forces
   the user to scroll and re-find it. Kept as the safe **default** so the feature
   works with zero plugin dependencies.

2. **Advanced-URI-only (always line jump).** Rejected as the sole mode: it silently
   degrades to "nothing useful happens" for users who haven't installed the plugin,
   and a personal tool should not hard-require a third-party plugin for a basic
   gesture. Kept as an **opt-in** for users who have installed it.

3. **TUI opens the vault file directly via absolute path / an `open <file>` call.**
   Rejected. (a) It would route around Obsidian entirely — the user wants Obsidian's
   editor and tooling, not whatever `.md` handler the OS picks. (b) It edges toward
   the "TUI touches vault files" discipline that ADR-0006 carefully keeps the TUI
   away from (even though `open` on a path doesn't *read* the file, it muddies the
   boundary). The URL scheme is the correct integration point: Obsidian resolves
   the vault and file the same way it resolves `[[wikilinks]]`.

4. **Absolute `path=` parameter (`obsidian://open?path=<abs>`).** Rejected. It
   works, but `vault=` + `file=` is more robust (survives vault relocation, works
   with vault IDs, matches wikilink resolution semantics) and is the documented
   primary form. `path=` also leaks the full filesystem path into a URL.

5. **Make it a daemon-routed action.** Rejected. This gesture has no write-back,
   no conflict surface, and no index consequence — routing it through
   `pending_actions` and the daemon would add latency and machinery for nothing.
   The TUI composing a URL and calling `open` is the simplest correct design.

6. **Surfaces failures in-TUI via the existing notice machinery.** Deferred (see
   Failure handling). Reusing `recent_actions` would couple a local-OS spawn
   outcome to the daemon-action lifecycle; a dedicated local-notice field is
   possible but not worth the render-surface cost for v1 of a personal tool.

## Consequences

- **New gesture, new OS-boundary surface.** This is the TUI's first
  `std::process::Command` spawn and the first read-only gesture that reaches
  outside the process (the daemon reaches outside via FS; the TUI until now did
  not). The spawn is fire-and-forget with null stdio, so it cannot garble the
  alternate screen (cf. the combined-mode "never `eprintln!` from TUI" gotcha —
  `open`'s stdio is explicitly `/dev/null`'d, and `open` does not touch the
  terminal regardless).
- **`tracing` becomes a `taski-tui` dependency.** Previously only `taski-daemon`
  and the `taski` launcher used `tracing`. In combined mode the launcher installs
  a tracing subscriber that routes the daemon thread to `<db_dir>/daemon.log`; the
  TUI thread's `tracing::warn!` events now also flow through that subscriber rather
  than being swallowed. This is strictly better than silent failure.
- **macOS-only for now.** `open(1)` is macOS-specific, matching the existing
  macOS-only `scripts/install-launchd.sh`. Cross-platform support (`xdg-open` on
  Linux, `start` on Windows) is a natural follow-up; Linux additionally requires
  double-encoding of URL parameter values under `xdg-open`, a quirk the current
  single-encoding does not handle. Out of scope for v1.
- **No ADR-0002/0003/0009 amendment.** Because nothing is written, none of the
  write-back gates (grammar-provability, append-only, refuse-on-conflict) apply.
  This ADR opens no new write gate class; it documents a read-side integration.
- **Tests.** Pure `obsidian_url` + `percent_encode_query` unit tests (native vs
  advanced, exact percent-encoding of space/`/`/`#`/`^`/unicode, well-formedness).
  Config tests for both new fields (deserialize, default-absent, template
  round-trip). No proptest — there is no safety invariant to property-test (no
  vault mutation, no conflict surface); the encoder is deterministic and its
  contract is fixed-string equality.
