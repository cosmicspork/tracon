//! A fake OpenCode server speaking the pinned release's HTTP API.
//!
//! Shared by the adapter tests, which drive it through the adapter, and the
//! gateway tests, which drive it through the operator router. It is the same
//! server in both: the credential it demands, the directory it records, and
//! the `reply` bodies it receives are what each side has to get right, and
//! asserting on one fake keeps the two from disagreeing about the wire.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub const SESSION: &str = "ses_faketestsession0000000";
pub const PERMISSION: &str = "per_fakepermission0000000";
const PASSWORD_HEADER: &str = "authorization";

/// One request as the fake saw it, so a gateway test can assert on what
/// reached the harness rather than on what the node says it sent.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    /// The `Authorization` header, which is how a test sees the credential
    /// being injected server-side.
    pub authorization: Option<String>,
    pub directory_header: Option<String>,
    /// The operator's own cookie, which must never be forwarded.
    pub cookie: Option<String>,
}

/// What the fake server was asked and what it was told, so a test can assert
/// on the wire rather than on the adapter's own account of it.
#[derive(Default)]
pub struct Seen {
    /// Every `?after=` the durable stream was reconnected with.
    pub resumed_from: Vec<u64>,
    /// Bodies posted to the permission reply route.
    pub replies: Vec<Value>,
    /// Directories a session was asked to open.
    pub directories: Vec<String>,
    /// Requests that arrived without the Basic credential.
    pub unauthenticated: usize,
    pub prompts: Vec<String>,
    pub aborted: usize,
    /// Every request that reached the fake, in order.
    pub requests: Vec<Recorded>,
}

#[derive(Clone)]
pub struct Fake {
    pub seen: Arc<Mutex<Seen>>,
    pub version: String,
    pub password: Arc<Mutex<String>>,
    /// How many durable frames to emit before dropping the connection, so a
    /// test can prove the stream resumes from the last sequence rather than
    /// replaying from the start or losing what it missed.
    pub cut_after: usize,
    /// Whether the prompt has been sent, which is what starts the script.
    pub prompted: Arc<AtomicUsize>,
}

impl Fake {
    pub fn new(version: &str, cut_after: usize) -> Self {
        Self {
            seen: Arc::new(Mutex::new(Seen::default())),
            version: version.to_string(),
            password: Arc::new(Mutex::new(String::new())),
            cut_after,
            prompted: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The scripted turn, as durable events with their aggregate sequences.
    fn script(&self) -> Vec<Value> {
        vec![
            durable(
                1,
                "session.next.prompt.admitted",
                json!({ "sessionID": SESSION }),
            ),
            durable(
                2,
                "session.next.text.ended",
                json!({
                    "sessionID": SESSION,
                    "assistantMessageID": "msg_1",
                    "textID": "txt_1",
                    "text": "working on it",
                }),
            ),
            durable(
                3,
                "session.next.tool.called",
                json!({
                    "sessionID": SESSION,
                    "assistantMessageID": "msg_1",
                    "callID": "call_1",
                    "tool": "bash",
                    "input": { "command": "just test" },
                    "provider": { "executed": false },
                }),
            ),
            durable(
                4,
                "session.next.tool.success",
                json!({
                    "sessionID": SESSION,
                    "assistantMessageID": "msg_1",
                    "callID": "call_1",
                    "structured": { "exit": 0 },
                    "content": [],
                    "provider": { "executed": false },
                }),
            ),
            durable(
                5,
                "session.next.step.ended",
                json!({
                    "sessionID": SESSION,
                    "assistantMessageID": "msg_1",
                    "finish": "stop",
                    "cost": 0,
                    "tokens": {
                        "input": 1000, "output": 20, "reasoning": 4,
                        "cache": { "read": 8, "write": 2 },
                    },
                }),
            ),
        ]
    }
}

fn durable(seq: u64, kind: &str, data: Value) -> Value {
    json!({
        "id": format!("evt_{seq}"),
        "type": kind,
        "durable": { "aggregateID": SESSION, "seq": seq, "version": 1 },
        "data": data,
    })
}

fn authorized(fake: &Fake, headers: &HeaderMap) -> bool {
    let password = fake.password.lock().unwrap().clone();
    if password.is_empty() {
        return true;
    }
    let expected = {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))
        )
    };
    headers
        .get(PASSWORD_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| value == expected)
}

/// Every route the adapter drives. A request without the credential is refused
/// exactly as the real server refuses one, and counted so a test can assert
/// that the node never sends an unauthenticated request in the first place.
pub fn app(fake: Fake) -> Router {
    async fn guard(fake: &Fake, headers: &HeaderMap) -> Option<Response> {
        if authorized(fake, headers) {
            return None;
        }
        fake.seen.lock().unwrap().unauthenticated += 1;
        Some((StatusCode::UNAUTHORIZED, "unauthorized").into_response())
    }

    let log = fake.seen.clone();
    Router::new()
        .route(
            "/global/health",
            get(|State(fake): State<Fake>, headers: HeaderMap| async move {
                if let Some(refused) = guard(&fake, &headers).await {
                    return refused;
                }
                Json(json!({ "healthy": true, "version": fake.version })).into_response()
            }),
        )
        .route(
            "/config/providers",
            get(|State(_fake): State<Fake>| async move {
                Json(json!({
                    "providers": [{
                        "id": "anthropic",
                        "models": { "claude-x": { "limit": { "context": 123_456 } } },
                    }],
                }))
            }),
        )
        .route(
            "/api/session",
            post(
                |State(fake): State<Fake>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    if let Some(refused) = guard(&fake, &headers).await {
                        return refused;
                    }
                    let directory = body["location"]["directory"].as_str().unwrap_or_default();
                    fake.seen
                        .lock()
                        .unwrap()
                        .directories
                        .push(directory.to_string());
                    Json(json!({
                        "data": {
                            "id": SESSION,
                            "projectID": "p",
                            "cost": 0,
                            "tokens": { "input": 0, "output": 0, "reasoning": 0,
                                        "cache": { "read": 0, "write": 0 } },
                            "time": { "created": 1, "updated": 1 },
                            "title": "t",
                            "location": { "directory": directory },
                        }
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/api/session/{session}/prompt",
            post(
                |State(fake): State<Fake>,
                 headers: HeaderMap,
                 Path(session): Path<String>,
                 Json(body): Json<Value>| async move {
                    if let Some(refused) = guard(&fake, &headers).await {
                        return refused;
                    }
                    fake.seen
                        .lock()
                        .unwrap()
                        .prompts
                        .push(body["prompt"]["text"].as_str().unwrap_or("").to_string());
                    fake.prompted.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "data": {
                            "admittedSeq": 1,
                            "id": "msg_1",
                            "sessionID": session,
                            "prompt": body["prompt"],
                            "delivery": "steer",
                            "timeCreated": 1,
                        }
                    }))
                    .into_response()
                },
            ),
        )
        .route("/api/session/{session}/event", get(durable_stream))
        .route("/api/event", get(server_stream))
        .route(
            "/api/session/{session}/permission",
            get(|State(_fake): State<Fake>| async move { Json(json!({ "data": [] })) }),
        )
        .route(
            "/api/session/{session}/permission/{request}/reply",
            post(
                |State(fake): State<Fake>,
                 Path((_session, request)): Path<(String, String)>,
                 Json(body): Json<Value>| async move {
                    let mut seen = fake.seen.lock().unwrap();
                    seen.replies.push(json!({ "id": request, "body": body }));
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/session/{session}/abort",
            post(|State(fake): State<Fake>| async move {
                fake.seen.lock().unwrap().aborted += 1;
                Json(json!(true))
            }),
        )
        .route(
            "/instance/dispose",
            post(|State(_fake): State<Fake>| async move { Json(json!(true)) }),
        )
        // A route the matrix classifies readable but the adapter never
        // drives: the gateway test needs somewhere real to send one.
        .fallback(|State(fake): State<Fake>, headers: HeaderMap| async move {
            if !authorized(&fake, &headers) {
                fake.seen.lock().unwrap().unauthenticated += 1;
                return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
            }
            Json(json!({ "ok": true })).into_response()
        })
        // Log every request before it is answered, so a test can assert on
        // what reached the harness rather than on what the node says it sent.
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let seen = log.clone();
                async move {
                    {
                        let headers = req.headers();
                        let header = |name: &str| {
                            headers
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string)
                        };
                        seen.lock().unwrap().requests.push(Recorded {
                            method: req.method().to_string(),
                            path: req.uri().path().to_string(),
                            query: req.uri().query().unwrap_or_default().to_string(),
                            authorization: header("authorization"),
                            directory_header: header("x-opencode-directory"),
                            cookie: header("cookie"),
                        });
                    }
                    next.run(req).await
                }
            },
        ))
        .with_state(fake)
}

/// The durable per-session stream. It replays from `after`, and — when the
/// fake was built to cut — closes the connection partway so the adapter has to
/// reconnect from the last sequence it saw.
async fn durable_stream(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(_session): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !authorized(&fake, &headers) {
        fake.seen.lock().unwrap().unauthenticated += 1;
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let after: u64 = query
        .get("after")
        .and_then(|a| a.parse().ok())
        .unwrap_or_default();
    fake.seen.lock().unwrap().resumed_from.push(after);
    let cut = if after == 0 {
        fake.cut_after
    } else {
        usize::MAX
    };
    let script = fake.script();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        // Nothing is published until a prompt is admitted, which is what makes
        // the reconnect deterministic rather than a race.
        while fake.prompted.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if tx.is_closed() {
                return;
            }
        }
        let pending: Vec<Value> = script
            .into_iter()
            .filter(|e| e["durable"]["seq"].as_u64().unwrap_or(0) > after)
            .collect();
        // Reaching the cut with events still unsent is the simulated
        // disconnection: drop the sender so the stream ends mid-turn.
        let cut_short = pending.len() > cut;
        for event in pending.into_iter().take(cut) {
            if tx
                .send(Ok(axum::body::Bytes::from(format!("data: {event}\n\n"))))
                .await
                .is_err()
            {
                return;
            }
        }
        if cut_short {
            return;
        }
        // The turn is over; hold the connection open as the real server does
        // rather than ending the stream.
        while tx
            .send(Ok(axum::body::Bytes::from(": heartbeat\n\n")))
            .await
            .is_ok()
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

/// The server-wide stream, which carries the permission asks and has no
/// replay of its own.
async fn server_stream(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if !authorized(&fake, &headers) {
        fake.seen.lock().unwrap().unauthenticated += 1;
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut asked = false;
        loop {
            if !asked && fake.prompted.load(Ordering::SeqCst) > 0 {
                asked = true;
                let event = json!({
                    "id": "evt_perm",
                    "type": "permission.v2.asked",
                    "data": {
                        "id": PERMISSION,
                        "sessionID": SESSION,
                        "action": "bash",
                        "resources": ["just test"],
                        "source": { "type": "tool", "messageID": "msg_1", "callID": "call_1" },
                    },
                });
                if tx
                    .send(Ok(axum::body::Bytes::from(format!("data: {event}\n\n"))))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if tx
                .send(Ok(axum::body::Bytes::from(": heartbeat\n\n")))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap()
}

pub async fn serve(fake: Fake) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = app(fake);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}
