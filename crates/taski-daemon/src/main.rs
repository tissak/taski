//! taski-daemon — Slice 0 walking skeleton.
//!
//! One-shot ingest: parses a hardcoded sample note, upserts every recognised task into
//! the shared SQLite index at `./taski.db`, prints `ingested N tasks`, and exits 0.
//!
//! Filesystem watching, reconciliation, and write-back are deferred to later slices.

use anyhow::Context;
use taski_core::parse_tasks;
use taski_db as db;

/// Fixed database path for Slice 0. The TUI reads from the same file, proving the
/// multi-process WAL handoff.
const DB_PATH: &str = "./taski.db";

/// Hardcoded sample note used to exercise the Slice 0 vertical slice. The fourth
/// checkbox lives inside a fenced code block and must be ignored by the parser.
const SAMPLE_NOTE: &str = "\
# 2026-06-20 — Daily

- [ ] Review PR #42
- [x] Reply to Ana about the deploy
- [/] Draft the weekly retro notes

```markdown
- [ ] this is not a real task (inside a fence)
```

Some plain prose here.
";

fn main() -> anyhow::Result<()> {
    let conn = db::open(DB_PATH).context("opening taski database")?;

    let tasks = parse_tasks(SAMPLE_NOTE, "2026-06-20.md");
    for task in &tasks {
        db::upsert_task(&conn, task).context("upserting task")?;
    }

    println!("ingested {} tasks", tasks.len());
    Ok(())
}
