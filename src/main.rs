use std::{env, path::PathBuf};
use trillium::{Conn, Method};
use trillium_static::{StaticConnExt, StaticFileHandler};

fn main() {
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
    let index = ui_dir.join("index.html");

    trillium_tokio::config()
        .with_host(&host)
        .with_port(port)
        .run((
            |conn: Conn| async move {
                if conn.path() == "/api" || conn.path().starts_with("/api/") {
                    if conn.path() == "/api/health" {
                        if !matches!(conn.method(), Method::Get | Method::Head) {
                            return conn
                                .with_response_header("allow", "GET, HEAD")
                                .with_status(405)
                                .halt();
                        }
                        return conn
                            .with_response_header("content-type", "application/json")
                            .with_response_header("cache-control", "no-store")
                            .ok(r#"{"status":"ok"}"#)
                            .halt();
                    }
                    return conn.with_status(404).halt();
                }
                if !matches!(conn.method(), Method::Get | Method::Head) {
                    return conn
                        .with_response_header("allow", "GET, HEAD")
                        .with_status(405)
                        .halt();
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
        ));
}
