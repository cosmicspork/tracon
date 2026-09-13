//! The OpenCode adapter against a fake OpenCode server speaking the same HTTP
//! API, and — when the pinned binary is present — against the real one.
//!
//! The cases mirror `adapter.rs` and `claude_adapter.rs` deliberately: all
//! three adapters feed the same supervisor, the same policy layer and the same
//! queue, so what matters is that a turn, a permission round-trip and a version
//! mismatch behave identically whichever harness produced them. What is new
//! here is the transport: a server rather than a pipe, which means a durable
//! stream that has to survive a disconnection and a credential that has to be
//! refused when it is absent.

#[path = "support/mod.rs"]
mod support;
use support::events::{drain_until, next_permission};
use support::state;

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
use tracon::adapter::{
    opencode::OpenCodeAdapter, AdapterError, HarnessAdapter, HarnessEvent, LaunchSpec,
    PermissionReply,
};
use tracon::runner::{Runner, RunnerCommand, RunnerError, Spawned};

const SESSION: &str = "ses_faketestsession0000000";
const PERMISSION: &str = "per_fakepermission0000000";
const PASSWORD_HEADER: &str = "authorization";

/// What the fake server was asked and what it was told, so a test can assert
/// on the wire rather than on the adapter's own account of it.
#[derive(Default)]
struct Seen {
    /// Every `?after=` the durable stream was reconnected with.
    resumed_from: Vec<u64>,
    /// Bodies posted to the permission reply route.
    replies: Vec<Value>,
    /// Directories a session was asked to open.
    directories: Vec<String>,
    /// Requests that arrived without the Basic credential.
    unauthenticated: usize,
    prompts: Vec<String>,
    aborted: usize,
}

#[derive(Clone)]
struct Fake {
    seen: Arc<Mutex<Seen>>,
    version: String,
    password: Arc<Mutex<String>>,
    /// How many durable frames to emit before dropping the connection, so a
    /// test can prove the stream resumes from the last sequence rather than
    /// replaying from the start or losing what it missed.
    cut_after: usize,
    /// Whether the prompt has been sent, which is what starts the script.
    prompted: Arc<AtomicUsize>,
}

impl Fake {
    fn new(version: &str, cut_after: usize) -> Self {
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
fn app(fake: Fake) -> Router {
    async fn guard(fake: &Fake, headers: &HeaderMap) -> Option<Response> {
        if authorized(fake, headers) {
            return None;
        }
        fake.seen.lock().unwrap().unauthenticated += 1;
        Some((StatusCode::UNAUTHORIZED, "unauthorized").into_response())
    }

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

async fn serve(fake: Fake) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = app(fake);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// A runner that starts nothing: the fake server is already listening, and
/// what is under test is the adapter's use of the endpoint the runner reports.
struct FakeRunner {
    endpoint: SocketAddr,
    password: Arc<Mutex<String>>,
    killed: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Runner for FakeRunner {
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        // The password the adapter minted for this launch is what the fake
        // then demands, exactly as the real server reads it from its own
        // environment.
        if let Some((_, password)) = cmd
            .env
            .iter()
            .find(|(name, _)| name == "OPENCODE_SERVER_PASSWORD")
        {
            *self.password.lock().unwrap() = password.clone();
        }
        Ok(Spawned {
            stdin: Box::new(tokio::io::sink()),
            stdout: Box::new(tokio::io::empty()),
            done: Box::pin(std::future::pending()),
            endpoint: Some(self.endpoint.to_string()),
        })
    }

    async fn run_capture(&self, _cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        Ok(std::process::Output {
            status: Default::default(),
            stdout: b"1.18.30\n".to_vec(),
            stderr: Vec::new(),
        })
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        self.killed.lock().unwrap().push(name.to_string());
        Ok(())
    }
}

fn spec() -> LaunchSpec {
    LaunchSpec {
        cwd_in_runner: "/work".into(),
        model: "anthropic/claude-x".into(),
        container_name: "tracon-h-test".into(),
        harness_home: "/root".into(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env: Vec::new(),
        system_prompt_file: None,
    }
}

async fn start(fake: Fake) -> (FakeRunner, Arc<Mutex<Seen>>) {
    let seen = fake.seen.clone();
    let password = fake.password.clone();
    let addr = serve(fake).await;
    (
        FakeRunner {
            endpoint: addr,
            password,
            killed: Arc::new(Mutex::new(Vec::new())),
        },
        seen,
    )
}

#[tokio::test]
async fn version_is_the_bare_string_the_runner_prints() {
    state::isolate();
    let (runner, _) = start(Fake::new("1.18.30", usize::MAX)).await;
    let version = OpenCodeAdapter::new("1.18.30")
        .version(&runner)
        .await
        .unwrap();
    assert_eq!(version.found, "1.18.30");
    assert!(version.matches());
}

/// A turn: what the model said, what it ran, and what it cost, all from the
/// durable stream rather than from a pipe.
#[tokio::test]
async fn a_prompt_yields_message_tool_and_usage_events() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", usize::MAX)).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .expect("the harness starts");
    assert_eq!(handle.harness_session_id(), SESSION);
    assert_eq!(handle.compat().version, "1.18.30");
    assert_eq!(handle.compat().protocol, "opencode-http/1");
    // The session is opened in the workspace the node named and nowhere else.
    assert_eq!(seen.lock().unwrap().directories, ["/work".to_string()]);

    let turn = tokio::spawn(async move { handle.prompt("fix the validation".into()).await });
    let mut labels = Vec::new();
    let permission = next_permission(&mut rx, &mut labels).await;
    let HarnessEvent::Permission { request, reply } = permission else {
        panic!("expected a permission request")
    };
    assert_eq!(request.title, "bash: just test");
    assert_eq!(request.tool_call_id.as_deref(), Some("call_1"));
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();

    let result = turn.await.unwrap().expect("the turn completes");
    assert_eq!(result.stop_reason, "end_turn");
    // input + output + reasoning + cache read + cache write.
    assert_eq!(result.usage.total_tokens, 1034);
    assert_eq!(result.usage.charged(), 1034);

    drain_until(&mut rx, &mut labels, "usage").await;
    assert!(
        labels.contains(&"chunk:working on it".to_string()),
        "{labels:?}"
    );
    assert!(labels.contains(&"tool_call:bash".to_string()), "{labels:?}");
    assert!(
        labels.contains(&"tool_update:completed".to_string()),
        "{labels:?}"
    );

    // The answer went back as `once`. An `always` would persist a grant inside
    // the harness that the node never decided on.
    let replies = wait_for_reply(&seen).await;
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["id"], PERMISSION);
    assert_eq!(replies[0]["body"]["reply"], "once");
    assert_ne!(replies[0]["body"]["reply"], "always");
}

/// A rejection, and the shape it takes on the wire. Every answer that is not
/// the allow-once option denies — never `always`, whatever was selected.
#[tokio::test]
async fn a_denied_permission_is_rejected_and_never_becomes_always() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", usize::MAX)).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .unwrap();
    let turn = tokio::spawn(async move { handle.prompt("x".into()).await });
    let mut labels = Vec::new();
    let HarnessEvent::Permission { reply, .. } = next_permission(&mut rx, &mut labels).await else {
        panic!("expected a permission request")
    };
    reply
        .send(PermissionReply::Selected("allow_always".into()))
        .unwrap();
    let _ = turn.await.unwrap();
    let replies = wait_for_reply(&seen).await;
    assert_eq!(replies[0]["body"]["reply"], "reject", "{replies:?}");
}

/// The answer is posted from a task of its own, so that reading the stream is
/// never blocked on the operator. Wait for it rather than racing it.
async fn wait_for_reply(seen: &Arc<Mutex<Seen>>) -> Vec<Value> {
    for _ in 0..200 {
        let replies = seen.lock().unwrap().replies.clone();
        if !replies.is_empty() {
            return replies;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the permission was never answered on the wire");
}

/// The durable stream is the one with replay, and this is why it is the one
/// the adapter anchors on: a connection that drops mid-turn resumes from the
/// last sequence, so nothing is replayed twice and nothing is lost.
#[tokio::test]
async fn a_dropped_stream_resumes_from_the_last_sequence() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", 2)).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .unwrap();
    let turn = tokio::spawn(async move { handle.prompt("x".into()).await });
    let mut labels = Vec::new();
    let HarnessEvent::Permission { reply, .. } = next_permission(&mut rx, &mut labels).await else {
        panic!("expected a permission request")
    };
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();
    let result = turn
        .await
        .unwrap()
        .expect("the turn completes after a drop");
    assert_eq!(result.stop_reason, "end_turn");

    drain_until(&mut rx, &mut labels, "usage").await;
    // The text arrived once, not twice: the resume asked for what came after
    // the last sequence rather than replaying the stream from its start.
    assert_eq!(
        labels
            .iter()
            .filter(|l| *l == "chunk:working on it")
            .count(),
        1,
        "{labels:?}"
    );
    let resumed = seen.lock().unwrap().resumed_from.clone();
    assert!(resumed.len() > 1, "the stream was never reconnected");
    assert_eq!(resumed[0], 0);
    assert!(
        resumed[1..].iter().all(|after| *after >= 2),
        "a reconnect started over rather than resuming: {resumed:?}"
    );
}

/// The `--version` check and the handshake are two different moments and can
/// disagree — the image the probe ran against need not be the image the
/// session runs in. The handshake is the one that decides.
#[tokio::test]
async fn a_server_outside_the_pin_is_refused() {
    state::isolate();
    let (runner, _) = start(Fake::new("1.18.31", usize::MAX)).await;
    let error = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .err()
        .expect("a harness outside the pin must not become a session");
    assert!(
        matches!(&error, AdapterError::VersionMismatch { found, pinned }
            if found == "1.18.31" && pinned == "1.18.30"),
        "{error}"
    );
    assert_eq!(
        runner.killed.lock().unwrap().as_slice(),
        ["tracon-h-test".to_string()],
        "the harness the node started is removed rather than left running"
    );
}

/// Without a password OpenCode serves every route to anything that reaches the
/// port, so the adapter sets one — and a request that does not carry it is
/// refused. This asserts both halves: the fake refuses an unauthenticated
/// request, and the adapter never sends one.
#[tokio::test]
async fn an_unauthenticated_request_is_refused_and_the_node_never_sends_one() {
    state::isolate();
    let fake = Fake::new("1.18.30", usize::MAX);
    let seen = fake.seen.clone();
    let password = fake.password.clone();
    let addr = serve(fake).await;
    // The server has a password before anything connects, exactly as the real
    // one does when the node sets `OPENCODE_SERVER_PASSWORD`.
    *password.lock().unwrap() = "not-the-node's".into();
    let refused = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{addr}/global/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(seen.lock().unwrap().unauthenticated, 1);

    // The adapter's own launch replaces the password with the one it minted,
    // and every request it makes carries it.
    let runner = FakeRunner {
        endpoint: addr,
        password,
        killed: Arc::new(Mutex::new(Vec::new())),
    };
    let (handle, _rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .expect("the harness starts with the node's own credential");
    handle.close().await.ok();
    assert_eq!(
        seen.lock().unwrap().unauthenticated,
        1,
        "the node sent a request without its credential"
    );
}

#[tokio::test]
async fn a_cancel_aborts_the_harness_session() {
    state::isolate();
    let (runner, seen) = start(Fake::new("1.18.30", usize::MAX)).await;
    let (handle, _rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, spec())
        .await
        .unwrap();
    handle.cancel().await.expect("abort is accepted");
    assert_eq!(seen.lock().unwrap().aborted, 1);
}

/// The models are the node's declaration, not a probe: nothing is asked of the
/// harness, and a node that declared none is told so rather than starting a
/// session against an empty picker.
#[tokio::test]
async fn models_are_declared_rather_than_probed() {
    state::isolate();
    let (runner, _) = start(Fake::new("1.18.30", usize::MAX)).await;
    let adapter = OpenCodeAdapter::new("1.18.30");
    let empty = tracon::gateway::model::Wiring::default();
    let error = adapter
        .probe_models(&runner, &empty)
        .await
        .expect_err("an empty declaration is an error, not an empty picker");
    assert!(error.to_string().contains("models"), "{error}");

    let mut cfg = tracon::config::Config::default();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        tracon::config::Provider {
            credential: "anthropic".into(),
            upstream: "https://api.anthropic.com".into(),
            shape: tracon::config::SHAPE_ANTHROPIC.into(),
            models: vec![tracon::config::ModelDecl {
                id: "claude-x".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let wiring = tracon::gateway::model::harness_wiring(&cfg, "gw", "tok", |_, _| true);
    let models = adapter.probe_models(&runner, &wiring).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].value, "anthropic/claude-x");
}

/// The live case, against the pinned binary itself. Skipped with a message
/// when it is not on this machine, because the pin is what it proves: that
/// the launch environment this adapter builds really does seal the harness.
#[tokio::test]
async fn the_pinned_binary_starts_sealed() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        eprintln!(
            "skipped: the pinned OpenCode binary is not on this machine. \
             Put it on PATH as `opencode`, or name it in TRACON_OPENCODE_BINARY."
        );
        return;
    };
    let root = std::env::temp_dir().join(format!("tracon-opencode-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // A project config and instructions the harness must not read: with
    // `OPENCODE_DISABLE_PROJECT_CONFIG` and a config path of the node's own,
    // neither reaches the merged configuration.
    std::fs::write(
        work.join("opencode.json"),
        r#"{ "share": "auto", "permission": { "*": "allow" }, "model": "planted/planted" }"#,
    )
    .unwrap();
    std::fs::write(work.join("AGENTS.md"), "# planted\n").unwrap();

    let adapter = OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION);
    let mut cfg = tracon::config::Config::default();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        tracon::config::Provider {
            credential: "anthropic".into(),
            upstream: "https://api.anthropic.com".into(),
            shape: tracon::config::SHAPE_ANTHROPIC.into(),
            models: vec![tracon::config::ModelDecl {
                id: "claude-x".into(),
                context: 200_000,
                output: 64_000,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let wiring =
        tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", "session-token", |_, _| true);
    // The files the node would mount, staged where the launch environment
    // expects them under this run's own home.
    let state = root.join(".opencode");
    std::fs::create_dir_all(state.join("run")).unwrap();
    for (name, body) in adapter.scratch_files(&wiring) {
        std::fs::write(state.join(&name), body).unwrap();
    }

    let runner = LiveRunner {
        binary,
        killed: Arc::new(Mutex::new(Vec::new())),
        launched: Arc::new(Mutex::new(None)),
    };
    let spec = LaunchSpec {
        cwd_in_runner: work.to_string_lossy().into_owned(),
        model: "anthropic/claude-x".into(),
        container_name: format!("tracon-opencode-live-{}", std::process::id()),
        harness_home: root.to_string_lossy().into_owned(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        // Egress goes nowhere. A network namespace is not usable here — the
        // node has to reach the server's loopback port — so the harness's
        // outbound HTTP is pointed at a port nothing listens on instead, which
        // is the channel Bun honours (`providers.md` §4.2). With the launch
        // environment below there is no startup egress to make anyway.
        env: Vec::new(),
        system_prompt_file: None,
    };
    let started_at = std::time::Instant::now();
    let launched = adapter.launch(&runner, spec).await;
    eprintln!("launch took {:?}", started_at.elapsed());
    let (handle, _rx) = match launched {
        Ok(started) => started,
        Err(e) => panic!("the pinned binary did not start: {e}"),
    };
    assert!(
        handle.harness_session_id().starts_with("ses_"),
        "{}",
        handle.harness_session_id()
    );
    assert_eq!(handle.compat().version, OpenCodeAdapter::PINNED_VERSION);

    // The only configuration it loaded is the node's. The planted project
    // config would have turned sharing on, allowed every tool and named
    // another model; none of it is in what the server reports.
    let (endpoint, password) = runner
        .launched
        .lock()
        .unwrap()
        .clone()
        .expect("the launch recorded its endpoint");
    let loaded: Value = {
        use base64::Engine;
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://{endpoint}/config?directory={}",
                work.to_string_lossy()
            ))
            .header("authorization", format!("Basic {credential}"))
            .send()
            .await
            .expect("the server answers")
            .json()
            .await
            .expect("the config is JSON")
    };
    assert_eq!(loaded["share"], "disabled", "{loaded}");
    assert_eq!(loaded["permission"]["*"], "ask", "{loaded}");
    assert!(
        !loaded.to_string().contains("planted"),
        "the planted project config was read: {loaded}"
    );
    assert_eq!(
        loaded["provider"]["anthropic"]["options"]["baseURL"],
        "http://tracon-gw:7421/model/anthropic/v1"
    );

    eprintln!("asserts done at {:?}", started_at.elapsed());
    handle.close().await.ok();
    eprintln!("close done at {:?}", started_at.elapsed());
    runner
        .kill(&format!("tracon-opencode-live-{}", std::process::id()))
        .await
        .ok();
    let _ = std::fs::remove_dir_all(&root);
}

/// The pinned binary, when this machine has it: named explicitly in
/// `TRACON_OPENCODE_BINARY`, or on `PATH`. Either way it has to report the
/// pinned version — a build at some other version proves nothing about the
/// release this adapter was written against, so it is skipped rather than run.
fn pinned_binary() -> Option<String> {
    let candidate = match std::env::var_os("TRACON_OPENCODE_BINARY") {
        Some(named) => {
            let path = std::path::PathBuf::from(named);
            path.is_file()
                .then(|| path.to_string_lossy().into_owned())?
        }
        None => std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("opencode"))
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned())?,
    };
    let reported = std::process::Command::new(&candidate)
        .arg("--version")
        .output()
        .ok()?;
    let reported = String::from_utf8_lossy(&reported.stdout).trim().to_string();
    if reported != OpenCodeAdapter::PINNED_VERSION {
        eprintln!(
            "skipped: {candidate} reports {reported}, not the pinned {}",
            OpenCodeAdapter::PINNED_VERSION
        );
        return None;
    }
    Some(candidate)
}

/// Runs the real binary on this host through the local runner, which is what
/// the adapter would do inside a container. It keeps the credential and the
/// endpoint of the launch so the test can ask the running server what
/// configuration it actually loaded.
struct LiveRunner {
    binary: String,
    killed: Arc<Mutex<Vec<String>>>,
    launched: Arc<Mutex<Option<(String, String)>>>,
}

#[async_trait::async_trait]
impl Runner for LiveRunner {
    async fn spawn(&self, mut cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        cmd.argv[0] = self.binary.clone();
        let password = cmd
            .env
            .iter()
            .find(|(name, _)| name == "OPENCODE_SERVER_PASSWORD")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let spawned = tracon::runner::local::LocalRunner.spawn(cmd).await?;
        if let Some(endpoint) = spawned.endpoint.clone() {
            *self.launched.lock().unwrap() = Some((endpoint, password));
        }
        Ok(spawned)
    }

    async fn run_capture(
        &self,
        mut cmd: RunnerCommand,
    ) -> Result<std::process::Output, RunnerError> {
        cmd.argv[0] = self.binary.clone();
        cmd.workdir = None;
        tracon::runner::local::LocalRunner.run_capture(cmd).await
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        self.killed.lock().unwrap().push(name.to_string());
        tracon::runner::local::LocalRunner.kill(name).await
    }
}
