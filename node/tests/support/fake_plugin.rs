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
//! - `search.json`: when it exists, a call to `search` answers as
//!   jira-review's paged search does: the page `arguments.cursor` names
//!   (`""` for the first) as JSON text. A page that is a string is answered
//!   as an error with that text.
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
            if tool == "sleep" {
                let ms = params["arguments"]["ms"].as_u64().unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            if tool == "search" {
                if let Ok(pages) = std::fs::read_to_string(fake.data.join("search.json")) {
                    let pages: Value = serde_json::from_str(&pages).unwrap();
                    let cursor = params["arguments"]["cursor"].as_str().unwrap_or("");
                    let page = &pages[cursor];
                    let (text, failed) = match page.as_str() {
                        Some(error) => (error.to_string(), true),
                        None => (page.to_string(), false),
                    };
                    return answer(
                        &request,
                        json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": failed,
                        }),
                    );
                }
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
