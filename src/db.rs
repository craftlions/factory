use serde::Serialize;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};

pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Sample {
    pub ts: i64,
    pub cpu_percent: f64,
    pub load1: f64,
    pub mem_used: i64,
    pub mem_total: i64,
    pub disk_used: i64,
    pub disk_total: i64,
    pub process_cpu_percent: f64,
    pub process_rss: i64,
    pub data_files: i64,
    pub data_bytes: i64,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Session {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub note: Option<String>,
    pub isolation: Option<String>,
    pub workdir: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub harness_session_id: Option<String>,
    pub harness_session_file: Option<String>,
}

pub async fn open(path: &Path) -> sqlx::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

pub async fn insert_sample(pool: &SqlitePool, sample: &Sample) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO samples (ts, cpu_percent, load1, mem_used, mem_total, disk_used, disk_total, \
         process_cpu_percent, process_rss, data_files, data_bytes) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sample.ts)
    .bind(sample.cpu_percent)
    .bind(sample.load1)
    .bind(sample.mem_used)
    .bind(sample.mem_total)
    .bind(sample.disk_used)
    .bind(sample.disk_total)
    .bind(sample.process_cpu_percent)
    .bind(sample.process_rss)
    .bind(sample.data_files)
    .bind(sample.data_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn prune_samples(pool: &SqlitePool, now: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM samples WHERE ts < ?")
        .bind(now - RETENTION.as_secs() as i64)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn samples_since(pool: &SqlitePool, since: i64) -> sqlx::Result<Vec<Sample>> {
    sqlx::query_as("SELECT * FROM samples WHERE ts >= ? ORDER BY ts")
        .bind(since)
        .fetch_all(pool)
        .await
}

pub async fn latest_sample(pool: &SqlitePool) -> sqlx::Result<Option<Sample>> {
    sqlx::query_as("SELECT * FROM samples ORDER BY ts DESC LIMIT 1")
        .fetch_optional(pool)
        .await
}

/// Sessions that were still running when the previous process died are closed
/// as interrupted, using the last recorded sample as their end time.
pub async fn close_interrupted_sessions(pool: &SqlitePool, now: i64) -> sqlx::Result<u64> {
    let last_ts: Option<i64> = sqlx::query("SELECT MAX(ts) AS ts FROM samples")
        .fetch_one(pool)
        .await?
        .try_get("ts")?;
    let result = sqlx::query(
        "UPDATE sessions SET status = 'interrupted', ended_at = ? WHERE ended_at IS NULL",
    )
    .bind(last_ts.unwrap_or(now))
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Inserts a running session under a fresh random id.
pub async fn insert_session(
    pool: &SqlitePool,
    request: &crate::sessions::CreateRequest,
    now: i64,
) -> sqlx::Result<String> {
    const ATTEMPTS: usize = 8;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let id = crate::ids::generate();
        let inserted = sqlx::query(
            "INSERT INTO sessions (id, kind, status, started_at, isolation, workdir, provider, model, reasoning) \
             VALUES (?, ?, 'running', ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&request.harness)
        .bind(now)
        .bind(&request.isolation)
        .bind(&request.workdir.kind)
        .bind(&request.model.provider)
        .bind(&request.model.id)
        .bind(&request.model.reasoning)
        .execute(pool)
        .await;
        match inserted {
            Ok(_) => return Ok(id),
            // The id is taken; draw another one.
            Err(sqlx::Error::Database(error))
                if error.is_unique_violation() && attempt < ATTEMPTS => {}
            Err(error) => return Err(error),
        }
    }
}

pub async fn finish_session(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    note: Option<&str>,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE sessions SET status = ?, note = ?, ended_at = ? WHERE id = ?")
        .bind(status)
        .bind(note)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_harness_session(
    pool: &SqlitePool,
    id: &str,
    session_id: &str,
    session_file: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE sessions SET harness_session_id = ?, harness_session_file = ? WHERE id = ?",
    )
    .bind(session_id)
    .bind(session_file)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks an ended session as running again. Returns whether a row changed,
/// so two concurrent resumes cannot both succeed.
pub async fn reopen_session(pool: &SqlitePool, id: &str) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE sessions SET status = 'running', ended_at = NULL, note = NULL \
         WHERE id = ? AND status != 'running'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn session(pool: &SqlitePool, id: &str) -> sqlx::Result<Option<Session>> {
    sqlx::query_as("SELECT * FROM sessions WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn recent_sessions(pool: &SqlitePool, limit: i64) -> sqlx::Result<Vec<Session>> {
    sqlx::query_as("SELECT * FROM sessions ORDER BY started_at DESC, id DESC LIMIT ?")
        .bind(limit)
        .fetch_all(pool)
        .await
}

#[derive(Debug, Serialize, Default)]
pub struct SessionCounts {
    pub running: i64,
    pub completed: i64,
    pub interrupted: i64,
    pub failed: i64,
}

pub async fn session_counts(pool: &SqlitePool) -> sqlx::Result<SessionCounts> {
    let rows = sqlx::query("SELECT status, COUNT(*) AS n FROM sessions GROUP BY status")
        .fetch_all(pool)
        .await?;
    let mut counts = SessionCounts::default();
    for row in rows {
        let status: String = row.try_get("status")?;
        let n: i64 = row.try_get("n")?;
        match status.as_str() {
            "running" => counts.running = n,
            "completed" => counts.completed = n,
            "interrupted" => counts.interrupted = n,
            "failed" => counts.failed = n,
            _ => {}
        }
    }
    Ok(counts)
}
