use trillium::Conn;

const INDEX: &str = include_str!("./index.html");

fn main() {
    trillium_tokio::config()
        .with_host("0.0.0.0")
        .with_port(80)
        .run(|conn: Conn| async move {
            conn.with_response_header("content-type", "text/html; charset=utf-8")
                .ok(INDEX)
        });
}