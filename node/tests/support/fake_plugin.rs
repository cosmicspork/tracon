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
//! - `node`: calls the node back on the socket it was given with
//!   `--node-socket` (written to `node_socket` at start): `arguments.method`
//!   with `arguments.params`, under its own token or `arguments.token`, and
//!   with the `_meta.tracon` it was sent added as `params.on_behalf` when
//!   `arguments.on_behalf` is true. It answers with `{status, body}`: the
//!   HTTP status and the node's JSON-RPC response.
//!
//! Previews and formats, read at each request rather than each start:
//!
//! - `preview`: absent, `tracon/preview` answers `-32601`. `on` answers a
//!   preview whose `token` is the contents of `version` (`v1` without it);
//!   `refuse` answers error `-32000 cannot preview`; `unpreviewable` answers
//!   `previewable: false` and no token. Each preview appends its
//!   `_meta.tracon` to `previews.jsonl`.
//! - `version`: what a preview is taken against. A `tools/call` whose
//!   context carries a `preview_token` other than it is refused as stale
//!   (`data.stale`), and recorded in `calls.jsonl` with `"refused": "stale"`.
//! - `marked`: `sql` carries `"x-tracon-format": "fake_wiki"` in the listing.
//!
//! `tracon/format/convert` converts `fake_wiki` (`h1. X` is `# X`, `*b*` is
//! `**b**`) to `markdown` and back; anything else is `-32602`.
//!
//! - `/api/...`, any method: a plugin UI's API (`tracon::http::plugin_ui`).
//!   It appends one JSON line to `ui.jsonl` and answers with
//!   `{method, uri, headers, context, body, own_instance, node_key}`, where
//!   `context` is the `tracon-context` header decoded. It also sends a
//!   `set-cookie`, an `x-extra` and a `content-disposition` header, so a test
//!   can see which of them reach the page. `/api/fail` answers 500.
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
    token: String,
    node_socket: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (mut socket, mut data, mut node_socket) = (None, None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--data" => data = args.next().map(PathBuf::from),
            "--node-socket" => node_socket = args.next().map(PathBuf::from),
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
    match &node_socket {
        Some(path) => std::fs::write(data.join("node_socket"), path.to_string_lossy().as_bytes()),
        None => std::fs::remove_file(data.join("node_socket")).or(Ok(())),
    }
    .unwrap();
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
        token,
        node_socket,
    };
    let app = Router::new()
        .route("/", post(mcp))
        .route("/mcp", post(mcp))
        .route("/api/{*rest}", axum::routing::any(api))
        .with_state(fake);
    axum::serve(listener, app).await.unwrap();
}

async fn api(
    State(fake): State<Fake>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use base64::Engine;
    let context = headers
        .get("tracon-context")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(v)
                .ok()
        })
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    let seen: serde_json::Map<String, Value> = headers
        .iter()
        .filter(|(k, _)| k.as_str() != "tracon-context")
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                Value::String(v.to_str().unwrap_or_default().to_string()),
            )
        })
        .collect();
    let echo = json!({
        "method": method.as_str(),
        "uri": uri.to_string(),
        "headers": seen,
        "context": context,
        "body": String::from_utf8_lossy(&body),
        "own_instance": fake.own_instance,
        "node_key": fake.node_key,
    });
    {
        let mut calls = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(fake.data.join("ui.jsonl"))
            .unwrap();
        writeln!(calls, "{echo}").unwrap();
    }
    let status = match uri.path() {
        "/api/fail" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::OK,
    };
    (
        status,
        [
            ("content-type", "application/json"),
            ("set-cookie", "sid=plugin; Path=/"),
            ("x-extra", "dropped"),
            ("content-disposition", "inline; filename=\"echo.json\""),
        ],
        echo.to_string(),
    )
        .into_response()
}

fn answer(request: &Value, result: Value) -> axum::response::Response {
    Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result })).into_response()
}

fn error(request: &Value, error: Value) -> axum::response::Response {
    Json(json!({ "jsonrpc": "2.0", "id": request["id"], "error": error })).into_response()
}

fn knob(fake: &Fake, name: &str) -> Option<String> {
    std::fs::read_to_string(fake.data.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

fn version(fake: &Fake) -> String {
    knob(fake, "version").unwrap_or_else(|| "v1".into())
}

fn append(fake: &Fake, file: &str, line: &Value) {
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fake.data.join(file))
        .unwrap();
    writeln!(out, "{line}").unwrap();
}

/// `fake_wiki` to Markdown, or back: headings and bold, line by line.
fn convert(text: &str, to_markdown: bool) -> String {
    text.lines()
        .map(|line| {
            if to_markdown {
                let line = match line.strip_prefix("h1. ") {
                    Some(rest) => format!("# {rest}"),
                    None => line.to_string(),
                };
                swap_bold(&line, "*", "**")
            } else {
                let line = match line.strip_prefix("# ") {
                    Some(rest) => format!("h1. {rest}"),
                    None => line.to_string(),
                };
                swap_bold(&line, "**", "*")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn swap_bold(line: &str, from: &str, to: &str) -> String {
    let parts: Vec<&str> = line.split(from).collect();
    if parts.len() < 3 || parts.len().is_multiple_of(2) {
        return line.to_string();
    }
    parts.join(to)
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
                    let mut sql = json!({ "type": "string" });
                    if knob(&fake, "marked").is_some() {
                        sql["x-tracon-format"] = json!("fake_wiki");
                    }
                    json!({
                        "name": name,
                        "description": format!("The fake's {name}."),
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "sql": sql,
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
            if let Some(token) = params["_meta"]["tracon"]["preview_token"].as_str() {
                if token != version(&fake) {
                    let mut refused = echo.clone();
                    refused["refused"] = json!("stale");
                    append(&fake, "calls.jsonl", &refused);
                    return error(
                        &request,
                        json!({
                            "code": -32002,
                            "message": "the issue moved since the preview",
                            "data": { "stale": true },
                        }),
                    );
                }
            }
            append(&fake, "calls.jsonl", &echo);
            if tool == "node" {
                let args = &params["arguments"];
                let mut rpc_params = args.get("params").cloned().unwrap_or(json!({}));
                if args["on_behalf"] == true {
                    rpc_params["on_behalf"] = params["_meta"]["tracon"].clone();
                }
                let token = args["token"].as_str().unwrap_or(&fake.token).to_string();
                let out = match &fake.node_socket {
                    None => json!({ "status": 0, "body": "no --node-socket" }),
                    Some(path) => {
                        let http = reqwest::Client::builder()
                            .unix_socket(path.as_path())
                            .build()
                            .unwrap();
                        let response = http
                            .post("http://node/")
                            .bearer_auth(token)
                            .json(&json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "method": args["method"],
                                "params": rpc_params,
                            }))
                            .send()
                            .await
                            .unwrap();
                        let status = response.status().as_u16();
                        let body: Value = response.json().await.unwrap_or(Value::Null);
                        json!({ "status": status, "body": body })
                    }
                };
                return answer(
                    &request,
                    json!({ "content": [{ "type": "text", "text": out.to_string() }], "isError": false }),
                );
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
        Some("tracon/preview") if knob(&fake, "preview").is_some() => {
            let params = &request["params"];
            append(&fake, "previews.jsonl", &params["_meta"]["tracon"]);
            let tool = params["name"].as_str().unwrap_or_default();
            match knob(&fake, "preview").as_deref() {
                Some("refuse") => error(
                    &request,
                    json!({ "code": -32000, "message": "cannot preview" }),
                ),
                Some("unpreviewable") => answer(
                    &request,
                    json!({
                        "previewable": false,
                        "summary": format!("fake {tool}"),
                        "body": "cannot show this one",
                        "token": null,
                    }),
                ),
                _ => answer(
                    &request,
                    json!({
                        "previewable": true,
                        "summary": format!("fake {tool}"),
                        "body": format!(
                            "```diff\n- old\n+ {}\n```",
                            params["arguments"]["sql"].as_str().unwrap_or_default()
                        ),
                        "token": version(&fake),
                    }),
                ),
            }
        }
        Some("tracon/format/convert") => {
            let params = &request["params"];
            let text = params["text"].as_str().unwrap_or_default();
            match (params["format"].as_str(), params["to"].as_str()) {
                (Some("fake_wiki"), Some("markdown")) => {
                    answer(&request, json!({ "text": convert(text, true) }))
                }
                (Some("fake_wiki"), Some("fake_wiki")) => {
                    answer(&request, json!({ "text": convert(text, false) }))
                }
                _ => error(
                    &request,
                    json!({ "code": -32602, "message": "not a format this owns" }),
                ),
            }
        }
        _ => Json(json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": { "code": -32601, "message": "method not found" },
        }))
        .into_response(),
    }
}
