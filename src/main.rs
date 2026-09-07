mod collector;
mod db;

use db::Sample;
use futures_lite::StreamExt;
use serde::Serialize;
use sqlx::SqlitePool;
use std::{env, path::PathBuf, sync::Arc, time::Duration};
use sysinfo::System;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use trillium::{Conn, Method};
use trillium_sse::{Event, sse};
use trillium_static::{StaticConnExt, StaticFileHandler};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY_WINDOW: i64 = 60 * 60;
const MAX_WINDOW: i64 = db::RETENTION.as_secs() as i64;

struct App {
    pool: SqlitePool,
    events: broadcast::Sender<Sample>,
    hostname: String,
    os: String,
}

#[derive(Serialize)]
struct Source {
    name: &'static str,
    status: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct Overview {
    version: &'static str,
    hostname: String,
    os: String,
    uptime_seconds: u64,
    sample_interval_seconds: u64,
    latest: Option<Sample>,
    sessions: db::SessionCounts,
    sources: Vec<Source>,
}

impl App {
    async fn overview(&self) -> sqlx::Result<Overview> {
        let latest = db::latest_sample(&self.pool).await?;
        let sessions = db::session_counts(&self.pool).await?;
        let sources = vec![
            Source {
                name: "host",
                status: "active",
                detail: "CPU, memory, load, and disk sampled with sysinfo".into(),
            },
            Source {
                name: "process",
                status: "active",
                detail: "CPU and resident memory of the factory service".into(),
            },
            Source {
                name: "files",
                status: "active",
                detail: "Files in the factory data directory".into(),
            },
            Source {
                name: "microvm",
                status: "planned",
                detail: "vsock guest reporting is not implemented yet".into(),
            },
        ];
        Ok(Overview {
            version: VERSION,
            hostname: self.hostname.clone(),
            os: self.os.clone(),
            uptime_seconds: System::uptime(),
            sample_interval_seconds: collector::INTERVAL.as_secs(),
            latest,
            sessions,
            sources,
        })
    }
}

fn json(conn: Conn, value: &impl Serialize) -> Conn {
    match serde_json::to_string(value) {
        Ok(body) => conn
            .with_response_header("content-type", "application/json")
            .with_response_header("cache-control", "no-store")
            .ok(body),
        Err(error) => {
            eprintln!("failed to serialise response: {error}");
            conn.with_status(500)
        }
    }
}

fn db_error(conn: Conn, error: sqlx::Error) -> Conn {
    eprintln!("database error: {error}");
    conn.with_status(500)
}

fn query_param<'a>(conn: &'a Conn, key: &str) -> Option<&'a str> {
    conn.querystring()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

async fn api(conn: Conn, app: Arc<App>) -> Conn {
    if !matches!(conn.method(), Method::Get | Method::Head) {
        return conn
            .with_response_header("allow", "GET, HEAD")
            .with_status(405)
            .halt();
    }
    match conn.path() {
        "/api/health" => conn
            .with_response_header("content-type", "application/json")
            .with_response_header("cache-control", "no-store")
            .ok(r#"{"status":"ok"}"#)
            .halt(),
        "/api/overview" => match app.overview().await {
            Ok(overview) => json(conn, &overview).halt(),
            Err(error) => db_error(conn, error).halt(),
        },
        "/api/samples" => {
            let window = query_param(&conn, "window")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(HISTORY_WINDOW)
                .clamp(1, MAX_WINDOW);
            match db::samples_since(&app.pool, collector::now() - window).await {
                Ok(samples) => json(conn, &samples).halt(),
                Err(error) => db_error(conn, error).halt(),
            }
        }
        "/api/sessions" => match db::recent_sessions(&app.pool, 50).await {
            Ok(sessions) => json(conn, &sessions).halt(),
            Err(error) => db_error(conn, error).halt(),
        },
        // Falls through to the SSE handler, which answers only when the client
        // asks for text/event-stream.
        "/api/events" => conn,
        _ => conn.with_status(404).halt(),
    }
}

fn sample_events(
    events: &broadcast::Sender<Sample>,
) -> impl futures_lite::Stream<Item = Event> + Unpin + Send + 'static + use<> {
    BroadcastStream::new(events.subscribe()).filter_map(|item| {
        let sample = item.ok()?;
        let data = serde_json::to_string(&sample).ok()?;
        Some(Event::new(data).with_type("sample"))
    })
}

#[tokio::main]
async fn main() {
    let host = env::var("FACTORY_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port = env::var("FACTORY_PORT")
        .map(|value| {
            value
                .parse::<u16>()
                .expect("FACTORY_PORT must be a valid port")
        })
        .unwrap_or(80);
    let ui_dir = PathBuf::from(
        env::var_os("FACTORY_UI_DIR").unwrap_or_else(|| "/usr/share/craftlions-factory/ui".into()),
    );
    let data_dir = PathBuf::from(
        env::var_os("FACTORY_DATA_DIR").unwrap_or_else(|| "/var/lib/craftlions-factory".into()),
    );
    std::fs::create_dir_all(&data_dir).expect("FACTORY_DATA_DIR must be writable");
    let index = ui_dir.join("index.html");

    let pool = db::open(&data_dir.join("factory.db"))
        .await
        .expect("database must open");
    let started_at = collector::now();
    let interrupted = db::close_interrupted_sessions(&pool, started_at)
        .await
        .expect("sessions must be readable");
    if interrupted > 0 {
        eprintln!("closed {interrupted} interrupted session(s)");
    }
    let session_id = db::start_session(
        &pool,
        "service",
        started_at,
        Some(&format!("craftlions-factory {VERSION}")),
    )
    .await
    .expect("session must be recorded");

    let (events, _) = broadcast::channel(16);
    tokio::spawn(collector::run(pool.clone(), data_dir, events.clone()));

    let app = Arc::new(App {
        pool: pool.clone(),
        events,
        hostname: System::host_name().unwrap_or_else(|| "unknown".into()),
        os: System::long_os_version().unwrap_or_else(|| "unknown".into()),
    });
    let api_app = app.clone();
    let sse_events = app.events.clone();

    trillium_tokio::config()
        .with_host(&host)
        .with_port(port)
        .run_async((
            move |conn: Conn| {
                let app = api_app.clone();
                async move {
                    if conn.path() == "/api" || conn.path().starts_with("/api/") {
                        return api(conn, app).await;
                    }
                    if !matches!(conn.method(), Method::Get | Method::Head) {
                        return conn
                            .with_response_header("allow", "GET, HEAD")
                            .with_status(405)
                            .halt();
                    }
                    conn
                }
            },
            sse(move |_: &mut Conn| sample_events(&sse_events))
                .with_heartbeat(Duration::from_secs(15)),
            |conn: Conn| async move {
                // /api/events without an event-stream Accept header reaches here.
                if conn.path() == "/api/events" {
                    return conn.with_status(406).halt();
                }
                conn
            },
            StaticFileHandler::new(ui_dir).with_index_file("index.html"),
            move |conn: Conn| {
                let index = index.clone();
                async move {
                    // Only browser navigation receives the SPA shell. Missing assets
                    // and non-GET requests must remain errors.
                    if matches!(conn.method(), Method::Get | Method::Head)
                        && !conn.path().starts_with("/assets/")
                        && !conn.path().rsplit('/').next().unwrap_or("").contains('.')
                        && conn
                            .request_headers()
                            .get_str("accept")
                            .is_some_and(|accept| accept.contains("text/html"))
                    {
                        return conn.send_path(index).await;
                    }
                    conn.with_status(404)
                }
            },
        ))
        .await;

    if let Err(error) = db::end_session(&pool, session_id, collector::now()).await {
        eprintln!("failed to close session: {error}");
    }
    pool.close().await;
}
