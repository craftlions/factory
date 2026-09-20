mod collector;
mod db;
mod harness;
mod ids;
mod sessions;

use db::Sample;
use futures_lite::StreamExt;
use serde::Serialize;
use sessions::{CreateError, ResumeError, Sessions, Snapshot};
use sqlx::SqlitePool;
use std::{env, path::PathBuf, pin::Pin, sync::Arc, time::Duration};
use sysinfo::System;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use trillium::{Conn, Method};
use trillium_sse::{Event, Sse, SseHandler};
use trillium_static::{StaticConnExt, StaticFileHandler};
use trillium_tokio::Swansong;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY_WINDOW: i64 = 60 * 60;
const MAX_WINDOW: i64 = db::RETENTION.as_secs() as i64;

struct App {
    pool: SqlitePool,
    events: broadcast::Sender<Sample>,
    sessions: Arc<Sessions>,
    hostname: String,
    os: String,
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
}

impl App {
    async fn overview(&self) -> sqlx::Result<Overview> {
        let latest = db::latest_sample(&self.pool).await?;
        let sessions = db::session_counts(&self.pool).await?;
        Ok(Overview {
            version: VERSION,
            hostname: self.hostname.clone(),
            os: self.os.clone(),
            uptime_seconds: System::uptime(),
            sample_interval_seconds: collector::INTERVAL.as_secs(),
            latest,
            sessions,
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

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| value.get(i + 1..i + 3))
            .flatten()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(conn: &Conn, key: &str) -> Option<String> {
    conn.querystring()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}

fn error(conn: Conn, status: u16, message: impl std::fmt::Display) -> Conn {
    json(conn, &serde_json::json!({ "error": message.to_string() })).with_status(status)
}

/// `/api/sessions/<id>/events` names a session's chat stream.
fn session_events_id(path: &str) -> Option<&str> {
    path.strip_prefix("/api/sessions/")?
        .strip_suffix("/events")
        .filter(|id| ids::is_valid(id))
}

/// Reads a JSON body. Requiring the JSON content type makes browsers preflight
/// cross-origin requests, which this server never approves, so other websites
/// cannot start or drive sessions.
async fn json_body<T: serde::de::DeserializeOwned>(conn: &mut Conn) -> Result<T, String> {
    let is_json = conn
        .request_headers()
        .get_str("content-type")
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_json {
        return Err("content-type must be application/json".into());
    }
    let body = conn
        .request_body_string()
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

#[derive(serde::Deserialize)]
struct PromptRequest {
    message: String,
}

async fn api(mut conn: Conn, app: Arc<App>) -> Conn {
    let path = conn.path().to_owned();
    let segments: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
    let method = conn.method();
    let read = matches!(method, Method::Get | Method::Head);

    match (read, method == Method::Post, segments.as_slice()) {
        (true, _, ["health"]) => json(conn, &serde_json::json!({ "status": "ok" })).halt(),
        (true, _, ["overview"]) => match app.overview().await {
            Ok(overview) => json(conn, &overview).halt(),
            Err(error) => db_error(conn, error).halt(),
        },
        (true, _, ["samples"]) => {
            let window = query_param(&conn, "window")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(HISTORY_WINDOW)
                .clamp(1, MAX_WINDOW);
            match db::samples_since(&app.pool, collector::now() - window).await {
                Ok(samples) => json(conn, &samples).halt(),
                Err(error) => db_error(conn, error).halt(),
            }
        }
        // Falls through to the SSE handler, which answers only when the client
        // asks for text/event-stream.
        (true, _, ["events"]) => conn,

        (true, _, ["sessions"]) => match db::recent_sessions(&app.pool, 50).await {
            Ok(sessions) => json(conn, &sessions).halt(),
            Err(error) => db_error(conn, error).halt(),
        },
        (_, true, ["sessions"]) => {
            let request = match json_body(&mut conn).await {
                Ok(request) => request,
                Err(message) => return error(conn, 400, message).halt(),
            };
            match app.sessions.create(request).await {
                Ok(id) => json(conn, &serde_json::json!({ "id": id }))
                    .with_status(201)
                    .halt(),
                Err(CreateError::Unsupported(message)) => error(conn, 501, message).halt(),
                Err(CreateError::Invalid(message)) => error(conn, 422, message).halt(),
                Err(CreateError::Launch(cause)) => error(conn, 502, cause).halt(),
                Err(CreateError::Database(cause)) => db_error(conn, cause).halt(),
            }
        }
        (true, _, ["sessions", id]) | (true, _, ["sessions", id, "events"]) => {
            // Ids become directory names, so anything malformed stops here.
            let found = match ids::is_valid(id) {
                true => db::session(&app.pool, id).await,
                false => Ok(None),
            };
            match found {
                // The chat stream itself is produced by the SSE handler.
                Ok(Some(_)) if segments.len() == 3 => conn,
                Ok(Some(session)) => json(conn, &session).halt(),
                Ok(None) => error(conn, 404, "no such session").halt(),
                Err(cause) => db_error(conn, cause).halt(),
            }
        }
        (_, true, ["sessions", id, "resume"]) => {
            let resumed = match ids::is_valid(id) {
                true => app.sessions.resume(id).await,
                false => Err(ResumeError::NotFound),
            };
            match resumed {
                Ok(()) => conn.with_status(202).halt(),
                Err(ResumeError::NotFound) => error(conn, 404, "no such session").halt(),
                Err(ResumeError::Conflict(message)) => error(conn, 409, message).halt(),
                Err(ResumeError::Launch(cause)) => error(conn, 502, cause).halt(),
                Err(ResumeError::Database(cause)) => db_error(conn, cause).halt(),
            }
        }
        (_, true, ["sessions", id, action @ ("prompt" | "abort" | "stop")]) => {
            let command = match *action {
                "prompt" => match json_body::<PromptRequest>(&mut conn).await {
                    Ok(body) if !body.message.trim().is_empty() => {
                        harness::Command::Prompt(body.message)
                    }
                    Ok(_) => return error(conn, 422, "message must not be empty").halt(),
                    Err(message) => return error(conn, 400, message).halt(),
                },
                "abort" => harness::Command::Abort,
                _ => harness::Command::Stop,
            };
            if app.sessions.send(id, command) {
                conn.with_status(202).halt()
            } else {
                error(conn, 409, "the session is not running").halt()
            }
        }

        (true, _, ["harnesses", id, "models"]) => match harness::models(id).await {
            Some(Ok(models)) => json(conn, &models).halt(),
            Some(Err(cause)) => error(conn, 502, cause).halt(),
            None => error(conn, 404, "no catalog for this harness").halt(),
        },
        (true, _, ["harnesses", id, "reasoning"]) => {
            let (Some(provider), Some(model)) =
                (query_param(&conn, "provider"), query_param(&conn, "model"))
            else {
                return error(conn, 422, "provider and model are required").halt();
            };
            match harness::reasoning_levels(id, &provider, &model).await {
                Some(Ok(levels)) => json(conn, &levels).halt(),
                Some(Err(cause)) => error(conn, 502, cause).halt(),
                None => error(conn, 404, "no catalog for this harness").halt(),
            }
        }

        (false, false, _) => conn
            .with_response_header("allow", "GET, HEAD, POST")
            .with_status(405)
            .halt(),
        _ => conn.with_status(404).halt(),
    }
}

type EventStream = Pin<Box<dyn futures_lite::Stream<Item = Event> + Send>>;

fn sample_events(events: &broadcast::Sender<Sample>) -> EventStream {
    Box::pin(BroadcastStream::new(events.subscribe()).filter_map(|item| {
        let sample = item.ok()?;
        let data = serde_json::to_string(&sample).ok()?;
        Some(Event::new(data).with_type("sample"))
    }))
}

#[derive(Serialize)]
struct SnapshotEvent<'a> {
    r#type: &'static str,
    #[serde(flatten)]
    snapshot: &'a Snapshot,
}

fn chat_event(value: &impl Serialize) -> Option<Event> {
    Some(Event::new(serde_json::to_string(value).ok()?).with_type("chat"))
}

/// Serves `/api/events` and `/api/sessions/<id>/events`. `api` has already
/// rejected every other path and unknown sessions.
struct Streams {
    samples: broadcast::Sender<Sample>,
    sessions: Arc<Sessions>,
    /// Event streams never end by themselves. Graceful shutdown waits for open
    /// connections, so without this a single browser tab keeps a stopping
    /// server alive and holding its port while its replacement fails to bind.
    shutdown: Swansong,
}

impl SseHandler for Streams {
    type Event = Event;
    type EventStream = EventStream;

    async fn connect(&self, conn: &mut Conn) -> EventStream {
        // Ends the stream as soon as shutdown begins.
        Box::pin(self.shutdown.interrupt(self.events(conn).await))
    }
}

impl Streams {
    async fn events(&self, conn: &Conn) -> EventStream {
        let Some(id) = session_events_id(conn.path()) else {
            return sample_events(&self.samples);
        };
        let (snapshot, live) = match self.sessions.open(id).await {
            Ok(Some(opened)) => opened,
            Ok(None) => return Box::pin(futures_lite::stream::empty()),
            Err(error) => {
                eprintln!("failed to open session {id}: {error}");
                return Box::pin(futures_lite::stream::empty());
            }
        };
        let first = chat_event(&SnapshotEvent {
            r#type: "snapshot",
            snapshot: &snapshot,
        });
        let first = futures_lite::stream::iter(first);
        match live {
            // A client that falls too far behind is cut off here; it then
            // reconnects and starts over from a fresh snapshot.
            Some(events) => {
                Box::pin(first.chain(
                    BroadcastStream::new(events).scan((), |(), item| chat_event(&item.ok()?)),
                ))
            }
            None => Box::pin(first),
        }
    }
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
    // Harness processes get these paths as their working directory.
    let data_dir = data_dir
        .canonicalize()
        .expect("FACTORY_DATA_DIR must resolve");
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
    let (events, _) = broadcast::channel(16);
    let sessions = Arc::new(Sessions::new(pool.clone(), &data_dir));
    tokio::spawn(collector::run(pool.clone(), data_dir, events.clone()));

    let app = Arc::new(App {
        pool: pool.clone(),
        events,
        sessions: sessions.clone(),
        hostname: System::host_name().unwrap_or_else(|| "unknown".into()),
        os: System::long_os_version().unwrap_or_else(|| "unknown".into()),
    });
    let api_app = app.clone();
    let shutdown = Swansong::new();
    let streams = Streams {
        samples: app.events.clone(),
        sessions,
        shutdown: shutdown.clone(),
    };

    trillium_tokio::config()
        .with_swansong(shutdown)
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
            Sse::new(streams).with_heartbeat(Duration::from_secs(15)),
            |conn: Conn| async move {
                // Event routes without an event-stream Accept header reach here.
                if conn.path() == "/api/events" || session_events_id(conn.path()).is_some() {
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

    pool.close().await;
}
