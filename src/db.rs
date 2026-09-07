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
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub note: Option<String>,
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

pub async fn start_session(
    pool: &SqlitePool,
    kind: &str,
    started_at: i64,
    note: Option<&str>,
) -> sqlx::Result<i64> {
    let result = sqlx::query(
        "INSERT INTO sessions (kind, status, started_at, note) VALUES (?, 'running', ?, ?)",
    )
    .bind(kind)
    .bind(started_at)
    .bind(note)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn end_session(pool: &SqlitePool, id: i64, ended_at: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE sessions SET status = 'completed', ended_at = ? WHERE id = ?")
        .bind(ended_at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
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
            _ => {}
        }
    }
    Ok(counts)
}
