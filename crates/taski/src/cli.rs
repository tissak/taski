//! Scriptable CLI (ADR-0027): `taski list|show|done|…` for AI agents and shell use.
//!
//! A third client of the index, exactly like the TUI: reads via `db::all_tasks`, writes
//! by enqueuing `pending_actions` rows. It never opens a vault file itself — the daemon
//! code path (`process_pending_actions` → `atomic_write`) stays the sole writer. If no
//! daemon holds the single-writer lock, the CLI takes it (ADR-0008) and drains the
//! queue in-process, so writes work with or without a running daemon.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use rusqlite::Connection;
use taski_db::{self as db, PendingAction, Status, Task};

use taski_daemon::{
    LockOutcome, acquire_daemon_lock, daemon_lock_path, index_note, local_today,
    process_pending_actions,
};

/// How long a write waits for a running daemon to resolve it (its tick is ≤500ms).
const WAIT: Duration = Duration::from_secs(10);

#[derive(Args)]
pub struct ListArgs {
    /// Which statuses to include. `open` = open, in-progress and blocked.
    #[arg(long, value_enum, default_value_t = StatusArg::Open)]
    status: StatusArg,
    /// Only tasks scheduled or due today.
    #[arg(long)]
    today: bool,
    /// Only tasks due or scheduled before today.
    #[arg(long)]
    overdue: bool,
    /// Case-insensitive substring match on task text.
    #[arg(long)]
    search: Option<String>,
    /// Case-insensitive substring match on the note path.
    #[arg(long)]
    file: Option<String>,
    /// Only tasks carrying this tag (with or without the leading `#`).
    #[arg(long)]
    tag: Option<String>,
    /// Emit a JSON array instead of one line per task.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Open,
    Done,
    Cancelled,
    All,
}

/// The checkbox state a status command targets.
#[derive(Clone, Copy)]
pub enum Target {
    Done,
    Open,
    Start,
    Block,
    Cancel,
}

impl Target {
    fn char(self) -> &'static str {
        match self {
            Target::Done => "x",
            Target::Open => " ",
            Target::Start => "/",
            Target::Block => "!",
            Target::Cancel => "-",
        }
    }
}

/// Shared context: resolved paths from flags → config → defaults.
pub struct Ctx {
    db: PathBuf,
    vault: Option<PathBuf>,
    inbox: String,
}

impl Ctx {
    pub fn load(vault: Option<&Path>, db_flag: Option<&Path>) -> Result<Self> {
        let cfg = taski_config::load().context("loading taski config")?;
        Ok(Ctx {
            db: taski_config::resolve_db(db_flag.and_then(Path::to_str), &cfg),
            vault: taski_config::resolve_vault(vault.and_then(Path::to_str), &cfg).ok(),
            inbox: taski_config::resolve_inbox_path(&cfg),
        })
    }

    fn conn(&self) -> Result<Connection> {
        db::open(&self.db.to_string_lossy()).with_context(|| format!("opening {:?}", self.db))
    }

    fn task(&self, conn: &Connection, id: i64) -> Result<Task> {
        db::all_tasks(conn)?
            .into_iter()
            .find(|t| t.id == id)
            .with_context(|| format!("no task with id {id} (run `taski list` for current ids)"))
    }
}

pub fn list(ctx: &Ctx, a: &ListArgs) -> Result<()> {
    let today = local_today();
    let tag = a.tag.as_deref().map(|t| t.trim_start_matches('#'));
    let search = a.search.as_deref().map(str::to_lowercase);
    let file = a.file.as_deref().map(str::to_lowercase);
    let tasks: Vec<Task> = db::all_tasks(&ctx.conn()?)?
        .into_iter()
        .filter(|t| match a.status {
            StatusArg::Open => t.status.is_open_like(),
            StatusArg::Done => t.status == Status::Done,
            StatusArg::Cancelled => t.raw_checkbox_char == "-",
            StatusArg::All => true,
        })
        .filter(|t| !a.today || t.is_today(&today))
        .filter(|t| !a.overdue || t.is_overdue(&today))
        .filter(|t| {
            search
                .as_ref()
                .is_none_or(|s| t.text.to_lowercase().contains(s))
        })
        .filter(|t| {
            file.as_ref()
                .is_none_or(|f| t.note_path.to_lowercase().contains(f))
        })
        .filter(|t| tag.is_none_or(|g| t.tags.iter().any(|x| x == g)))
        .collect();

    if a.json {
        let items: Vec<String> = tasks.iter().map(|t| task_json(t, ctx)).collect();
        println!("[{}]", items.join(",\n "));
    } else {
        for t in &tasks {
            println!("{}", task_line(t));
        }
    }
    Ok(())
}

pub fn show(ctx: &Ctx, id: i64, context: usize, json: bool) -> Result<()> {
    let conn = ctx.conn()?;
    let t = ctx.task(&conn, id)?;
    let content = db::note_content(&conn, &t.note_path)?.map(|n| n.content);
    let lines: Vec<(usize, &str)> = content
        .as_deref()
        .unwrap_or("")
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .filter(|(n, _)| n.abs_diff(t.line_number) <= context)
        .collect();

    if json {
        let ctx_json: Vec<String> = lines
            .iter()
            .map(|(n, l)| format!("{{\"line\":{n},\"text\":{}}}", json_str(l)))
            .collect();
        println!(
            "{{\"task\":{},\"context\":[{}]}}",
            task_json(&t, ctx),
            ctx_json.join(",")
        );
    } else {
        println!("{}", task_line(&t));
        if let Some(p) = abs_path(&t, ctx) {
            println!("{p}");
        }
        for (n, l) in lines {
            let mark = if n == t.line_number { ">" } else { " " };
            println!("{mark}{n:>5} | {l}");
        }
    }
    Ok(())
}

pub fn set_status(ctx: &Ctx, id: i64, target: Target) -> Result<()> {
    let task = ctx.task(&ctx.conn()?, id)?;
    if task.raw_checkbox_char == target.char() {
        println!("unchanged: #{id} is already [{}]", target.char());
        return Ok(());
    }
    submit(ctx, Some(&task), |conn| {
        Ok(db::enqueue_action(
            conn,
            task.id,
            &task.note_path,
            task.line_number,
            &task.raw_checkbox_char,
            target.char(),
        )?)
    })
}

pub fn schedule(ctx: &Ctx, id: i64, date: &str) -> Result<()> {
    let desired = match date {
        "none" | "clear" => None,
        "today" => Some(local_today()),
        d if is_iso_date(d) => Some(d.to_string()),
        d => bail!("invalid date {d:?}: expected YYYY-MM-DD, `today`, or `none`"),
    };
    let task = ctx.task(&ctx.conn()?, id)?;
    submit(ctx, Some(&task), |conn| {
        Ok(db::enqueue_set_scheduled(
            conn,
            task.id,
            &task.note_path,
            task.line_number,
            desired.as_deref(),
        )?)
    })
}

pub fn add(ctx: &Ctx, text: &str) -> Result<()> {
    let text = single_line(text)?;
    submit(ctx, None, |conn| {
        Ok(db::enqueue_quick_add(conn, &ctx.inbox, text)?)
    })
}

pub fn note(ctx: &Ctx, id: i64, text: &str) -> Result<()> {
    let text = single_line(text)?;
    let task = ctx.task(&ctx.conn()?, id)?;
    submit(ctx, Some(&task), |conn| {
        Ok(db::enqueue_add_note(
            conn,
            task.id,
            &task.note_path,
            task.line_number,
            text,
        )?)
    })
}

/// Enqueue one action and report how it resolved. With a daemon running we just wait
/// for it. Without one we take the single-writer lock (ADR-0008) and act as the daemon
/// for this one drain: re-index the target note first (nobody else is keeping the index
/// fresh), then run the same `process_pending_actions` the daemon runs.
///
/// On success, prints the resulting task line. Its id may differ from the one passed
/// in: reconciliation keys on the task text's hash (ADR-0005), and the write just
/// changed the text (`✅`/`⏳` stamps, a notes link), so the agent needs the new id.
fn submit(
    ctx: &Ctx,
    target: Option<&Task>,
    enqueue: impl FnOnce(&Connection) -> Result<i64>,
) -> Result<()> {
    let conn = ctx.conn()?;
    // The task's line before the write (re-read after any re-index, which may move it).
    let line_now = |conn: &Connection| {
        target
            .and_then(|t| ctx.task(conn, t.id).ok())
            .map(|t| t.line_number)
    };
    let lock = daemon_lock_path(&ctx.db);
    let (id, line) = match acquire_daemon_lock(&lock).context("probing daemon lock")? {
        LockOutcome::Acquired(_guard) => {
            let vault = ctx.vault.as_deref().context(
                "no daemon running and no vault configured; set `vault` or pass --vault",
            )?;
            if let Some(t) = target {
                index_note(&conn, &vault.join(&t.note_path), vault).context("re-indexing note")?;
            }
            let line = line_now(&conn);
            let id = enqueue(&conn)?;
            process_pending_actions(&conn, vault)?;
            (id, line)
        }
        LockOutcome::HeldByOther(_) => (enqueue(&conn)?, line_now(&conn)),
    };
    let action = wait_for(&conn, id)?;
    report(&action)?;
    // Same note, same line — or, for quick-add (no target), the inbox's last task.
    let after = db::all_tasks(&conn)?
        .into_iter()
        .rfind(|t| t.note_path == action.note_path && line.is_none_or(|l| t.line_number == l));
    match after {
        Some(t) => println!("{}", task_line(&t)),
        None => println!("(task not found in index yet; run `taski list`)"),
    }
    Ok(())
}

fn wait_for(conn: &Connection, id: i64) -> Result<PendingAction> {
    let start = Instant::now();
    loop {
        let a = db::action(conn, id)?.with_context(|| format!("action {id} vanished"))?;
        if a.state != "pending" || start.elapsed() > WAIT {
            return Ok(a);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn report(a: &PendingAction) -> Result<()> {
    match a.state.as_str() {
        "done" => Ok(()),
        "failed" => bail!(
            "refused: {}",
            a.error.as_deref().unwrap_or("unknown reason")
        ),
        _ => bail!(
            "action {} still pending after {}s; it will apply when the daemon gets to it. Do not retry.",
            a.id,
            WAIT.as_secs()
        ),
    }
}

fn single_line(text: &str) -> Result<&str> {
    let text = text.trim();
    if text.is_empty() {
        bail!("text is empty");
    }
    if text.contains(['\n', '\r']) {
        bail!("text must be a single line");
    }
    Ok(text)
}

fn is_iso_date(d: &str) -> bool {
    let b = d.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && (1..=12).contains(&d[5..7].parse::<u8>().unwrap_or(0))
        && (1..=31).contains(&d[8..10].parse::<u8>().unwrap_or(0))
}

fn status_name(raw: &str) -> &'static str {
    match raw {
        " " => "open",
        "x" | "X" => "done",
        "/" => "in_progress",
        "!" => "blocked",
        "-" => "cancelled",
        _ => "other",
    }
}

fn abs_path(t: &Task, ctx: &Ctx) -> Option<String> {
    ctx.vault
        .as_ref()
        .map(|v| v.join(&t.note_path).display().to_string())
}

fn task_line(t: &Task) -> String {
    format!(
        "{}\t[{}] {}\t{}:{}",
        t.id, t.raw_checkbox_char, t.text, t.note_path, t.line_number
    )
}

fn task_json(t: &Task, ctx: &Ctx) -> String {
    let opt = |v: Option<&str>| v.map_or("null".to_string(), json_str);
    let tags: Vec<String> = t.tags.iter().map(|s| json_str(s)).collect();
    format!(
        "{{\"id\":{},\"status\":{},\"checkbox\":{},\"text\":{},\"note_path\":{},\"path\":{},\"line\":{},\"due\":{},\"scheduled\":{},\"start\":{},\"created\":{},\"done\":{},\"cancelled\":{},\"priority\":{},\"tags\":[{}]}}",
        t.id,
        json_str(status_name(&t.raw_checkbox_char)),
        json_str(&t.raw_checkbox_char),
        json_str(&t.text),
        json_str(&t.note_path),
        opt(abs_path(t, ctx).as_deref()),
        t.line_number,
        opt(t.due_date.as_deref()),
        opt(t.scheduled_date.as_deref()),
        opt(t.start_date.as_deref()),
        opt(t.created_date.as_deref()),
        opt(t.done_date.as_deref()),
        opt(t.cancelled_date.as_deref()),
        opt(t.priority.as_ref().and_then(|p| p.to_emoji())),
        tags.join(","),
    )
}

/// Minimal JSON string encoder — keeps `serde_json` out of the dep tree for one use.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_str_escapes() {
        assert_eq!(json_str("a\"b\\c\nd\u{1}é"), r#""a\"b\\c\nd\u0001é""#);
    }

    #[test]
    fn iso_date_validation() {
        assert!(is_iso_date("2026-10-07"));
        for bad in [
            "2026-13-01",
            "2026-1-01",
            "20261007xx",
            "2026-10-00",
            "today",
        ] {
            assert!(!is_iso_date(bad), "{bad}");
        }
    }
}
