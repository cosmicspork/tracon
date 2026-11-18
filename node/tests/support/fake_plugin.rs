//! A fake plugin for the supervisor's tests: started as the node starts a
//! plugin (`--socket <path> --data <dir>`), it serves MCP `initialize` on
//! the socket. Its environment is the node's to set, so every knob is a file
//! in its data directory, read at each start:
//!
//! - `mode`: `serve` (the default) answers; `exit` exits 3 at once; `hang`
//!   binds the socket and never answers; `crash` answers, then exits 1 after
//!   200 ms.
//!
//! Each start writes `env.json` (its whole environment), `pid`, `cwd`, and a line
//! to `started` (when, in milliseconds, and the mode it read), and says on
//! stdout and stderr that it started.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (mut socket, mut data) = (None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--data" => data = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    let socket = socket.expect("--socket");
    let data = data.expect("--data");

    let env: serde_json::Map<String, Value> = std::env::vars()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    std::fs::write(data.join("env.json"), Value::Object(env).to_string()).unwrap();
    std::fs::write(data.join("pid"), std::process::id().to_string()).unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::fs::write(data.join("cwd"), cwd.to_string_lossy().as_bytes()).unwrap();
    let mode = std::fs::read_to_string(data.join("mode")).unwrap_or_default();
    let mode = match mode.trim() {
        "" => "serve",
        other => other,
    };
    let mut started = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("started"))
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    writeln!(started, "{now} {mode}").unwrap();
    println!("fake plugin started");
    eprintln!("fake plugin says hello on stderr");

    if mode == "exit" {
        std::process::exit(3);
    }
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    if mode == "hang" {
        let _held = listener;
        return std::future::pending::<()>().await;
    }
    if mode == "crash" {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(1);
        });
    }
    let app = Router::new().route("/", post(mcp)).route("/mcp", post(mcp));
    axum::serve(listener, app).await.unwrap();
}

async fn mcp(Json(request): Json<Value>) -> axum::response::Response {
    match request["method"].as_str() {
        Some("initialize") => Json(json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {},
                "serverInfo": { "name": "fake_plugin", "version": "0.0.0" },
            },
        }))
        .into_response(),
        Some(method) if method.starts_with("notifications/") => {
            StatusCode::ACCEPTED.into_response()
        }
        _ => Json(json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": { "code": -32601, "message": "method not found" },
        }))
        .into_response(),
    }
}
