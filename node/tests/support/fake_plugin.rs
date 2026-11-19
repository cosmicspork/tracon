//! A fake plugin for the supervisor's and the brokered tools' tests: started
//! as the node starts a plugin (`--socket <path> --data <dir>`), it serves MCP
//! on the socket. Its environment is the node's to set, so every knob is a
//! file in its data directory, read at each start:
//!
//! - `mode`: `serve` (the default) answers; `exit` exits 3 at once; `hang`
//!   binds the socket and never answers; `crash` answers, then exits 1 after
//!   200 ms.
//! - `tools`: the tools it lists, one name a line; `query` without it.
//!
//! `initialize` starts an MCP session (`mcp-session-id`), and a request
//! naming one it does not know is answered 404, as a server that forgot it
//! answers; a file `forget` in its data directory makes it forget them all
//! at the next request. `tools/call` appends one JSON line to `calls.jsonl`
//! and answers with a text block holding `{tool, arguments, meta, own_instance,
//! node_key}`: `meta` is the `_meta.tracon` it was sent, `own_instance` the
//! sha256 of its own token. A tool named `fail` answers as an error; `sleep`
//! answers after `arguments.ms` milliseconds.
//!
//! Each start writes `env.json` (its whole environment), `pid`, `cwd`, and a line
//! to `started` (when, in milliseconds, and the mode it read), and says on
//! stdout and stderr that it started.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone)]
struct Fake {
    data: PathBuf,
    tools: Arc<Vec<String>>,
    sessions: Arc<Mutex<HashSet<String>>>,
    own_instance: String,
    node_key: String,
}

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
    let tools: Vec<String> = std::fs::read_to_string(data.join("tools"))
        .unwrap_or_else(|_| "query".into())
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let token = std::env::var("TRACON_PLUGIN_TOKEN").unwrap_or_default();
    let fake = Fake {
        data: data.clone(),
        tools: Arc::new(tools),
        sessions: Default::default(),
        own_instance: hex::encode(Sha256::digest(token.as_bytes())),
        node_key: std::env::var("TRACON_NODE_KEY").unwrap_or_default(),
    };
    let app = Router::new()
        .route("/", post(mcp))
        .route("/mcp", post(mcp))
        .with_state(fake);
    axum::serve(listener, app).await.unwrap();
}

fn answer(request: &Value, result: Value) -> axum::response::Response {
    Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result })).into_response()
}

async fn mcp(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> axum::response::Response {
    if std::fs::remove_file(fake.data.join("forget")).is_ok() {
        fake.sessions.lock().unwrap().clear();
    }
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(session) = &session {
        if !fake.sessions.lock().unwrap().contains(session) {
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    match request["method"].as_str() {
        Some("initialize") => {
            let id = hex::encode(rand::random::<[u8; 8]>());
            fake.sessions.lock().unwrap().insert(id.clone());
            let mut response = answer(
                &request,
                json!({
                    "protocolVersion": request["params"]["protocolVersion"],
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake_plugin", "version": "0.0.0" },
                }),
            );
            response
                .headers_mut()
                .insert("mcp-session-id", id.parse().unwrap());
            response
        }
        Some(method) if method.starts_with("notifications/") => {
            StatusCode::ACCEPTED.into_response()
        }
        Some("tools/list") => {
            let tools: Vec<Value> = fake
                .tools
                .iter()
                .map(|name| {
                    json!({
                        "name": name,
                        "description": format!("The fake's {name}."),
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "sql": { "type": "string" },
                                "ms": { "type": "integer" },
                            },
                        },
                    })
                })
                .collect();
            answer(&request, json!({ "tools": tools }))
        }
        Some("tools/call") => {
            let params = &request["params"];
            let tool = params["name"].as_str().unwrap_or_default().to_string();
            let echo = json!({
                "tool": tool,
                "arguments": params.get("arguments").cloned().unwrap_or(Value::Null),
                "meta": params["_meta"]["tracon"],
                "own_instance": fake.own_instance,
                "node_key": fake.node_key,
            });
            {
                let mut calls = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(fake.data.join("calls.jsonl"))
                    .unwrap();
                writeln!(calls, "{echo}").unwrap();
            }
            if tool == "sleep" {
                let ms = params["arguments"]["ms"].as_u64().unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            answer(
                &request,
                json!({
                    "content": [{ "type": "text", "text": echo.to_string() }],
                    "isError": tool == "fail",
                }),
            )
        }
        _ => Json(json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": { "code": -32601, "message": "method not found" },
        }))
        .into_response(),
    }
}
